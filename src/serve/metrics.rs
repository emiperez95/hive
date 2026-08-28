//! Prometheus text-format metrics, served at `GET /metrics`.
//!
//! Why a scrape endpoint rather than an OTLP push: hive is deliberately synchronous (no tokio,
//! see CLAUDE.md), and `opentelemetry-otlp`'s gRPC transport pulls in tonic + an async runtime.
//! The web server is already a long-lived `tiny_http` process that autostarts (`__hive_web`), so
//! exposing the text exposition format and letting the OTel collector scrape it costs zero new
//! dependencies and no runtime.
//!
//! The point of this module is to put hive's own view of the work — how many agent windows are
//! open, how long each project was actually worked on, how much worktree debt has accumulated —
//! into the same Grafana as Claude Code's native `claude_code.*` cost and token metrics.

use crate::common::activity;
use crate::common::frozen::FrozenState;
use crate::common::projects::ProjectRegistry;
use crate::common::registry::ConversationRegistry;
use crate::common::tmux::get_current_tmux_session_names;
use crate::common::worktree::WorktreeState;
use crate::ipc::messages::SessionStatus;
use std::collections::BTreeMap;
use std::fmt::Write as _;

/// Stable, low-cardinality label for a status variant. The payload-carrying variants
/// (tool name, filename, workflow summary) are deliberately **not** included — they are
/// unbounded free text and would explode series cardinality.
fn status_label(status: &SessionStatus) -> &'static str {
    match status {
        SessionStatus::Waiting => "waiting",
        SessionStatus::NeedsPermission { .. } => "needs_permission",
        SessionStatus::EditApproval { .. } => "edit_approval",
        SessionStatus::PlanReview => "plan_review",
        SessionStatus::QuestionAsked => "question_asked",
        SessionStatus::RunningWorkflow { .. } => "running_workflow",
        SessionStatus::Working => "working",
        SessionStatus::Unknown => "unknown",
    }
}

/// True for statuses that mean "this agent is stopped until a human answers". This is the
/// attention bottleneck the adoption model describes at Step 1/2 — the thing that caps how
/// many agents one person can actually keep moving.
fn is_blocked(status: &SessionStatus) -> bool {
    matches!(
        status,
        SessionStatus::NeedsPermission { .. }
            | SessionStatus::EditApproval { .. }
            | SessionStatus::PlanReview
            | SessionStatus::QuestionAsked
    )
}

/// Aggregates of the conversation registry, computed **once per data-thread tick** rather than
/// per scrape.
///
/// The registry is hive's base entity and the single richest thing it knows that Claude Code's
/// own telemetry cannot see: Claude reports what a session *spent*, hive reports how many
/// sessions exist, which project they belong to, whether they're live or merely resumable, and
/// whether they're blocked waiting on a human. Gathering it costs a full process/tmux/JSONL
/// sweep, so `/metrics` must never trigger one — the web data thread already gathers every
/// second and hands the result here.
///
/// Built from the **whole** registry, including archived conversations (which the
/// `/api/conversations` view filters out) — a count that silently omits set-aside work would
/// misreport the backlog.
#[derive(Debug, Clone, Default)]
pub struct RegistrySnapshot {
    /// (project, lifecycle) → count. Project is `"(unassigned)"` when unresolvable.
    pub by_project: BTreeMap<(String, &'static str), usize>,
    /// status label → count, live conversations only.
    pub by_status: BTreeMap<&'static str, usize>,
    /// auth profile (`work`, …) → live count; `"default"` for plain `~/.claude`.
    pub by_auth: BTreeMap<String, usize>,
    /// CPU percent summed per project, live conversations only.
    pub cpu_by_project: BTreeMap<String, f32>,
    /// Resident memory summed per project, live conversations only.
    pub mem_kb_by_project: BTreeMap<String, u64>,
    pub live: usize,
    pub closed: usize,
    pub archived: usize,
    pub pinned: usize,
    /// Live conversations flagged as needing the user.
    pub needs_attention: usize,
    /// Live conversations stopped until a human answers (see [`is_blocked`]).
    pub blocked: usize,
}

impl RegistrySnapshot {
    pub fn from_registry(reg: &ConversationRegistry) -> Self {
        let mut snap = Self::default();
        for c in reg.conversations.values() {
            // `parent` is "project" or "project/branch"; the project part is what groups.
            let project = c
                .parent
                .as_deref()
                .map(|p| p.split('/').next().unwrap_or(p).to_string())
                .unwrap_or_else(|| "(unassigned)".to_string());

            let live = c.lifecycle.is_actionable_here();
            let lifecycle = if live { "live" } else { "closed" };
            *snap
                .by_project
                .entry((project.clone(), lifecycle))
                .or_default() += 1;

            if live {
                snap.live += 1;
            } else {
                snap.closed += 1;
            }
            if c.archived {
                snap.archived += 1;
            }
            if c.pinned {
                snap.pinned += 1;
            }

            if !live {
                continue;
            }

            if let Some(st) = &c.status {
                *snap.by_status.entry(status_label(&st.status)).or_default() += 1;
                if st.needs_attention {
                    snap.needs_attention += 1;
                }
                if is_blocked(&st.status) {
                    snap.blocked += 1;
                }
            }

            let profile = c
                .auth_config_dir
                .as_deref()
                .and_then(|dir| {
                    std::path::Path::new(dir)
                        .file_name()
                        .and_then(|s| s.to_str())
                        .and_then(|b| b.strip_prefix(".claude-"))
                })
                .unwrap_or("default")
                .to_string();
            *snap.by_auth.entry(profile).or_default() += 1;

            *snap.cpu_by_project.entry(project.clone()).or_default() += c.cpu;
            *snap.mem_kb_by_project.entry(project).or_default() += c.mem_kb;
        }
        snap
    }
}

/// Lookback for the counter series. The activity log is append-only, so "all time" is what
/// gives counters their required monotonicity — a rolling window would sawtooth as events age
/// out and Prometheus would read every decrease as a counter reset.
const ALL_TIME_DAYS: i64 = 36_500;

/// Escape a Prometheus label value: backslash, double-quote and newline are the only characters
/// the text format reserves. Session names are user-authored and routinely contain emoji and
/// spaces, both of which are fine unescaped (the format is UTF-8).
fn escape_label(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            _ => out.push(c),
        }
    }
    out
}

/// Emit `# HELP` / `# TYPE` headers followed by a single unlabelled sample.
fn scalar(out: &mut String, name: &str, help: &str, kind: &str, value: impl std::fmt::Display) {
    let _ = writeln!(out, "# HELP {name} {help}");
    let _ = writeln!(out, "# TYPE {name} {kind}");
    let _ = writeln!(out, "{name} {value}");
}

/// Render the full exposition payload.
///
/// `snapshot` carries the conversation-registry aggregates. It is `None` when no gather has
/// happened yet (first second of `hive web`) or in tests — those series are simply omitted
/// rather than reported as zero, so a missing gather can't be mistaken for "no conversations".
pub fn render(snapshot: Option<&RegistrySnapshot>) -> String {
    let stats = activity::compute_stats(ALL_TIME_DAYS);
    let mut out = String::with_capacity(8192);

    // ---- Lifecycle counters -------------------------------------------------------------
    // These answer "how much agent work is being started, and is it being cleaned up".
    scalar(
        &mut out,
        "hive_windows_opened_total",
        "Claude windows opened (all time).",
        "counter",
        stats.opened,
    );
    scalar(
        &mut out,
        "hive_windows_closed_total",
        "Claude windows closed cleanly (all time).",
        "counter",
        stats.closed,
    );
    scalar(
        &mut out,
        "hive_windows_frozen_total",
        "Claude windows frozen/hibernated (all time).",
        "counter",
        stats.frozen,
    );
    scalar(
        &mut out,
        "hive_windows_thawed_total",
        "Frozen Claude windows resumed (all time).",
        "counter",
        stats.thawed,
    );
    scalar(
        &mut out,
        "hive_sessions_killed_total",
        "Whole tmux sessions killed (all time).",
        "counter",
        stats.killed,
    );
    scalar(
        &mut out,
        "hive_focus_switches_total",
        "Focus changes between windows (all time).",
        "counter",
        stats.switches,
    );
    scalar(
        &mut out,
        "hive_web_views_total",
        "Web dashboard session views (all time).",
        "counter",
        stats.web_views,
    );

    // ---- Current state ------------------------------------------------------------------
    // `hive_windows_open` is the concurrency signal: how many agents are running side by side
    // right now. Graphed over time it is the single clearest measure of parallel operation.
    scalar(
        &mut out,
        "hive_windows_open",
        "Claude windows currently open (concurrency).",
        "gauge",
        stats.open_now,
    );
    scalar(
        &mut out,
        "hive_frozen_windows",
        "Claude windows currently frozen, awaiting resume.",
        "gauge",
        FrozenState::load().frozen.len(),
    );

    // ---- Active time per project --------------------------------------------------------
    // Seconds of real focused work, machine-sleep already subtracted. A counter so Prometheus
    // can rate() it into "hours worked per day per project".
    let _ = writeln!(
        out,
        "# HELP hive_active_seconds_total Focused tmux time per session (seconds, sleep-adjusted)."
    );
    let _ = writeln!(out, "# TYPE hive_active_seconds_total counter");
    for entry in &stats.active_by_session {
        let _ = writeln!(
            out,
            "hive_active_seconds_total{{session=\"{}\"}} {}",
            escape_label(&entry.session),
            entry.secs
        );
    }

    let _ = writeln!(
        out,
        "# HELP hive_web_seconds_total Time spent viewing each session in the web dashboard (seconds)."
    );
    let _ = writeln!(out, "# TYPE hive_web_seconds_total counter");
    for entry in &stats.web_by_session {
        let _ = writeln!(
            out,
            "hive_web_seconds_total{{session=\"{}\"}} {}",
            escape_label(&entry.session),
            entry.secs
        );
    }

    // ---- Worktree debt ------------------------------------------------------------------
    // A registered worktree whose tmux session is gone is finished-or-abandoned work still
    // holding a checkout on disk. This is the maintenance backlog that a pruning routine is
    // meant to drive down, so it needs to be visible as a number over time.
    let worktrees = WorktreeState::load();
    let live_sessions = get_current_tmux_session_names();
    let mut by_project: BTreeMap<(String, &'static str), usize> = BTreeMap::new();
    for entry in worktrees.worktrees.values() {
        let state = if live_sessions.contains(&entry.session_name) {
            "live"
        } else {
            "dead"
        };
        *by_project
            .entry((entry.project_key.clone(), state))
            .or_default() += 1;
    }
    let _ = writeln!(
        out,
        "# HELP hive_worktrees Registered worktrees by project and tmux liveness."
    );
    let _ = writeln!(out, "# TYPE hive_worktrees gauge");
    for ((project, state), count) in &by_project {
        let _ = writeln!(
            out,
            "hive_worktrees{{project=\"{}\",state=\"{}\"}} {}",
            escape_label(project),
            state,
            count
        );
    }

    // ---- Project registry ---------------------------------------------------------------
    let registry = ProjectRegistry::load();
    let archived = registry.projects.values().filter(|p| p.archived).count();
    let active = registry.projects.len().saturating_sub(archived);
    let _ = writeln!(
        out,
        "# HELP hive_projects Registered projects by archive state."
    );
    let _ = writeln!(out, "# TYPE hive_projects gauge");
    let _ = writeln!(out, "hive_projects{{state=\"active\"}} {active}");
    let _ = writeln!(out, "hive_projects{{state=\"archived\"}} {archived}");

    // ---- Todos --------------------------------------------------------------------------
    // hive's own per-session task list — a backlog Claude Code has no visibility into.
    let active_todos: usize = crate::common::persistence::load_session_todos()
        .values()
        .map(|v| v.len())
        .sum();
    let done_todos: usize = crate::common::persistence::load_completed_todos()
        .values()
        .map(|v| v.len())
        .sum();
    let _ = writeln!(out, "# HELP hive_todos Per-session todo items by state.");
    let _ = writeln!(out, "# TYPE hive_todos gauge");
    let _ = writeln!(out, "hive_todos{{state=\"active\"}} {active_todos}");
    let _ = writeln!(out, "hive_todos{{state=\"done\"}} {done_todos}");

    // ---- tmux ---------------------------------------------------------------------------
    scalar(
        &mut out,
        "hive_tmux_sessions",
        "Live tmux sessions (hive's internal web session excluded).",
        "gauge",
        live_sessions.len(),
    );

    // ---- Conversation registry ------------------------------------------------------------
    // Everything below comes from the shared snapshot. Omitted entirely when absent — see the
    // note on `render`.
    let Some(snap) = snapshot else {
        return out;
    };

    let _ = writeln!(
        out,
        "# HELP hive_conversations Known Claude conversations by lifecycle."
    );
    let _ = writeln!(out, "# TYPE hive_conversations gauge");
    let _ = writeln!(
        out,
        "hive_conversations{{lifecycle=\"live\"}} {}",
        snap.live
    );
    let _ = writeln!(
        out,
        "hive_conversations{{lifecycle=\"closed\"}} {}",
        snap.closed
    );

    scalar(
        &mut out,
        "hive_conversations_archived",
        "Conversations set aside via archive (still resumable).",
        "gauge",
        snap.archived,
    );
    scalar(
        &mut out,
        "hive_conversations_pinned",
        "Conversations pinned by the user.",
        "gauge",
        snap.pinned,
    );

    // The attention bottleneck: how many running agents are stopped waiting on a human.
    // Sustained non-zero here is the ceiling on how many agents one person can drive.
    scalar(
        &mut out,
        "hive_conversations_blocked",
        "Live conversations stopped awaiting a human decision (permission/plan/question/edit).",
        "gauge",
        snap.blocked,
    );
    scalar(
        &mut out,
        "hive_conversations_needs_attention",
        "Live conversations flagged as needing the user.",
        "gauge",
        snap.needs_attention,
    );

    let _ = writeln!(
        out,
        "# HELP hive_conversations_by_status Live conversations by activity status."
    );
    let _ = writeln!(out, "# TYPE hive_conversations_by_status gauge");
    for (status, count) in &snap.by_status {
        let _ = writeln!(
            out,
            "hive_conversations_by_status{{status=\"{status}\"}} {count}"
        );
    }

    let _ = writeln!(
        out,
        "# HELP hive_conversations_by_project Conversations per project and lifecycle."
    );
    let _ = writeln!(out, "# TYPE hive_conversations_by_project gauge");
    for ((project, lifecycle), count) in &snap.by_project {
        let _ = writeln!(
            out,
            "hive_conversations_by_project{{project=\"{}\",lifecycle=\"{}\"}} {}",
            escape_label(project),
            lifecycle,
            count
        );
    }

    // Which identity the work ran under — personal vs work credentials.
    let _ = writeln!(
        out,
        "# HELP hive_conversations_by_auth_profile Live conversations by Claude auth profile."
    );
    let _ = writeln!(out, "# TYPE hive_conversations_by_auth_profile gauge");
    for (profile, count) in &snap.by_auth {
        let _ = writeln!(
            out,
            "hive_conversations_by_auth_profile{{profile=\"{}\"}} {}",
            escape_label(profile),
            count
        );
    }

    // ---- Local resource cost --------------------------------------------------------------
    // The complement to Claude Code's dollar cost: running 17 agents is bounded by this
    // machine's RAM and CPU long before it's bounded by spend.
    let _ = writeln!(
        out,
        "# HELP hive_claude_cpu_percent CPU percent used by live Claude process trees, per project."
    );
    let _ = writeln!(out, "# TYPE hive_claude_cpu_percent gauge");
    for (project, cpu) in &snap.cpu_by_project {
        let _ = writeln!(
            out,
            "hive_claude_cpu_percent{{project=\"{}\"}} {:.2}",
            escape_label(project),
            cpu
        );
    }
    let _ = writeln!(
        out,
        "# HELP hive_claude_memory_bytes Resident memory of live Claude process trees, per project."
    );
    let _ = writeln!(out, "# TYPE hive_claude_memory_bytes gauge");
    for (project, mem_kb) in &snap.mem_kb_by_project {
        let _ = writeln!(
            out,
            "hive_claude_memory_bytes{{project=\"{}\"}} {}",
            escape_label(project),
            mem_kb * 1024
        );
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::registry::{Conversation, ConversationId, ConversationStatus, Lifecycle};

    #[test]
    fn escapes_reserved_label_characters() {
        assert_eq!(escape_label(r#"a"b"#), r#"a\"b"#);
        assert_eq!(escape_label(r"a\b"), r"a\\b");
        assert_eq!(escape_label("a\nb"), "a\\nb");
    }

    #[test]
    fn leaves_emoji_and_spaces_intact() {
        // Session names are `{emoji} {name}` — the text format is UTF-8, so these pass through.
        assert_eq!(escape_label("🌳 Clear Session"), "🌳 Clear Session");
    }

    #[test]
    fn scalar_emits_help_type_and_sample() {
        let mut out = String::new();
        scalar(&mut out, "hive_x", "An x.", "gauge", 7);
        assert_eq!(out, "# HELP hive_x An x.\n# TYPE hive_x gauge\nhive_x 7\n");
    }

    #[test]
    fn render_emits_wellformed_exposition() {
        // Runs against the real cache dir; content varies by machine, so assert on shape only:
        // every non-comment line must be `name{labels} value` with a parseable numeric value.
        let text = render(None);
        assert!(text.contains("# TYPE hive_windows_open gauge"));
        for line in text
            .lines()
            .filter(|l| !l.starts_with('#') && !l.is_empty())
        {
            let value = line.rsplit(' ').next().expect("sample has a value");
            assert!(
                value.parse::<f64>().is_ok(),
                "non-numeric sample value in line: {line}"
            );
        }
    }

    #[test]
    fn registry_series_are_omitted_without_a_snapshot() {
        // Absent gather must not read as "zero conversations".
        let text = render(None);
        assert!(!text.contains("hive_conversations"));
    }

    fn conv(id: &str, live: bool, parent: Option<&str>) -> Conversation {
        Conversation {
            id: ConversationId::from(id),
            cwd: "/proj".to_string(),
            lifecycle: if live {
                Lifecycle::Live
            } else {
                Lifecycle::Closed
            },
            status: None,
            last_activity: None,
            placement: None,
            parent: parent.map(|s| s.to_string()),
            frozen: None,
            note: String::new(),
            pinned: false,
            archived: false,
            notify_override: false,
            archive_reason: None,
            archived_at: None,
            title: None,
            auth_config_dir: None,
            cpu: 0.0,
            mem_kb: 0,
            ports: Vec::new(),
            pids: Vec::new(),
        }
    }

    fn registry_of(convs: Vec<Conversation>) -> ConversationRegistry {
        let mut reg = ConversationRegistry::default();
        for c in convs {
            reg.conversations.insert(c.id.as_str().to_string(), c);
        }
        reg
    }

    #[test]
    fn snapshot_counts_lifecycle_and_groups_by_project() {
        // A worktree conversation ("project/branch") groups under its PROJECT, not the branch.
        let reg = registry_of(vec![
            conv("a", true, Some("hive")),
            conv("b", false, Some("hive")),
            conv("c", true, Some("clear-session/CSD-1")),
        ]);
        let snap = RegistrySnapshot::from_registry(&reg);

        assert_eq!(snap.live, 2);
        assert_eq!(snap.closed, 1);
        assert_eq!(snap.by_project[&("hive".to_string(), "live")], 1);
        assert_eq!(snap.by_project[&("hive".to_string(), "closed")], 1);
        assert_eq!(snap.by_project[&("clear-session".to_string(), "live")], 1);
    }

    #[test]
    fn snapshot_includes_archived_conversations() {
        // The /api/conversations view filters archived out; the registry snapshot must not,
        // or set-aside work silently vanishes from the backlog count.
        let mut c = conv("a", false, Some("hive"));
        c.archived = true;
        let snap = RegistrySnapshot::from_registry(&registry_of(vec![c]));
        assert_eq!(snap.archived, 1);
        assert_eq!(snap.closed, 1);
    }

    #[test]
    fn snapshot_counts_blocked_separately_from_working() {
        let mut blocked = conv("a", true, Some("hive"));
        blocked.status = Some(ConversationStatus {
            status: SessionStatus::PlanReview,
            needs_attention: true,
        });
        let mut busy = conv("b", true, Some("hive"));
        busy.status = Some(ConversationStatus {
            status: SessionStatus::Working,
            needs_attention: false,
        });
        let snap = RegistrySnapshot::from_registry(&registry_of(vec![blocked, busy]));

        assert_eq!(snap.blocked, 1);
        assert_eq!(snap.needs_attention, 1);
        assert_eq!(snap.by_status["plan_review"], 1);
        assert_eq!(snap.by_status["working"], 1);
    }

    #[test]
    fn closed_conversations_contribute_no_status_or_resources() {
        // Status/CPU on a closed conversation is stale by definition — it must not be summed.
        let mut c = conv("a", false, Some("hive"));
        c.status = Some(ConversationStatus {
            status: SessionStatus::Working,
            needs_attention: true,
        });
        c.cpu = 50.0;
        let snap = RegistrySnapshot::from_registry(&registry_of(vec![c]));

        assert!(snap.by_status.is_empty());
        assert_eq!(snap.needs_attention, 0);
        assert!(snap.cpu_by_project.is_empty());
    }

    #[test]
    fn auth_profile_defaults_when_unset() {
        let mut work = conv("a", true, Some("hive"));
        work.auth_config_dir = Some("/Users/x/.claude-work".to_string());
        let plain = conv("b", true, Some("hive"));
        let snap = RegistrySnapshot::from_registry(&registry_of(vec![work, plain]));

        assert_eq!(snap.by_auth["work"], 1);
        assert_eq!(snap.by_auth["default"], 1);
    }

    #[test]
    fn status_labels_are_payload_free() {
        // Payload-carrying variants must collapse to a fixed label — the payloads are
        // unbounded free text and would explode cardinality.
        assert_eq!(
            status_label(&SessionStatus::NeedsPermission {
                tool_name: "Bash".into(),
                description: None
            }),
            "needs_permission"
        );
        assert_eq!(
            status_label(&SessionStatus::RunningWorkflow {
                summary: "anything at all".into()
            }),
            "running_workflow"
        );
        assert!(is_blocked(&SessionStatus::QuestionAsked));
        assert!(!is_blocked(&SessionStatus::Working));
        assert!(!is_blocked(&SessionStatus::RunningWorkflow {
            summary: String::new()
        }));
    }
}
