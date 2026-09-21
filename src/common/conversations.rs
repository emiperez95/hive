//! Shared conversation gathering — builds the read-only [`ConversationRegistry`]
//! from live state (hook status + a disk scan + live tmux placements + the overlay
//! sidecar), with parents resolved, live status recovered, and the Closed set
//! bounded.
//!
//! Homed in `common/` so BOTH the conversations TUI (`cli::conversations`) and the
//! web dashboard (`serve`) can call it without either depending on the other. The
//! TUI used to own this; it moved here for the web port.

use std::collections::{HashMap, HashSet};
use std::process::Command;

use anyhow::{anyhow, Result};
use sysinfo::System;

use crate::common::activity::{self, WindowSeen};
use crate::common::claude_sessions;
use crate::common::frozen::FrozenState;
use crate::common::instances;
use crate::common::jsonl;
use crate::common::ports::get_listening_ports_for_pids;
use crate::common::process::{build_cmdline_map, get_process_info, parse_resume_id};
use crate::common::projects::{activate_project, ensure_tmux_session, ProjectRegistry};
use crate::common::registry::{
    self, Conversation, ConversationRegistry, ConversationSidecar, ConversationStatus, Lifecycle,
    TmuxPlacement,
};
use crate::common::types::ClaudeStatus;
use crate::common::worktree::WorktreeState;
use crate::ipc::messages::{HookState, SessionStatus};

/// Build the conversation registry from live state (READ-ONLY): hook status +
/// disk existence + live placements + overlay, with parents resolved, live
/// status recovered from transcripts, and the Closed set bounded.
pub fn gather_conversations() -> ConversationRegistry {
    gather_conversations_inner(None)
}

/// Like [`gather_conversations`], but also samples live CPU/mem/ports into each
/// live conversation's runtime fields. `sys` must be kept alive across calls so
/// `cpu_percent` is a real delta (the TUI keeps its `System` alive too).
pub fn gather_conversations_stats(sys: &mut System) -> ConversationRegistry {
    gather_conversations_inner(Some(sys))
}

fn gather_conversations_inner(stats: Option<&mut System>) -> ConversationRegistry {
    let hook = HookState::load();
    // Cached scan: unchanged transcripts (by mtime) skip the head+tail re-parse,
    // so repeat refreshes and popup re-opens stay snappy.
    let disk = jsonl::scan_all_disk_conversations_cached();
    let disk_ids: Vec<String> = disk.iter().map(|d| d.id.clone()).collect();
    let sidecar = ConversationSidecar::load();

    // Live placements: every currently-running Claude instance we can tie to a
    // conversation id becomes the SOLE Live discriminator for that id. `id_pids`
    // keeps each live id's process tree for the optional CPU/mem/ports sample.
    let mut live_placements: HashMap<String, TmuxPlacement> = HashMap::new();
    let mut id_pids: HashMap<String, Vec<u32>> = HashMap::new();
    // Full process argv (one `ps`) so we can read `claude --resume <id>` — the
    // per-window conversation id that survives hook-state pruning.
    let cmdlines = build_cmdline_map();
    let instances = instances::detect_all_instances();
    // `claimed` prevents two windows resolving to the same transcript.
    let mut claimed: HashSet<String> = HashSet::new();

    // Pass 1 — EXACT ids: the hook-resolved id, else `--resume <id>` from the
    // process argv (validated: the transcript must exist). Both are per-window
    // exact, so they always beat the recency guess below.
    let mut pending: Vec<(Option<String>, instances::ClaudeInstance)> = Vec::new();
    for inst in instances {
        let sid = inst.session_id.clone().or_else(|| {
            inst.pids
                .iter()
                .filter_map(|pid| cmdlines.get(pid))
                .find_map(|cmd| parse_resume_id(cmd))
                .filter(|id| jsonl::find_jsonl_by_session_id(&inst.cwd, id).is_some())
        });
        if let Some(s) = &sid {
            claimed.insert(s.clone());
        }
        pending.push((sid, inst));
    }

    // Pass 2 — FILL unresolved windows (no hook, plain `claude` with no id in argv,
    // e.g. state.json pruned). A single-window cwd takes its newest transcript; a
    // shared cwd takes the newest transcript not already claimed by a sibling window
    // (N live windows ↔ N most-recent transcripts). Without this, live windows
    // absent from state.json are invisible here though classic `prefix + s` shows them.
    for (sid, inst) in &mut pending {
        if sid.is_some() {
            continue;
        }
        let candidate = if inst.cwd_shared {
            jsonl::list_jsonls_for_cwd_by_recency(&inst.cwd)
                .into_iter()
                .find(|id| !claimed.contains(id))
        } else {
            jsonl::find_latest_jsonl_for_cwd(&inst.cwd)
                .and_then(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned()))
        };
        if let Some(c) = candidate {
            claimed.insert(c.clone());
            *sid = Some(c);
        }
    }

    for (sid, inst) in pending {
        if let Some(sid) = sid {
            id_pids.insert(sid.clone(), inst.pids.clone());
            live_placements.insert(
                sid,
                TmuxPlacement {
                    session_name: inst.session_name,
                    window_index: inst.window_index,
                    window_name: inst.window_name,
                    pane_id: Some(inst.pane_id),
                },
            );
        }
    }

    let mut reg = ConversationRegistry::from_shadow(&hook, &disk_ids, &live_placements, &sidecar);

    // Enrich every conversation from its transcript: last-activity and title where the hook
    // side has none, and — for ALL of them — the home cwd. The transcript's launch dir beats
    // the hook's cwd (see `registry::home_cwd`): a hook reports where the process is right
    // now, which a background Workflow moves into an agent worktree under the MAIN checkout,
    // and parent resolution below would follow it there.
    let disk_map: HashMap<&str, &jsonl::DiskConversation> =
        disk.iter().map(|d| (d.id.as_str(), d)).collect();
    for (id, c) in reg.conversations.iter_mut() {
        if let Some(d) = disk_map.get(id.as_str()) {
            c.cwd = registry::home_cwd(&c.cwd, d.cwd.as_deref()).to_string();
            if c.last_activity.is_none() {
                c.last_activity = d.last_activity.clone();
            }
            if c.title.is_none() {
                c.title = d.title.clone();
            }
            if c.auth_config_dir.is_none() {
                c.auth_config_dir = d.config_dir.clone();
            }
        }
    }

    // Claude's own account of what each live conversation is doing. Confirmed
    // against `id_pids` so a record left behind by a dead process — the files have
    // no heartbeat — can't be mistaken for a live one. See `claude_sessions`.
    let first_party = claude_sessions::index_confirmed(claude_sessions::load_all(), &id_pids);

    // Recover live status for conversations whose hook entry was pruned (state.json
    // drops entries after ~10 min idle), and overlay a RunningWorkflow badge when
    // the main thread is idle but a background task is in flight. Without this a
    // long-idle-but-running conversation reads as blank/unknown. Runs for LIVE
    // conversations only (a Closed one is not executing). The conversation id is
    // the transcript basename, so we pass it as the exact `session_id`.
    for c in reg.conversations.values_mut() {
        if !c.lifecycle.is_actionable_here() {
            continue;
        }
        let sid = c.id.as_str().to_string();
        if c.status.is_none() {
            if let Some(js) = jsonl::get_claude_status_from_jsonl_for(&c.cwd, Some(&sid)) {
                let status = convert_claude_to_session_status(&js.status);
                c.status = Some(ConversationStatus {
                    needs_attention: status_needs_attention(&status),
                    status,
                });
                if c.last_activity.is_none() {
                    c.last_activity = js.timestamp.map(|t| t.to_rfc3339());
                }
            }
        }

        // Claude's word beats ours wherever it has one: the hook and the transcript
        // are both outside views, and both are stale in exactly the situations that
        // matter (a pruned entry, a conversation idle long enough to have scrolled
        // its last decision out of the tail).
        if let Some(fp) = first_party.get(&sid) {
            let inferred = c.status.take().map(|s| s.status);
            let status = reconcile_with_claude(fp, inferred, || {
                jsonl::background_running_summary(&c.cwd, Some(&sid))
            });
            c.status = Some(ConversationStatus {
                needs_attention: status.blocks_human(),
                status,
            });
            continue;
        }

        if matches!(
            c.status.as_ref().map(|s| &s.status),
            Some(SessionStatus::Waiting)
        ) {
            if let Some(summary) = jsonl::background_running_summary(&c.cwd, Some(&sid)) {
                c.status = Some(ConversationStatus {
                    status: SessionStatus::RunningWorkflow { summary },
                    needs_attention: false,
                });
            }
        }
    }

    // Settle each conversation's parent against the one cached in the overlay (see
    // `registry::settle_parent`): a cached parent is never re-derived from a shifting cwd.
    // New or refined parents are only RECORDED here — the gather stays read-only — and
    // `persist_parents` writes them from the long-running loops.
    let worktrees = WorktreeState::load();
    let projects = ProjectRegistry::load();
    let registered =
        |k: &str| worktrees.worktrees.contains_key(k) || projects.projects.contains_key(k);
    let mut pending = Vec::new();
    for (id, c) in reg.conversations.iter_mut() {
        let launch = disk_map.get(id.as_str()).and_then(|d| d.cwd.as_deref());
        let from_launch = launch.and_then(|l| registry::resolve_parent(l, &worktrees, &projects));
        let from_hook = if launch.is_none() {
            registry::resolve_parent(&c.cwd, &worktrees, &projects)
        } else {
            None
        };
        let settled = registry::settle_parent(
            c.parent.as_deref(),
            from_launch.as_deref(),
            from_hook.as_deref(),
            registered,
        );
        c.parent = settled.parent;
        if let Some(p) = settled.persist {
            pending.push((id.clone(), p));
        }
    }
    reg.pending_parents = pending;

    // Overlay the frozen facet (note + timestamp) so frozen windows read as
    // Closed+frozen (💤). Must run BEFORE bounding, since a pinned freeze is one
    // of the reasons a Closed conversation is surfaced.
    reg.apply_frozen(&FrozenState::load());

    // Bound the (unbounded) on-disk Closed set: Live is always shown; a Closed
    // conversation is kept only if recently active, parented, or a pinned freeze.
    // Archived ones are kept too (they're still their project's history — the
    // project detail lists them with a reason); hiding them from the default
    // listings is each consumer's call, not the registry's.
    let now = chrono::Utc::now();
    let cfg = registry::BoundingCfg { max_age_days: 14 };
    reg.conversations.retain(|_, c| {
        c.lifecycle.is_actionable_here() || registry::should_surface_closed(c, now, &cfg)
    });

    // Optional live-resource sample: sum each live conversation's window process
    // tree. `refresh_all` gives cpu_percent as a delta since the caller's last call.
    if let Some(sys) = stats {
        sys.refresh_all();
        for (id, pids) in &id_pids {
            if let Some(c) = reg.conversations.get_mut(id) {
                let (mut cpu, mut mem) = (0.0f32, 0u64);
                for &pid in pids {
                    if let Some(info) = get_process_info(sys, pid) {
                        cpu += info.cpu_percent;
                        mem += info.memory_kb;
                    }
                }
                c.cpu = cpu;
                c.mem_kb = mem;
                c.pids = pids.clone();
                c.ports = get_listening_ports_for_pids(pids, sys)
                    .into_iter()
                    .map(|lp| lp.port)
                    .collect();
            }
        }
    }

    reg
}

/// Convert a JSONL-parsed [`ClaudeStatus`] to a wire [`SessionStatus`]. Shared by
/// the conversation gather (live-status recovery) and the legacy web session path.
pub(crate) fn convert_claude_to_session_status(status: &ClaudeStatus) -> SessionStatus {
    match status {
        ClaudeStatus::Waiting => SessionStatus::Waiting,
        ClaudeStatus::NeedsPermission(tool, desc) => SessionStatus::NeedsPermission {
            tool_name: tool.clone(),
            description: desc.clone(),
        },
        ClaudeStatus::EditApproval(filename) => SessionStatus::EditApproval {
            filename: filename.clone(),
        },
        ClaudeStatus::PlanReview => SessionStatus::PlanReview,
        ClaudeStatus::QuestionAsked => SessionStatus::QuestionAsked,
        ClaudeStatus::Unknown => SessionStatus::Working,
    }
}

/// Whether a status is one that requires the user to act (the "needs attention"
/// axis), used when we recover status from a transcript rather than the hook.
/// Delegates to the canonical [`SessionStatus::blocks_human`].
fn status_needs_attention(s: &SessionStatus) -> bool {
    s.blocks_human()
}

/// Settle a conversation's status against Claude's own, given whatever hive had
/// inferred from the hook or the transcript.
///
/// Claude decides *which* of the four states it is in — it is the only party that
/// can see its own task table and its own open prompt. Hive's inference is kept
/// only where it adds detail Claude's vocabulary doesn't carry:
///
/// - `waiting` says a human is needed but not what for, so a specific blocking
///   status we already inferred (which permission, which file, a plan, a question)
///   is preserved. Anything non-blocking is discarded — it contradicts Claude.
/// - `shell` says background work is in flight but not what it is, so the summary
///   still comes from the transcript. `summary` is a closure because reading that
///   tail is the expensive part and only this branch needs it — two of eleven live
///   conversations here, rather than all eleven every tick.
///
/// `busy` and `idle` replace whatever we had outright. That is the point: an
/// orphaned background launch in the transcript, or a permission prompt the hook
/// recorded and never retracted, both survive indefinitely in hive's own reading
/// and are exactly what this corrects.
pub(crate) fn reconcile_with_claude(
    claude: &claude_sessions::ClaudeSession,
    inferred: Option<SessionStatus>,
    summary: impl FnOnce() -> Option<String>,
) -> SessionStatus {
    use claude_sessions::ClaudeSessionStatus as Cs;
    match claude.status {
        Some(Cs::Busy) => SessionStatus::Working,
        Some(Cs::Idle) => SessionStatus::Waiting,
        Some(Cs::Waiting) => match inferred {
            Some(s) if s.blocks_human() => s,
            _ => SessionStatus::NeedsInput {
                reason: claude.waiting_for.clone(),
            },
        },
        Some(Cs::Shell) => SessionStatus::RunningWorkflow {
            summary: summary()
                .or_else(|| claude.waiting_for.clone())
                .unwrap_or_else(|| "background shell".to_string()),
        },
        // `index_confirmed` never yields a record without a status; falling back to
        // what we inferred keeps this total without inventing an answer.
        None => inferred.unwrap_or(SessionStatus::Unknown),
    }
}

// ── Recovery frame ──────────────────────────────────────────────────────────

/// The registry's live windows, shaped as recovery-snapshot input.
///
/// The gather already resolves every running Claude window to its conversation id (hook id →
/// `--resume` from argv → recency fill), which is strictly more than the hooks see: a window
/// that hasn't run a turn since it opened fires no hook, so it is missing from the
/// hook-written snapshot while being perfectly visible here. Feeding this to
/// [`activity::sync_open_windows`] is what makes the recovery frame complete.
pub fn live_windows(reg: &ConversationRegistry) -> Vec<WindowSeen> {
    let mut out: Vec<WindowSeen> = reg
        .conversations
        .values()
        .filter(|c| c.lifecycle == Lifecycle::Live)
        .filter_map(|c| {
            let p = c.placement.as_ref()?;
            Some(WindowSeen {
                claude_session_id: c.id.as_str().to_string(),
                session_name: p.session_name.clone(),
                window_index: p.window_index.clone(),
                // The tmux window name mirrors the Claude title while it runs; the stored
                // title is the fallback for a window whose name hasn't synced yet.
                window_name: if p.window_name.is_empty() {
                    c.title.clone().unwrap_or_default()
                } else {
                    p.window_name.clone()
                },
                // The conversation's HOME — its transcript's launch dir (`registry::home_cwd`)
                // — never the pane's shell cwd or the latest hook cwd. Both drift: the shell
                // with every `cd`, the hook into a background Workflow's agent worktree under
                // the main checkout. A restore replays this value, so drift here re-homes the
                // conversation into the wrong session.
                cwd: c.cwd.clone(),
                claude_config_dir: c.auth_config_dir.clone(),
            })
        })
        .collect();
    // Stable order so the file's diff is meaningful when read by a human.
    out.sort_by(|a, b| a.claude_session_id.cmp(&b.claude_session_id));
    out
}

/// Reconcile the recovery snapshot against a freshly gathered registry. Call from the
/// long-running refresh loops (the TUI and the web data thread) — never from a one-shot
/// command, whose single observation says nothing about what closed.
pub fn sync_recovery_frame(reg: &ConversationRegistry) {
    activity::sync_open_windows(&live_windows(reg));
}

/// Write the parents a gather established or refined into the overlay, so the next gather
/// treats them as settled. Call from the long-running loops (TUI, web data thread), next to
/// [`sync_recovery_frame`]; the gather itself stays read-only.
///
/// Merges into a FRESHLY loaded sidecar rather than one captured at gather time: the TUI
/// edits the same file (pin, note, archive), and saving a stale copy would silently undo an
/// edit made in between. Only `parent` fields are touched, and nothing is written when every
/// pending parent is already on disk.
pub fn persist_parents(reg: &ConversationRegistry) {
    if reg.pending_parents.is_empty() {
        return;
    }
    let mut sidecar = ConversationSidecar::load();
    let mut changed = false;
    for (id, parent) in &reg.pending_parents {
        let overlay = sidecar.conversations.entry(id.clone()).or_default();
        if overlay.parent.as_deref() != Some(parent.as_str()) {
            overlay.parent = Some(parent.clone());
            changed = true;
        }
    }
    if changed {
        let _ = sidecar.save();
    }
}

// ── Reopen (resume a closed conversation) ───────────────────────────────────

/// Resolve a conversation's target tmux session name from its logical parent: a
/// worktree parent ("project/branch") carries its own recorded session name; a
/// project parent maps to the project's generated session name. None when the
/// conversation has no resolvable parent (e.g. the "unassigned" group).
pub fn target_session(c: &Conversation) -> Option<String> {
    let parent = c.parent.as_ref()?;
    if parent.contains('/') {
        let wts = WorktreeState::load();
        let name = wts.worktrees.get(parent).map(|e| e.session_name.clone())?;
        return (!name.is_empty()).then_some(name);
    }
    let projects = ProjectRegistry::load();
    let config = projects.projects.get(parent)?;
    Some(ProjectRegistry::session_name(parent, config))
}

/// Reopen a closed conversation: resume it (`claude --resume <id>`) in its
/// project/worktree session under its original auth profile — creating the
/// session if needed. Returns the target tmux session name. Does NOT switch or
/// attach the caller's tmux client (the TUI does that itself after; a web request
/// must not). `fallback_session` is used when the conversation has no resolvable
/// parent (the TUI passes the current tmux session; the web passes None).
pub fn reopen_conversation(c: &Conversation, fallback_session: Option<String>) -> Result<String> {
    // Resuming work in a project un-archives it — same rule as opening an archived
    // conversation, one level up. Shared by the TUI and the web's /api/resume.
    if let Some(parent) = c.parent.as_deref() {
        activate_project(parent);
    }

    // Frozen conversations thaw through the existing frozen path, which recreates
    // the window/session, resumes (`--resume <id>` or `claude -c` for id-less
    // legacy entries), and removes the frozen.json entry.
    if c.is_frozen() {
        return crate::common::frozen::thaw_window(c.id.as_str());
    }

    let startup = format!("claude --resume {}", c.id.as_str());
    // Resume under the same auth profile the conversation was created in.
    let env: Vec<(String, String)> = match &c.auth_config_dir {
        Some(dir) => vec![("CLAUDE_CONFIG_DIR".to_string(), dir.clone())],
        None => Vec::new(),
    };

    let target = target_session(c)
        .or(fallback_session)
        .ok_or_else(|| anyhow!("no target session (no project match)"))?;

    // Exact match — a bare `-t` prefix-matches a longer session name (see `tmux::exact`),
    // which would resume the conversation inside a different project's session.
    let tmux_target = crate::common::tmux::exact(&target);
    let alive = Command::new("tmux")
        .args(["has-session", "-t", &tmux_target])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);

    if alive {
        // Add a window to the existing session and resume in it.
        let mut cmd = Command::new("tmux");
        cmd.args(["new-window", "-t", &tmux_target, "-c", &c.cwd]);
        for (k, v) in &env {
            cmd.arg("-e").arg(format!("{k}={v}"));
        }
        if let Some(title) = &c.title {
            if !title.is_empty() {
                cmd.args(["-n", title]);
            }
        }
        if !cmd.output().map(|o| o.status.success()).unwrap_or(false) {
            return Err(anyhow!("failed to open a new window in '{target}'"));
        }
        // new-window made it the session's current window; type the resume into its
        // active PANE. `exact` alone is a session target — send-keys rejects it and
        // the window would sit at a bare shell (see `tmux::exact_active_pane`).
        let _ = Command::new("tmux")
            .args([
                "send-keys",
                "-t",
                &crate::common::tmux::exact_active_pane(&target),
                &startup,
                "Enter",
            ])
            .output();
    } else if !ensure_tmux_session(&target, &c.cwd, Some(&startup), &env) {
        return Err(anyhow!("failed to create session '{target}'"));
    }

    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::claude_sessions::{ClaudeSession, ClaudeSessionStatus};

    fn claude(status: ClaudeSessionStatus) -> ClaudeSession {
        ClaudeSession {
            pid: 1,
            session_id: Some("a".into()),
            status: Some(status),
            status_updated_at: Some(1),
            waiting_for: None,
        }
    }

    fn no_summary() -> Option<String> {
        None
    }

    #[test]
    fn busy_overrides_whatever_we_inferred() {
        let s = reconcile_with_claude(
            &claude(ClaudeSessionStatus::Busy),
            Some(SessionStatus::PlanReview),
            no_summary,
        );
        assert_eq!(s, SessionStatus::Working);
    }

    #[test]
    fn idle_clears_a_permission_prompt_the_hook_never_retracted() {
        // state.json keeps the last status a hook reported. If Claude was answered
        // outside hive's view the entry can sit on NeedsPermission indefinitely,
        // pinning the conversation in the Blocked tier — a tier that never empties
        // is a tier that stops being read.
        let s = reconcile_with_claude(
            &claude(ClaudeSessionStatus::Idle),
            Some(SessionStatus::NeedsPermission {
                tool_name: "Bash".into(),
                description: None,
            }),
            no_summary,
        );
        assert_eq!(s, SessionStatus::Waiting);
        assert!(!s.blocks_human());
    }

    #[test]
    fn idle_clears_an_orphaned_background_launch() {
        // The bug this whole change exists for: a backgrounded dev server never
        // exits, so it never emits the `<task-notification>` the transcript pairing
        // waits for, and the conversation reads as busy forever. Claude's own task
        // table says otherwise.
        let s = reconcile_with_claude(
            &claude(ClaudeSessionStatus::Idle),
            Some(SessionStatus::RunningWorkflow {
                summary: "bg: dev server".into(),
            }),
            || Some("bg: dev server".into()),
        );
        assert_eq!(s, SessionStatus::Waiting);
    }

    #[test]
    fn waiting_keeps_the_specific_prompt_we_already_knew_about() {
        // Claude says "a human is needed"; only hive knows it's a plan.
        let s = reconcile_with_claude(
            &claude(ClaudeSessionStatus::Waiting),
            Some(SessionStatus::PlanReview),
            no_summary,
        );
        assert_eq!(s, SessionStatus::PlanReview);
    }

    #[test]
    fn waiting_with_nothing_inferred_still_blocks() {
        // The case that used to read as plain idle: hook entry pruned, prompt long
        // since scrolled out of the transcript tail. Losing this is losing the one
        // window that cannot move without you.
        let s = reconcile_with_claude(&claude(ClaudeSessionStatus::Waiting), None, no_summary);
        assert_eq!(s, SessionStatus::NeedsInput { reason: None });
        assert!(s.blocks_human());
    }

    #[test]
    fn waiting_discards_a_non_blocking_inference() {
        let s = reconcile_with_claude(
            &claude(ClaudeSessionStatus::Waiting),
            Some(SessionStatus::Working),
            no_summary,
        );
        assert!(s.blocks_human());
    }

    #[test]
    fn waiting_carries_claudes_reason_when_it_gives_one() {
        let mut c = claude(ClaudeSessionStatus::Waiting);
        c.waiting_for = Some("approve the migration".into());
        let s = reconcile_with_claude(&c, None, no_summary);
        assert_eq!(
            s,
            SessionStatus::NeedsInput {
                reason: Some("approve the migration".into())
            }
        );
    }

    #[test]
    fn shell_takes_its_summary_from_the_transcript() {
        let s = reconcile_with_claude(&claude(ClaudeSessionStatus::Shell), None, || {
            Some("bg: Start the API dev server on port 4000".into())
        });
        assert_eq!(
            s,
            SessionStatus::RunningWorkflow {
                summary: "bg: Start the API dev server on port 4000".into()
            }
        );
    }

    #[test]
    fn shell_still_reports_when_the_transcript_has_no_summary() {
        // The launch can have scrolled out of the 256KB tail. Claude says a shell is
        // running; not knowing which one is no reason to report the session as idle.
        let s = reconcile_with_claude(&claude(ClaudeSessionStatus::Shell), None, no_summary);
        assert_eq!(
            s,
            SessionStatus::RunningWorkflow {
                summary: "background shell".into()
            }
        );
    }

    #[test]
    fn the_transcript_tail_is_only_read_for_shell() {
        // Reading it is the expensive part of the status pass, and only one of the
        // four states can use the result. Measured on this machine: 2 of 11 live
        // conversations, rather than all 11 on every tick.
        for status in [
            ClaudeSessionStatus::Busy,
            ClaudeSessionStatus::Idle,
            ClaudeSessionStatus::Waiting,
        ] {
            let mut read = false;
            reconcile_with_claude(&claude(status), None, || {
                read = true;
                None
            });
            assert!(!read, "{status:?} must not read the transcript");
        }
    }
}
