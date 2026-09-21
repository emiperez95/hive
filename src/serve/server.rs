//! Data gathering for the web dashboard's JSON API.
//!
//! Projects the shared [`crate::common::conversations`] registry into the wire
//! shapes the dashboard consumes: [`gather_active_views`] (the session-grouped
//! live view, `/api/active`) and [`build_conversation_views`] (the closed/frozen
//! set, `/api/conversations`). A single tmux session can host several Claude
//! windows; each becomes a [`WindowView`] and they aggregate into a
//! [`SessionView`], which the dashboard renders as an accordion.

use crate::common::persistence::{load_session_todos, load_skipped_sessions};
use crate::common::process::get_process_info;
use crate::common::projects::ProjectRegistry;
use crate::common::registry::{Conversation, ConversationRegistry};
use crate::common::tmux::{get_other_client_sessions, get_tmux_sessions};
use crate::ipc::messages::SessionStatus;
use crate::serve::web_types::{
    ConversationView, PlacementView, ProcessView, SessionView, WindowView,
};

use std::collections::{HashMap, HashSet};
use std::time::Instant;
use sysinfo::System;

/// Auto-approved sessions should surface as Working, never as a permission/edit prompt.
fn mask_auto_approve(
    status: Option<SessionStatus>,
    is_auto_approve: bool,
) -> Option<SessionStatus> {
    if !is_auto_approve {
        return status;
    }
    match status {
        Some(SessionStatus::NeedsPermission { .. }) | Some(SessionStatus::EditApproval { .. }) => {
            Some(SessionStatus::Working)
        }
        other => other,
    }
}

/// Pick the session-level status from its windows: a window needing attention wins,
/// then any working window, then waiting, falling back to the first window's status.
fn aggregate_status(windows: &[WindowView]) -> Option<SessionStatus> {
    if windows.is_empty() {
        return None;
    }
    if let Some(w) = windows
        .iter()
        .find(|w| w.status.as_ref().is_some_and(SessionStatus::blocks_human))
    {
        return w.status.clone();
    }
    if windows
        .iter()
        .any(|w| matches!(w.status, Some(SessionStatus::Working)))
    {
        return Some(SessionStatus::Working);
    }
    // A running workflow on any window is busy — surface it above an idle window.
    if let Some(w) = windows
        .iter()
        .find(|w| matches!(w.status, Some(SessionStatus::RunningWorkflow { .. })))
    {
        return w.status.clone();
    }
    if windows
        .iter()
        .any(|w| matches!(w.status, Some(SessionStatus::Waiting)))
    {
        return Some(SessionStatus::Waiting);
    }
    windows.first().and_then(|w| w.status.clone())
}

// ── Conversation-first view (new `/api/conversations`) ──────────────────────

/// Build the conversation-first view for `/api/conversations` from an already-
/// gathered registry (live + closed + frozen, UUID-keyed) so the web dashboard
/// sees the same model as the TUI. The caller gathers the registry once and feeds
/// both this and [`gather_active_views`].
pub(crate) fn build_conversation_views(reg: &ConversationRegistry) -> Vec<ConversationView> {
    let projects = ProjectRegistry::load();
    let mut views: Vec<ConversationView> = reg
        .conversations
        .values()
        // Archived conversations stay in the registry (the TUI's project detail
        // lists them with their archive reason) but are set aside by definition —
        // keep them off the Resume view unless they're actually running.
        .filter(|c| !c.archived || c.lifecycle.is_actionable_here())
        .map(|c| build_conversation_view(c, &projects))
        .collect();
    // Live first, then most-recently-active first — a stable, sensible default;
    // the frontend regroups by project/session as needed.
    views.sort_by(|a, b| {
        let a_live = a.lifecycle == "live";
        let b_live = b.lifecycle == "live";
        b_live
            .cmp(&a_live)
            .then_with(|| b.last_activity.cmp(&a.last_activity))
            .then_with(|| a.id.cmp(&b.id))
    });
    views
}

/// Project the registry's LIVE conversations into the session-grouped
/// [`SessionView`] shape the Active view consumes (`/api/active`), plus every
/// live tmux session as a bare/startable entry. The conversation-model
/// replacement for the old session gather: each window carries a stable
/// conversation id and the registry's robust window→conversation resolution +
/// enriched status (so shared-cwd multi-Claude sessions report correct per-window
/// status, which the hook-only path could not). CPU/mem/ports come from the
/// registry sample; `sys` (kept alive by the caller for CPU deltas) is used only
/// to build the per-process breakdown for the info modal.
pub(crate) fn gather_active_views(reg: &ConversationRegistry, sys: &System) -> Vec<SessionView> {
    let sessions = match get_tmux_sessions() {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let other_client_sessions = get_other_client_sessions();
    let skipped_sessions = load_skipped_sessions();
    let auto_approve_sessions = crate::common::persistence::load_auto_approve_sessions();
    let session_todos = load_session_todos();

    // Group live conversations by the tmux session running them.
    let mut convs_by_session: HashMap<String, Vec<&Conversation>> = HashMap::new();
    for c in reg.conversations.values() {
        if !c.lifecycle.is_actionable_here() {
            continue;
        }
        if let Some(p) = &c.placement {
            convs_by_session
                .entry(p.session_name.clone())
                .or_default()
                .push(c);
        }
    }

    let mut results = Vec::new();
    for session in &sessions {
        let is_auto_approve = auto_approve_sessions.contains(&session.name);

        let mut session_convs = convs_by_session.remove(&session.name).unwrap_or_default();
        session_convs.sort_by(|a, b| window_index_of(a).cmp(window_index_of(b)));

        let windows: Vec<WindowView> = session_convs
            .iter()
            .map(|c| conv_to_window_view(c, is_auto_approve))
            .collect();

        let cpu = windows.iter().map(|w| w.cpu).sum();
        let mem_kb = windows.iter().map(|w| w.mem_kb).sum();
        let mut ports: Vec<u16> = windows
            .iter()
            .flat_map(|w| w.ports.iter().copied())
            .collect();
        ports.sort_unstable();
        ports.dedup();
        let last_activity = windows.iter().filter_map(|w| w.last_activity.clone()).max();
        let status = aggregate_status(&windows);

        let session_cwd = session
            .windows
            .first()
            .and_then(|w| w.panes.first())
            .map(|p| p.cwd.clone());

        // Bare session with no Claude: expose the pane only when `claude -c` failed
        // with "No conversation found to continue" (safe to offer a fresh start).
        let mut pane = windows.first().and_then(|w| w.pane.clone());
        let mut claude_continue_failed = false;
        if pane.is_none() {
            if let Some((s, w, p)) = session.windows.first().and_then(|win| {
                win.panes
                    .first()
                    .map(|p| (session.name.clone(), win.index.clone(), p.index.clone()))
            }) {
                if crate::common::tmux::capture_pane(&s, &w, &p)
                    .is_some_and(|t| t.contains("No conversation found to continue"))
                {
                    claude_continue_failed = true;
                    pane = Some((s, w, p));
                }
            }
        }

        // Per-process breakdown (info modal), from this session's Claude pids.
        let mut processes: Vec<ProcessView> = session_convs
            .iter()
            .flat_map(|c| c.pids.iter().copied())
            .filter_map(|pid| {
                get_process_info(sys, pid).map(|info| ProcessView {
                    pid: info.pid,
                    name: info.name,
                    cpu_percent: info.cpu_percent,
                    memory_kb: info.memory_kb,
                    command: info.command,
                })
            })
            .collect();
        processes.sort_by(|a, b| {
            b.cpu_percent
                .partial_cmp(&a.cpu_percent)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        results.push(SessionView {
            name: session.name.clone(),
            status,
            cpu,
            mem_kb,
            ports,
            processes,
            cwd: session_cwd,
            last_activity,
            attached: other_client_sessions.contains(&session.name),
            pane,
            claude_continue_failed,
            skipped: skipped_sessions.contains(&session.name),
            todo_count: session_todos
                .get(&session.name)
                .map(|t| t.len() as u32)
                .unwrap_or(0),
            messages: Vec::new(),
            windows,
        });
    }

    results
}

/// A live conversation's tmux window index (for ordering windows within a session).
fn window_index_of(c: &Conversation) -> &str {
    c.placement
        .as_ref()
        .map(|p| p.window_index.as_str())
        .unwrap_or("")
}

/// Build one [`WindowView`] from a live conversation: its placement + the
/// registry-resolved status/resources, with auto-approve masking applied.
fn conv_to_window_view(c: &Conversation, is_auto_approve: bool) -> WindowView {
    let (session, window_index, window_name, pane_id) = match &c.placement {
        Some(p) => (
            p.session_name.clone(),
            p.window_index.clone(),
            p.window_name.clone(),
            p.pane_id.clone().unwrap_or_default(),
        ),
        None => (String::new(), String::new(), String::new(), String::new()),
    };
    let status = mask_auto_approve(c.status.as_ref().map(|s| s.status.clone()), is_auto_approve);
    WindowView {
        pane_id: pane_id.clone(),
        window_index: window_index.clone(),
        window_name,
        session_id: Some(c.id.as_str().to_string()),
        status,
        cpu: c.cpu,
        mem_kb: c.mem_kb,
        ports: c.ports.clone(),
        cwd: (!c.cwd.is_empty()).then(|| c.cwd.clone()),
        last_activity: c.last_activity.clone(),
        pane: Some((session, window_index, pane_id)),
        // Filled by `annotate_attention`, which owns the I/O and the clock. Keeping
        // this function pure is what lets the view-building tests run without a git
        // repo or a wall clock.
        attention: AttentionTier::Idle as u8,
        unreviewed_work: false,
        state_secs: None,
    }
}

// ── Attention ordering ──────────────────────────────────────────────────────
//
// The sidebar can rank conversations by how much they want a human. The ranking
// is computed here, on the wire, rather than in `web.html`: the frontend has no
// test harness, and a second copy of "is this blocked" would drift from
// `SessionStatus::blocks_human`.

/// How badly a conversation wants a human, worst first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum AttentionTier {
    /// A decision is pending; nothing moves until someone answers.
    Blocked = 0,
    /// Idle, with work in its tree that nobody has looked at.
    ReadyForReview = 1,
    /// Busy. Nothing to do but let it run.
    Working = 2,
    /// Idle with nothing pending and nothing produced.
    Idle = 3,
}

/// Pure tier assignment.
///
/// `unreviewed` is only consulted for an idle window — a conversation that is still
/// working has not handed anything over, however dirty its tree looks.
pub(crate) fn tier_for(status: Option<&SessionStatus>, unreviewed: bool) -> AttentionTier {
    match status {
        Some(s) if s.blocks_human() => AttentionTier::Blocked,
        Some(SessionStatus::Working)
        | Some(SessionStatus::RunningWorkflow { .. })
        | Some(SessionStatus::Unknown) => AttentionTier::Working,
        // `Waiting`, or no status at all (no Claude resolved for the window yet).
        _ if unreviewed => AttentionTier::ReadyForReview,
        _ => AttentionTier::Idle,
    }
}

/// A stable discriminant for "the same kind of wait".
///
/// Payloads are excluded on purpose: a `NeedsPermission` whose tool name changes is
/// still one unbroken wait, and resetting its timer would keep it looking fresh
/// forever.
fn status_kind(s: Option<&SessionStatus>) -> u8 {
    match s {
        None => 0,
        Some(SessionStatus::Waiting) => 1,
        Some(SessionStatus::Working) => 2,
        Some(SessionStatus::Unknown) => 3,
        Some(SessionStatus::RunningWorkflow { .. }) => 4,
        Some(SessionStatus::NeedsPermission { .. }) => 5,
        Some(SessionStatus::EditApproval { .. }) => 6,
        Some(SessionStatus::PlanReview) => 7,
        Some(SessionStatus::QuestionAsked) => 8,
        Some(SessionStatus::NeedsInput { .. }) => 9,
    }
}

/// How long each conversation has held its current status.
///
/// Nothing in hive records status transitions — `last_activity` is "last hook event
/// fired", stamped unconditionally, and was measured reporting a `Working`
/// conversation as 29 minutes inactive. So the web data thread watches for changes
/// itself. In memory only: this is a sort key, not a fact worth persisting.
#[derive(Default)]
pub(crate) struct StateAges {
    seen: HashMap<String, (u8, Instant)>,
}

impl StateAges {
    /// Record `id` as being in `kind` at `now`; return how long it has held it.
    ///
    /// `None` on the first sighting — it may have been in that status for hours
    /// before hive started watching, and claiming 0 would be a lie. A transition
    /// observed while running returns `Some(0)` and climbs from there, which is the
    /// real thing.
    pub(crate) fn observe(&mut self, id: &str, kind: u8, now: Instant) -> Option<u32> {
        match self.seen.get_mut(id) {
            Some((prev, since)) if *prev == kind => {
                Some(now.saturating_duration_since(*since).as_secs() as u32)
            }
            Some(entry) => {
                *entry = (kind, now);
                Some(0)
            }
            None => {
                self.seen.insert(id.to_string(), (kind, now));
                None
            }
        }
    }

    /// Drop conversations that are no longer live, so the map can't grow forever.
    pub(crate) fn gc(&mut self, live: &HashSet<String>) {
        self.seen.retain(|id, _| live.contains(id));
    }
}

/// Fill in `attention` / `unreviewed_work` / `state_secs` across a gathered view.
///
/// `unreviewed` is injected so this is testable without a git repo, and so the
/// call-count contract below can be asserted rather than merely commented.
///
/// **It is only called for idle windows.** That is what keeps the cost sane: the
/// git probe is ~0.045s per working tree, and probing all 13 live trees every tick
/// would be 0.59s of the 1s budget. A busy or blocked window's tree is irrelevant
/// to its tier, so it is never asked about.
pub(crate) fn annotate_attention(
    views: &mut [SessionView],
    ages: &mut StateAges,
    unreviewed: &mut dyn FnMut(&str) -> bool,
    now: Instant,
) {
    let mut live: HashSet<String> = HashSet::new();
    for view in views.iter_mut() {
        // Skipped sessions are deliberately set aside; they are not switch targets
        // for the attention view any more than they are for `cycle-free`.
        if view.skipped {
            continue;
        }
        for w in view.windows.iter_mut() {
            let idle = matches!(w.status, None | Some(SessionStatus::Waiting));
            w.unreviewed_work = idle
                && w.cwd
                    .as_deref()
                    .is_some_and(|cwd| !cwd.is_empty() && unreviewed(cwd));
            w.attention = tier_for(w.status.as_ref(), w.unreviewed_work) as u8;
            if let Some(id) = &w.session_id {
                w.state_secs = ages.observe(id, status_kind(w.status.as_ref()), now);
                live.insert(id.clone());
            }
        }
    }
    ages.gc(&live);
}

fn build_conversation_view(c: &Conversation, projects: &ProjectRegistry) -> ConversationView {
    let short_id = if c.id.as_str().contains('#') {
        "—".to_string()
    } else {
        c.id.as_str().chars().take(8).collect()
    };
    // parent is "project" or "project/branch"; the project part drives emoji/grouping.
    let project_key = c.parent.as_deref().and_then(|p| {
        let pk = p.split('/').next().unwrap_or(p);
        projects.projects.contains_key(pk).then(|| pk.to_string())
    });
    let emoji = project_key
        .as_deref()
        .and_then(|pk| projects.projects.get(pk))
        .map(|cfg| cfg.emoji.clone())
        .unwrap_or_default();
    // Auth profile = the `.claude-<name>` suffix of the config dir; None = default.
    let auth_profile = c.auth_config_dir.as_ref().and_then(|dir| {
        std::path::Path::new(dir)
            .file_name()
            .and_then(|s| s.to_str())
            .and_then(|b| b.strip_prefix(".claude-"))
            .map(|s| s.to_string())
    });
    let (frozen, frozen_note, frozen_relative) = match &c.frozen {
        Some(f) => (
            true,
            Some(f.note.clone()).filter(|s| !s.is_empty()),
            Some(crate::common::frozen::relative_time(&f.frozen_at)),
        ),
        None => (false, None, None),
    };
    let placement = c.placement.as_ref().map(|p| PlacementView {
        session_name: p.session_name.clone(),
        window_index: p.window_index.clone(),
        window_name: p.window_name.clone(),
        pane_id: p.pane_id.clone(),
    });
    ConversationView {
        id: c.id.as_str().to_string(),
        short_id,
        title: c.title.clone(),
        cwd: (!c.cwd.is_empty()).then(|| c.cwd.clone()),
        lifecycle: if c.lifecycle.is_actionable_here() {
            "live"
        } else {
            "closed"
        }
        .to_string(),
        status: c.status.as_ref().map(|s| s.status.clone()),
        needs_attention: c
            .status
            .as_ref()
            .map(|s| s.needs_attention)
            .unwrap_or(false),
        last_activity: c.last_activity.clone(),
        placement,
        parent: c.parent.clone(),
        project_key,
        emoji,
        frozen,
        frozen_note,
        frozen_relative,
        note: c.note.clone(),
        pinned: c.pinned,
        archived: c.archived,
        auth_profile,
        cpu: c.cpu,
        mem_kb: c.mem_kb,
        ports: c.ports.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn win(id: &str, status: Option<SessionStatus>, cwd: &str) -> WindowView {
        WindowView {
            pane_id: "%1".into(),
            window_index: "1".into(),
            window_name: "w".into(),
            session_id: Some(id.into()),
            status,
            cpu: 0.0,
            mem_kb: 0,
            ports: vec![],
            cwd: (!cwd.is_empty()).then(|| cwd.to_string()),
            last_activity: None,
            pane: None,
            attention: AttentionTier::Idle as u8,
            unreviewed_work: false,
            state_secs: None,
        }
    }

    fn session(name: &str, skipped: bool, windows: Vec<WindowView>) -> SessionView {
        SessionView {
            name: name.into(),
            status: None,
            cpu: 0.0,
            mem_kb: 0,
            ports: vec![],
            processes: vec![],
            cwd: None,
            last_activity: None,
            attached: false,
            pane: None,
            claude_continue_failed: false,
            skipped,
            todo_count: 0,
            messages: vec![],
            windows,
        }
    }

    #[test]
    fn blocked_beats_a_dirty_tree() {
        // A pending decision outranks anything the tree looks like — you cannot
        // review work from a conversation that is still asking you a question.
        for s in [
            SessionStatus::PlanReview,
            SessionStatus::QuestionAsked,
            SessionStatus::NeedsPermission {
                tool_name: "Bash".into(),
                description: None,
            },
            SessionStatus::EditApproval {
                filename: "a.rs".into(),
            },
        ] {
            assert_eq!(tier_for(Some(&s), true), AttentionTier::Blocked);
            assert_eq!(tier_for(Some(&s), false), AttentionTier::Blocked);
        }
    }

    #[test]
    fn busy_is_working_never_ready() {
        for s in [
            SessionStatus::Working,
            SessionStatus::Unknown,
            SessionStatus::RunningWorkflow {
                summary: "3 agents".into(),
            },
        ] {
            assert_eq!(
                tier_for(Some(&s), true),
                AttentionTier::Working,
                "{s:?} has not handed anything over yet, however dirty its tree"
            );
        }
    }

    #[test]
    fn idle_splits_on_unreviewed_work() {
        assert_eq!(
            tier_for(Some(&SessionStatus::Waiting), true),
            AttentionTier::ReadyForReview
        );
        assert_eq!(
            tier_for(Some(&SessionStatus::Waiting), false),
            AttentionTier::Idle
        );
        // No resolved status behaves like idle.
        assert_eq!(tier_for(None, true), AttentionTier::ReadyForReview);
        assert_eq!(tier_for(None, false), AttentionTier::Idle);
    }

    #[test]
    fn tiers_sort_worst_first() {
        let mut v = [
            AttentionTier::Idle,
            AttentionTier::Blocked,
            AttentionTier::Working,
            AttentionTier::ReadyForReview,
        ];
        v.sort();
        assert_eq!(
            v,
            [
                AttentionTier::Blocked,
                AttentionTier::ReadyForReview,
                AttentionTier::Working,
                AttentionTier::Idle
            ]
        );
    }

    #[test]
    fn state_ages_first_sighting_is_unknown_not_zero() {
        let mut ages = StateAges::default();
        let t0 = Instant::now();
        assert_eq!(
            ages.observe("a", 5, t0),
            None,
            "it may have been blocked for an hour before hive started watching"
        );
        assert_eq!(ages.observe("a", 5, t0 + Duration::from_secs(30)), Some(30));
    }

    #[test]
    fn state_ages_reset_only_when_the_kind_changes() {
        let mut ages = StateAges::default();
        let t0 = Instant::now();
        ages.observe("a", 5, t0);
        assert_eq!(ages.observe("a", 5, t0 + Duration::from_secs(10)), Some(10));
        // Kind changed: observed live, so zero is the truth here.
        assert_eq!(ages.observe("a", 2, t0 + Duration::from_secs(11)), Some(0));
        assert_eq!(ages.observe("a", 2, t0 + Duration::from_secs(20)), Some(9));
    }

    #[test]
    fn state_ages_ignore_payload_churn() {
        // Two NeedsPermission for different tools share a kind, so one unbroken wait
        // keeps accumulating instead of looking perpetually fresh.
        let a = SessionStatus::NeedsPermission {
            tool_name: "Bash".into(),
            description: None,
        };
        let b = SessionStatus::NeedsPermission {
            tool_name: "Write".into(),
            description: Some("x".into()),
        };
        assert_eq!(status_kind(Some(&a)), status_kind(Some(&b)));

        let mut ages = StateAges::default();
        let t0 = Instant::now();
        ages.observe("a", status_kind(Some(&a)), t0);
        assert_eq!(
            ages.observe("a", status_kind(Some(&b)), t0 + Duration::from_secs(60)),
            Some(60)
        );
    }

    #[test]
    fn state_ages_gc_drops_conversations_that_ended() {
        let mut ages = StateAges::default();
        let t0 = Instant::now();
        ages.observe("gone", 1, t0);
        ages.observe("here", 1, t0);
        ages.gc(&HashSet::from(["here".to_string()]));
        assert_eq!(
            ages.observe("here", 1, t0 + Duration::from_secs(5)),
            Some(5)
        );
        assert_eq!(
            ages.observe("gone", 1, t0 + Duration::from_secs(5)),
            None,
            "a pruned id is a first sighting again"
        );
    }

    #[test]
    fn annotate_probes_git_only_for_idle_windows() {
        // The performance contract, asserted rather than commented: the git probe is
        // ~0.045s per tree and there are 13 live trees, so probing busy or blocked
        // windows too would spend most of the 1s gather budget on answers that can
        // never change a tier.
        let mut views = vec![session(
            "s",
            false,
            vec![
                win("idle", Some(SessionStatus::Waiting), "/w/idle"),
                win("busy", Some(SessionStatus::Working), "/w/busy"),
                win("blocked", Some(SessionStatus::PlanReview), "/w/blocked"),
                win("nostatus", None, "/w/nostatus"),
            ],
        )];
        let mut asked: Vec<String> = Vec::new();
        let mut ages = StateAges::default();
        annotate_attention(
            &mut views,
            &mut ages,
            &mut |cwd| {
                asked.push(cwd.to_string());
                true
            },
            Instant::now(),
        );

        assert_eq!(asked, vec!["/w/idle", "/w/nostatus"]);
        let w = &views[0].windows;
        assert_eq!(w[0].attention, AttentionTier::ReadyForReview as u8);
        assert_eq!(w[1].attention, AttentionTier::Working as u8);
        assert_eq!(w[2].attention, AttentionTier::Blocked as u8);
        assert_eq!(w[3].attention, AttentionTier::ReadyForReview as u8);
        assert!(!w[1].unreviewed_work, "a busy window is never marked ready");
    }

    #[test]
    fn annotate_skips_skipped_sessions_entirely() {
        let mut views = vec![session(
            "skipped",
            true,
            vec![win("s1", Some(SessionStatus::Waiting), "/w/s1")],
        )];
        let mut asked = 0;
        let mut ages = StateAges::default();
        annotate_attention(
            &mut views,
            &mut ages,
            &mut |_| {
                asked += 1;
                true
            },
            Instant::now(),
        );
        assert_eq!(
            asked, 0,
            "skipped is set aside — never probed, never ranked"
        );
        assert!(views[0].windows[0].state_secs.is_none());
    }

    #[test]
    fn annotate_handles_a_missing_cwd_without_shelling_out() {
        let mut views = vec![session(
            "s",
            false,
            vec![win("a", Some(SessionStatus::Waiting), "")],
        )];
        let mut asked = 0;
        let mut ages = StateAges::default();
        annotate_attention(
            &mut views,
            &mut ages,
            &mut |_| {
                asked += 1;
                true
            },
            Instant::now(),
        );
        assert_eq!(asked, 0, "`git -C ''` is never worth spawning");
        assert_eq!(views[0].windows[0].attention, AttentionTier::Idle as u8);
    }
}
