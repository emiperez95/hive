//! `hive stage` / `hive freeze` / `hive close` — move a conversation on the board, or
//! park / end its window, usually from inside it (`/hive:stage`, `/hive:freeze`,
//! `/hive:close`).
//!
//! Freeze and close kill a window. Run from the conversation IN that window, an
//! immediate kill cuts its reply off and leaves the transcript ending on an unfinished
//! `hive freeze` call — which a thawed Claude reasonably retries, freezing itself again
//! (the trap `hive fork` meets too). So from inside they are **deferred**: a waiter on
//! the tmux server (`run-shell -b`, which outlives the window it kills) watches Claude's
//! own session record and acts once the turn has ended.

use anyhow::{anyhow, bail, Result};
use std::time::{Duration, Instant};

use crate::common::claude_sessions::{self, ClaudeSessionStatus};
use crate::common::frozen::{freeze_window, FreezeTarget};
use crate::common::registry::{Conversation, ConversationSidecar, Stage};
use crate::common::tmux::{exact_window, sh_quote};

/// How long a deferred freeze/close waits for the turn to end before giving up.
const TURN_TIMEOUT_SECS: u64 = 30 * 60;

/// A record that already reads "ended" when the waiter starts is only believed after
/// this long: the waiter is launched mid-turn, so the honest first reading is `busy`.
const STALE_GRACE: Duration = Duration::from_secs(5);

pub fn parse_stage(s: &str) -> Option<Stage> {
    match s.to_ascii_lowercase().replace(['_', ' '], "-").as_str() {
        "todo" => Some(Stage::Todo),
        "planning" | "plan" => Some(Stage::Planning),
        "in-progress" | "progress" | "wip" | "doing" => Some(Stage::InProgress),
        "blocked" => Some(Stage::Blocked),
        "done" => Some(Stage::Done),
        _ => None,
    }
}

/// Claude's main thread has finished its turn. `shell` counts — only background
/// shells remain, and freezing by hand would end those too. `waiting` does not: that
/// is a prompt open mid-turn.
pub fn turn_ended(status: Option<ClaudeSessionStatus>) -> bool {
    matches!(
        status,
        Some(ClaudeSessionStatus::Idle | ClaudeSessionStatus::Shell)
    )
}

/// The conversation to act on, and whether it is the one running this command.
struct Subject {
    conv: Conversation,
    mid_turn: bool,
}

fn subject(id: Option<String>) -> Result<Subject> {
    let running = crate::cli::fork::running_conversation_id();
    let wanted = id.or_else(|| running.clone()).ok_or_else(|| {
        anyhow!("not inside a Claude conversation — pass --id <uuid> (or use the board / TUI)")
    })?;
    let reg = crate::common::conversations::gather_conversations();
    // Exact id, else an unambiguous prefix (the 8-char ids the board and TUI show).
    let conv = match reg.conversations.get(wanted.as_str()) {
        Some(c) => c.clone(),
        None => {
            let mut hits = reg
                .conversations
                .values()
                .filter(|c| c.id.as_str().starts_with(&wanted));
            match (hits.next(), hits.next()) {
                (Some(c), None) => c.clone(),
                (Some(_), Some(_)) => bail!("'{wanted}' matches several conversations"),
                _ => bail!("no conversation '{wanted}' that hive can see"),
            }
        }
    };
    let mid_turn = running.as_deref() == Some(conv.id.as_str());
    Ok(Subject { conv, mid_turn })
}

pub fn run_stage(
    stage: &str,
    reason: Option<String>,
    freeze: bool,
    id: Option<String>,
    now: bool,
) -> Result<()> {
    let stage = parse_stage(stage).ok_or_else(|| {
        anyhow!("unknown stage '{stage}' — todo, planning, in-progress, blocked or done")
    })?;
    let s = subject(id)?;
    let note = reason.filter(|r| !r.trim().is_empty());
    let id = s.conv.id.as_str().to_string();
    ConversationSidecar::edit(&id, |o| {
        o.stage = Some(stage);
        o.stage_at = Some(chrono::Utc::now().to_rfc3339());
        o.stage_note = note.clone().filter(|_| stage == Stage::Blocked);
    })?;
    let label = serde_json::to_value(stage)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
        .replace('_', " ");
    match (&note, stage) {
        (Some(n), Stage::Blocked) => println!("{} → {label}: {n}", short(&id)),
        (Some(_), _) => println!(
            "{} → {label} (reason ignored — kept only for blocked)",
            short(&id)
        ),
        _ => println!("{} → {label}", short(&id)),
    }
    if freeze {
        let freeze_note = match (stage, note) {
            (Stage::Blocked, Some(n)) => format!("Blocked: {n}"),
            (Stage::Blocked, None) => "Blocked".to_string(),
            (_, n) => n.unwrap_or_default(),
        };
        freeze_subject(&s, &freeze_note, now)?;
    }
    Ok(())
}

pub fn run_freeze(note: Option<String>, id: Option<String>, now: bool) -> Result<()> {
    let s = subject(id)?;
    freeze_subject(&s, note.as_deref().unwrap_or(""), now)
}

pub fn run_close(id: Option<String>, now: bool) -> Result<()> {
    let s = subject(id)?;
    let p = placement(&s.conv)?;
    if s.mid_turn && !now {
        defer(&["close", "--now", "--id", s.conv.id.as_str()])?;
        println!(
            "Will close this window ('{}' window {}) when this turn ends.",
            p.session_name, p.window_index
        );
        println!("  The transcript stays on disk — resumable from the TUI or the board.");
        println!("  End your reply now; anything after it is cut off.");
        return Ok(());
    }
    let target = exact_window(&p.session_name, &p.window_index);
    let ok = std::process::Command::new("tmux")
        .args(["kill-window", "-t", &target])
        .output()
        .is_ok_and(|o| o.status.success());
    if !ok {
        bail!("failed to close window '{target}'");
    }
    println!("Closed {}", short(s.conv.id.as_str()));
    Ok(())
}

fn freeze_subject(s: &Subject, note: &str, now: bool) -> Result<()> {
    let p = placement(&s.conv)?;
    let id = s.conv.id.as_str();
    if s.mid_turn && !now {
        let mut args = vec!["freeze", "--now", "--id", id];
        if !note.is_empty() {
            args.extend(["--", note]);
        }
        defer(&args)?;
        println!(
            "Will freeze this window ('{}' window {}) when this turn ends.",
            p.session_name, p.window_index
        );
        println!("  Thaw it from the board, the TUI's 💤 list, or `R`.");
        println!("  End your reply now; anything after it is cut off.");
        return Ok(());
    }
    let target = FreezeTarget {
        session_name: p.session_name.clone(),
        window_index: p.window_index.clone(),
        window_name: p.window_name.clone(),
        cwd: s.conv.cwd.clone(),
        claude_session_id: Some(id.to_string()),
    };
    freeze_window(&target, note)?;
    println!("Froze {} — thaw it from the board or the TUI", short(id));
    Ok(())
}

fn placement(c: &Conversation) -> Result<&crate::common::registry::TmuxPlacement> {
    c.placement.as_ref().ok_or_else(|| {
        anyhow!(
            "{} isn't running in a tmux window here",
            short(c.id.as_str())
        )
    })
}

/// Hand `hive <args>` to a waiter on the tmux server that runs it once this turn ends.
/// `run-shell -b` runs in the server, not this pane, so it survives the window it
/// kills. Output goes to `after-turn.log` — a failure there has no window to land in.
fn defer(args: &[&str]) -> Result<()> {
    if std::env::var_os("TMUX").is_none() {
        bail!("not inside tmux — nothing to defer to; pass --now");
    }
    let pid: u32 = std::env::var("CLAUDE_PID")
        .ok()
        .and_then(|p| p.parse().ok())
        .ok_or_else(|| {
            anyhow!("$CLAUDE_PID is unset, so the turn's end can't be watched — pass --now")
        })?;
    let exe = std::env::current_exe()?;
    let hive = sh_quote(&exe.to_string_lossy());
    let rest: Vec<String> = args.iter().map(|a| sh_quote(a)).collect();
    let log = crate::common::persistence::cache_dir()
        .map(|d| d.join("after-turn.log"))
        .ok_or_else(|| anyhow!("no cache dir"))?;
    let cmd = deferred_command(
        &hive,
        pid,
        &rest.join(" "),
        &sh_quote(&log.to_string_lossy()),
    );
    let ok = std::process::Command::new("tmux")
        .args(["run-shell", "-b", &cmd])
        .status()
        .is_ok_and(|s| s.success());
    if !ok {
        bail!("tmux refused the deferred command — pass --now to act immediately");
    }
    Ok(())
}

/// The shell line the waiter runs. Pure, so its shape is testable.
fn deferred_command(hive: &str, pid: u32, rest: &str, log: &str) -> String {
    format!("{{ date; {hive} wait-turn {pid} && {hive} {rest}; }} >> {log} 2>&1",)
}

/// `hive wait-turn <pid>` (hidden): block until Claude `pid` ends its turn. Exits
/// non-zero if the process goes away or the timeout passes — the caller's `&&` then
/// does nothing, which is the safe direction.
pub fn run_wait_turn(pid: u32, timeout_secs: Option<u64>) -> Result<()> {
    let timeout = Duration::from_secs(timeout_secs.unwrap_or(TURN_TIMEOUT_SECS));
    let start = Instant::now();
    let mut seen_busy = false;
    loop {
        let Some(rec) = claude_sessions::for_pid(pid) else {
            bail!("claude {pid} is gone — nothing to wait for");
        };
        if !turn_ended(rec.status) {
            seen_busy = true;
        } else if seen_busy || start.elapsed() >= STALE_GRACE {
            println!(
                "claude {pid}: turn ended after {:.1}s",
                start.elapsed().as_secs_f32()
            );
            return Ok(());
        }
        if start.elapsed() >= timeout {
            bail!(
                "claude {pid}: turn still running after {}s — giving up",
                timeout.as_secs()
            );
        }
        std::thread::sleep(Duration::from_millis(400));
    }
}

fn short(id: &str) -> &str {
    &id[..id.len().min(8)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stages_parse_with_the_spellings_people_type() {
        assert_eq!(parse_stage("todo"), Some(Stage::Todo));
        assert_eq!(parse_stage("Plan"), Some(Stage::Planning));
        for s in ["in-progress", "in_progress", "in progress", "wip"] {
            assert_eq!(parse_stage(s), Some(Stage::InProgress), "{s}");
        }
        assert_eq!(parse_stage("blocked"), Some(Stage::Blocked));
        assert_eq!(parse_stage("DONE"), Some(Stage::Done));
        assert_eq!(parse_stage("unsorted"), None);
    }

    #[test]
    fn a_turn_ends_at_idle_or_shell_never_at_a_prompt() {
        assert!(turn_ended(Some(ClaudeSessionStatus::Idle)));
        assert!(turn_ended(Some(ClaudeSessionStatus::Shell)));
        // A permission prompt or question is mid-turn: killing then loses the answer.
        assert!(!turn_ended(Some(ClaudeSessionStatus::Waiting)));
        assert!(!turn_ended(Some(ClaudeSessionStatus::Busy)));
        // A status we can't read is not "ended".
        assert!(!turn_ended(None));
    }

    #[test]
    fn the_deferred_action_only_runs_if_the_wait_succeeds() {
        let cmd = deferred_command("'/bin/hive'", 42, "'freeze' '--now'", "'/tmp/l'");
        // `&&`: a timeout or a vanished process must not go on to kill a window.
        assert!(cmd.contains("'/bin/hive' wait-turn 42 && '/bin/hive' 'freeze' '--now'"));
        assert!(cmd.ends_with(">> '/tmp/l' 2>&1"));
    }
}
