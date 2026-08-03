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

use crate::common::frozen::FrozenState;
use crate::common::instances;
use crate::common::jsonl;
use crate::common::ports::get_listening_ports_for_pids;
use crate::common::process::{build_cmdline_map, get_process_info, parse_resume_id};
use crate::common::projects::{ensure_tmux_session, ProjectRegistry};
use crate::common::registry::{
    self, Conversation, ConversationRegistry, ConversationSidecar, ConversationStatus,
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

    // Enrich disk-only (Closed) conversations with cwd + last-activity + title
    // from the transcript — the hook side had none, so without this they can't
    // be placed or bounded.
    let disk_map: HashMap<&str, &jsonl::DiskConversation> =
        disk.iter().map(|d| (d.id.as_str(), d)).collect();
    for (id, c) in reg.conversations.iter_mut() {
        if let Some(d) = disk_map.get(id.as_str()) {
            if c.cwd.is_empty() {
                if let Some(cwd) = &d.cwd {
                    c.cwd = cwd.clone();
                }
            }
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

    // Resolve a logical parent for any conversation without one cached.
    let worktrees = WorktreeState::load();
    let projects = ProjectRegistry::load();
    for c in reg.conversations.values_mut() {
        if c.parent.is_none() {
            c.parent = registry::resolve_parent(&c.cwd, &worktrees, &projects);
        }
    }

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
fn status_needs_attention(s: &SessionStatus) -> bool {
    matches!(
        s,
        SessionStatus::NeedsPermission { .. }
            | SessionStatus::EditApproval { .. }
            | SessionStatus::PlanReview
            | SessionStatus::QuestionAsked
    )
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
