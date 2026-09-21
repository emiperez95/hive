//! Claude's own live-session registry — `~/.claude*/sessions/<pid>.json`.
//!
//! Every running Claude process writes one of these and rewrites it whenever its
//! status changes. It is the only **first-party** statement of what a conversation
//! is doing: everything else hive has is inference — a hook that fires from inside
//! Claude (and so reports presence, not state, and is pruned after 10 min), or a
//! transcript tail read from the outside.
//!
//! What it gives us, per live conversation:
//!
//! - `sessionId` — the conversation UUID, i.e. hive's own key. No resolution needed.
//! - `status` — `busy | shell | idle | waiting`, decided by Claude's own task table.
//! - `statusUpdatedAt` — a real status-*transition* timestamp. Nothing else in hive
//!   records one (`last_activity` is "last hook event fired"; see `StateAges`).
//! - `tmux` — `session:@window.%pane`, exactly the link `instances.rs` reconstructs
//!   from hook ids, `--resume` argv and a recency guess. Not consumed yet.
//!
//! Two properties shape how it must be read:
//!
//! - **Profile-scoped, like everything else Claude writes.** Sessions live under
//!   whichever `CLAUDE_CONFIG_DIR` launched them, so all of `~/.claude*/sessions/`
//!   must be globbed — the same rule `jsonl::claude_slug_dirs` already follows for
//!   transcripts. Measured here: 5 of 11 live sessions were under `~/.claude`, the
//!   rest under `~/.claude-work` and `~/.claude-local`. (`claude agents --json` only
//!   reports the current profile, which is why hive reads the files directly — and
//!   they carry `tmux` and `statusUpdatedAt`, which that command's output omits.)
//!
//! - **There is no heartbeat.** The file is rewritten only on a status change —
//!   measured: `updatedAt == statusUpdatedAt` on every live session — so a timestamp
//!   four days old is indistinguishable by age alone from a stale file left by a
//!   dead process. Liveness is the reader's job, and it cannot be answered from the
//!   file's contents. [`index_confirmed`] therefore accepts a record only when its
//!   pid is in the process tree hive already resolved for that conversation, which
//!   also rules out pid reuse for free.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Deserializer};

/// What Claude says the session is doing. The vocabulary is Claude's own
/// (`busy | shell | idle | waiting`); anything else parses as `None` so a value
/// added in a future release degrades to "we don't know" rather than a wrong guess.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClaudeSessionStatus {
    /// The main thread is executing a turn.
    Busy,
    /// The main thread is **idle** but background shells are still running.
    ///
    /// This is the state hive had to infer by pairing `<task-notification>` ids
    /// against background launches in the transcript — an inference that can never
    /// clear for a backgrounded *service* (a dev server exits only when killed, so
    /// it never notifies). Claude reads it straight off its live task table.
    Shell,
    /// Nothing running, nothing pending.
    Idle,
    /// A human decision is pending — Claude's own Agent View labels this bucket
    /// "Sessions that have a question or need your decision".
    Waiting,
}

/// One live Claude process, as it describes itself.
#[derive(Debug, Clone, Deserialize)]
pub struct ClaudeSession {
    pub pid: u32,
    /// The conversation UUID — the `<uuid>.jsonl` basename, hive's registry key.
    #[serde(rename = "sessionId")]
    pub session_id: Option<String>,
    /// `None` when the value is one this hive doesn't know (see [`ClaudeSessionStatus`]).
    #[serde(default, deserialize_with = "lenient_status")]
    pub status: Option<ClaudeSessionStatus>,
    /// Epoch millis of the last status *transition*.
    #[serde(rename = "statusUpdatedAt")]
    pub status_updated_at: Option<i64>,
    /// What the session is waiting on, when Claude supplies it.
    #[serde(rename = "waitingFor")]
    pub waiting_for: Option<String>,
}
// The record carries more than this — `cwd`, `tmux`, `version`, `name`,
// `messagingSocketPath`, `formerNames`. They're deliberately not fields: an unread
// field is dead code, and the two worth having (`tmux` for window resolution,
// `statusUpdatedAt`'s sibling `name`) belong to changes that haven't been made yet.
// Serde ignores unknown keys, so adding one back is a one-line change.

/// Parse `status` without failing on a value Claude added after this hive was
/// built. `#[serde(rename_all)]` alone rejects an unknown variant outright, which
/// would drop the whole record — including the fields we *can* read — the first
/// time the vocabulary grows.
fn lenient_status<'de, D>(d: D) -> Result<Option<ClaudeSessionStatus>, D::Error>
where
    D: Deserializer<'de>,
{
    let raw = Option::<String>::deserialize(d)?;
    Ok(match raw.as_deref() {
        Some("busy") => Some(ClaudeSessionStatus::Busy),
        Some("shell") => Some(ClaudeSessionStatus::Shell),
        Some("idle") => Some(ClaudeSessionStatus::Idle),
        Some("waiting") => Some(ClaudeSessionStatus::Waiting),
        _ => None,
    })
}

/// Every `~/.claude*/sessions` directory, across all auth profiles.
pub fn session_dirs() -> Vec<PathBuf> {
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    let mut dirs_out = Vec::new();
    if let Ok(entries) = fs::read_dir(&home) {
        for entry in entries.filter_map(|e| e.ok()) {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name == ".claude" || name.starts_with(".claude-") {
                let sessions = entry.path().join("sessions");
                if sessions.is_dir() {
                    dirs_out.push(sessions);
                }
            }
        }
    }
    dirs_out
}

/// Read every session record in `dirs`. Path-injectable so the parse is testable
/// without a real home directory. Unreadable or unparseable files are skipped —
/// a record being written as we read it must not take out the whole gather.
pub fn load_from(dirs: &[PathBuf]) -> Vec<ClaudeSession> {
    let mut out = Vec::new();
    for dir in dirs {
        let Ok(entries) = fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Ok(text) = fs::read_to_string(&path) else {
                continue;
            };
            if let Ok(s) = serde_json::from_str::<ClaudeSession>(&text) {
                out.push(s);
            }
        }
    }
    out
}

/// Read every session record across all auth profiles.
pub fn load_all() -> Vec<ClaudeSession> {
    load_from(&session_dirs())
}

/// Index the records hive can **confirm** are live, by conversation id.
///
/// `id_pids` is the gather's own map of conversation id → the pids of its pane's
/// process tree. A record is kept only when its `sessionId` names a conversation
/// hive already sees running *and* its `pid` is in that conversation's tree.
///
/// That single check covers both failure modes of a registry with no heartbeat: a
/// file left behind by a process that died (its pid is in nobody's tree) and a file
/// whose pid has since been recycled by an unrelated process (the pid is live, but
/// not under this conversation). It costs no syscalls — the gather has already
/// built the process tree.
///
/// A record carrying an unrecognised status is dropped here rather than kept as a
/// half-answer: callers use the presence of an entry to mean "Claude told us", and
/// an entry that cannot say what the status is would silently suppress the
/// transcript fallback.
pub fn index_confirmed(
    sessions: Vec<ClaudeSession>,
    id_pids: &HashMap<String, Vec<u32>>,
) -> HashMap<String, ClaudeSession> {
    let mut out: HashMap<String, ClaudeSession> = HashMap::new();
    for s in sessions {
        let Some(id) = s.session_id.clone() else {
            continue;
        };
        if s.status.is_none() {
            continue;
        }
        let Some(pids) = id_pids.get(&id) else {
            continue;
        };
        if !pids.contains(&s.pid) {
            continue;
        }
        // Two confirmed records for one conversation shouldn't happen (a pid hosts
        // one session), but if it does, the most recent transition wins.
        let newer = out
            .get(&id)
            .is_none_or(|e| s.status_updated_at >= e.status_updated_at);
        if newer {
            out.insert(id, s);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(pid: u32, id: &str, status: &str) -> ClaudeSession {
        serde_json::from_str(&format!(
            r#"{{"pid":{pid},"sessionId":"{id}","cwd":"/p","status":"{status}","statusUpdatedAt":1}}"#
        ))
        .unwrap()
    }

    fn pids(pairs: &[(&str, &[u32])]) -> HashMap<String, Vec<u32>> {
        pairs
            .iter()
            .map(|(id, p)| (id.to_string(), p.to_vec()))
            .collect()
    }

    #[test]
    fn parses_the_real_record_shape() {
        // Verbatim from a live `~/.claude/sessions/<pid>.json`, trimmed of fields we
        // don't read. Unknown keys must not fail the parse — the schema is Claude's.
        let raw = r#"{"pid":86993,"sessionId":"19c03217-33eb-4154-911c-24f801d22c24",
            "cwd":"/Users/x/hive","startedAt":1789684947778,"procStart":"Thu Sep 17 22:42:26 2026",
            "version":"2.1.275","peerProtocol":1,"peerFeatures":["notify_idle"],
            "kind":"interactive","entrypoint":"cli","pidDomain":"darwin",
            "tmux":"🐝 hive:@35.%40","messagingSocketPath":"/tmp/cc-socks/86993.sock",
            "name":"sidebar-attention-ordering","nameSource":"auto","nameSince":1789990054450,
            "status":"busy","updatedAt":1789993326778,"statusUpdatedAt":1789993326778,
            "formerNames":[{"name":"old","until":1789990054450}]}"#;
        let s: ClaudeSession = serde_json::from_str(raw).unwrap();
        assert_eq!(s.pid, 86993);
        assert_eq!(s.status, Some(ClaudeSessionStatus::Busy));
        assert_eq!(s.status_updated_at, Some(1789993326778));
        assert_eq!(
            s.session_id.as_deref(),
            Some("19c03217-33eb-4154-911c-24f801d22c24")
        );
    }

    #[test]
    fn every_status_in_claudes_vocabulary_parses() {
        for (raw, want) in [
            ("busy", ClaudeSessionStatus::Busy),
            ("shell", ClaudeSessionStatus::Shell),
            ("idle", ClaudeSessionStatus::Idle),
            ("waiting", ClaudeSessionStatus::Waiting),
        ] {
            assert_eq!(rec(1, "a", raw).status, Some(want), "{raw}");
        }
    }

    #[test]
    fn an_unknown_status_parses_as_none_rather_than_failing() {
        // A value added in a future Claude release must degrade to "we don't know",
        // not take the whole record (or the whole directory) out of the gather.
        let s: ClaudeSession =
            serde_json::from_str(r#"{"pid":1,"sessionId":"a","status":"compacting"}"#).unwrap();
        assert_eq!(s.pid, 1);
        assert!(s.status.is_none());
    }

    #[test]
    fn keeps_a_record_whose_pid_is_in_its_conversations_tree() {
        let idx = index_confirmed(vec![rec(42, "a", "waiting")], &pids(&[("a", &[7, 42])]));
        assert_eq!(idx["a"].status, Some(ClaudeSessionStatus::Waiting));
    }

    #[test]
    fn drops_a_record_left_behind_by_a_dead_process() {
        // The file survives the process. Nothing else in the record says so — there
        // is no heartbeat — so the pid not being in any live tree is the only tell.
        let idx = index_confirmed(vec![rec(42, "a", "busy")], &pids(&[("b", &[7])]));
        assert!(idx.is_empty());
    }

    #[test]
    fn drops_a_record_whose_pid_was_recycled_by_another_process() {
        // Conversation "a" is live, but on a different pid than the stale file claims.
        // Trusting the sessionId alone would import a dead session's last status.
        let idx = index_confirmed(vec![rec(42, "a", "busy")], &pids(&[("a", &[7, 8])]));
        assert!(idx.is_empty());
    }

    #[test]
    fn drops_a_record_with_a_status_we_cannot_read() {
        // Callers read "entry present" as "Claude told us", which suppresses the
        // transcript fallback — so an entry that can't say what the status is would
        // be worse than no entry at all.
        let s: ClaudeSession =
            serde_json::from_str(r#"{"pid":42,"sessionId":"a","status":"compacting"}"#).unwrap();
        assert!(index_confirmed(vec![s], &pids(&[("a", &[42])])).is_empty());
    }

    #[test]
    fn drops_a_record_with_no_session_id() {
        let s: ClaudeSession = serde_json::from_str(r#"{"pid":42,"status":"busy"}"#).unwrap();
        assert!(index_confirmed(vec![s], &pids(&[("a", &[42])])).is_empty());
    }

    #[test]
    fn most_recent_transition_wins_for_a_duplicated_conversation() {
        let mut older = rec(1, "a", "idle");
        older.status_updated_at = Some(100);
        let mut newer = rec(2, "a", "busy");
        newer.status_updated_at = Some(200);
        let idx = index_confirmed(vec![newer, older], &pids(&[("a", &[1, 2])]));
        assert_eq!(idx["a"].status, Some(ClaudeSessionStatus::Busy));
    }

    #[test]
    fn load_from_skips_unparseable_and_non_json_files() {
        let dir = std::env::temp_dir().join(format!("hive-cs-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("1.json"),
            r#"{"pid":1,"sessionId":"a","status":"idle"}"#,
        )
        .unwrap();
        // A record caught mid-write, and a sibling that isn't a record at all.
        fs::write(dir.join("2.json"), r#"{"pid":2,"sessi"#).unwrap();
        fs::write(dir.join("3.key"), "not json").unwrap();

        let loaded = load_from(std::slice::from_ref(&dir));
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].pid, 1);
        let _ = fs::remove_dir_all(&dir);
    }
}
