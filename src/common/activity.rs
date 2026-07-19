//! Session activity log — the instrumentation layer behind recovery and usage metrics.
//!
//! A single `record_*` / `log_*` layer feeds two projections (see
//! `docs/session-activity-log.md`): the mutable `open-windows.json` snapshot of what's open
//! right now, and the append-only `activity.jsonl` history of discrete lifecycle/focus events
//! for metrics. The two are defined in their own sections below. The snapshot is what lets hive
//! show "here's what you had open" after a machine restart wipes every tmux session — the
//! conversation JSONL survives on disk, but *which* conversations were open, grouped into
//! which sessions, and their **titles** (which live only on the live tmux pane title) do not.
//!
//! The snapshot is a write-through current-state view, not a replay of events: each Claude
//! hook upserts the window's latest title + `last_seen`, and hive removes a window when it
//! cleanly closes, is frozen (it moves to `frozen.json`), or its session is killed. A crash
//! fires no removal, so a crashed window simply lingers — which is exactly the recovery
//! candidate we want. Entries are pruned by age so a never-recovered window doesn't linger
//! forever.
//!
//! Entries deliberately mirror [`crate::common::frozen::FrozenEntry`] so that Phase 2 restore
//! can reuse the same recreate-session + `claude --resume` path.

use anyhow::{anyhow, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

use crate::common::persistence::cache_dir;

/// Drop snapshot entries whose `last_seen` is older than this many days. Bounds the file so a
/// window that crashed and was never recovered eventually ages out of the recovery list.
const PRUNE_AFTER_DAYS: i64 = 7;

// ─── Types ───────────────────────────────────────────────────────────────────

/// One Claude window that is (or was, until a crash) open. Keyed in the snapshot by its
/// Claude `session_id`, which the hook always has.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OpenWindow {
    /// Parent tmux/display session name (e.g. `🐝 hive`).
    pub session_name: String,
    /// tmux window name — mirrors the Claude conversation title once one is set.
    #[serde(default)]
    pub window_name: String,
    /// tmux window index (string, as tmux reports it).
    #[serde(default)]
    pub window_index: String,
    /// Working directory of the window.
    pub cwd: String,
    /// Claude conversation id (the `<session_id>.jsonl` basename) — used to `--resume`.
    pub claude_session_id: String,
    /// `CLAUDE_CONFIG_DIR` (auth profile) the window runs under, if any.
    #[serde(default)]
    pub claude_config_dir: Option<String>,
    /// RFC3339 timestamp of when this window was first recorded.
    pub first_seen: String,
    /// RFC3339 timestamp of the most recent hook that touched this window.
    pub last_seen: String,
}

/// The fields a hook fire knows about a window — input to [`OpenWindowsState::upsert`].
#[derive(Debug, Clone)]
pub struct WindowSeen {
    pub claude_session_id: String,
    pub session_name: String,
    pub window_index: String,
    pub window_name: String,
    pub cwd: String,
    pub claude_config_dir: Option<String>,
}

/// Top-level snapshot file, keyed by Claude `session_id`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OpenWindowsState {
    #[serde(default)]
    pub windows: HashMap<String, OpenWindow>,
}

impl OpenWindowsState {
    fn file_path() -> Option<PathBuf> {
        cache_dir().map(|p| p.join("open-windows.json"))
    }

    /// Load the snapshot from disk. Returns empty state on any error.
    pub fn load() -> Self {
        let Some(path) = Self::file_path() else {
            return Self::default();
        };
        let Ok(content) = std::fs::read_to_string(&path) else {
            return Self::default();
        };
        serde_json::from_str(&content).unwrap_or_else(|e| {
            eprintln!("Warning: failed to parse {}: {}", path.display(), e);
            Self::default()
        })
    }

    /// Save the snapshot atomically (write .tmp, rename).
    pub fn save(&self) -> Result<()> {
        let path = Self::file_path().ok_or_else(|| anyhow!("Cannot determine cache directory"))?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let content = serde_json::to_string_pretty(self)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, &content)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    /// Insert or update a window from a hook fire. `first_seen` is set once; `last_seen`,
    /// `window_name` (latest title), cwd, and config dir are refreshed every time.
    pub fn upsert(&mut self, seen: &WindowSeen, now: DateTime<Utc>) {
        let now_str = now.to_rfc3339();
        self.windows
            .entry(seen.claude_session_id.clone())
            .and_modify(|w| {
                w.session_name = seen.session_name.clone();
                w.window_index = seen.window_index.clone();
                w.window_name = seen.window_name.clone();
                w.cwd = seen.cwd.clone();
                w.claude_config_dir = seen.claude_config_dir.clone();
                w.last_seen = now_str.clone();
            })
            .or_insert_with(|| OpenWindow {
                session_name: seen.session_name.clone(),
                window_name: seen.window_name.clone(),
                window_index: seen.window_index.clone(),
                cwd: seen.cwd.clone(),
                claude_session_id: seen.claude_session_id.clone(),
                claude_config_dir: seen.claude_config_dir.clone(),
                first_seen: now_str.clone(),
                last_seen: now_str,
            });
    }

    /// Remove a window by its Claude session id. Returns whether anything was removed.
    pub fn remove(&mut self, claude_session_id: &str) -> bool {
        self.windows.remove(claude_session_id).is_some()
    }

    /// Remove every window belonging to a session (e.g. on kill-session). Returns whether
    /// anything was removed.
    pub fn remove_for_session(&mut self, session_name: &str) -> bool {
        let before = self.windows.len();
        self.windows.retain(|_, w| w.session_name != session_name);
        self.windows.len() != before
    }

    /// Drop entries older than [`PRUNE_AFTER_DAYS`] by `last_seen`. An unparseable timestamp
    /// is treated as stale and dropped.
    pub fn prune(&mut self, now: DateTime<Utc>) {
        self.windows.retain(|_, w| {
            DateTime::parse_from_rfc3339(&w.last_seen)
                .map(|t| (now - t.with_timezone(&Utc)).num_days() < PRUNE_AFTER_DAYS)
                .unwrap_or(false)
        });
    }

    /// Entries sorted newest-`last_seen` first, for display.
    #[allow(dead_code)] // retained + tested; no live caller since classic went
    pub fn sorted(&self) -> Vec<&OpenWindow> {
        let mut entries: Vec<&OpenWindow> = self.windows.values().collect();
        entries.sort_by(|a, b| b.last_seen.cmp(&a.last_seen));
        entries
    }
}

// ─── Recording (the instrumentation layer) ────────────────────────────────────

/// Record that a Claude window was seen by a hook fire: upsert it into the snapshot, prune
/// stale entries, and persist. Best-effort — failures are swallowed so a hook never breaks.
pub fn record_window_seen(seen: &WindowSeen) {
    if seen.claude_session_id.is_empty() || seen.claude_session_id == "unknown" {
        return;
    }
    let mut state = OpenWindowsState::load();
    // A sid absent from the snapshot is a window we haven't seen before → append an
    // `window_open` lifecycle event. (After a fresh install the snapshot is empty, so the
    // first hook of an already-running session logs a one-off open — harmless.)
    let is_new = !state.windows.contains_key(&seen.claude_session_id);
    let now = Utc::now();
    state.upsert(seen, now);
    state.prune(now);
    let _ = state.save();
    if is_new {
        append_event(&ActivityEntry::new(EVENT_WINDOW_OPEN, now).with(
            &seen.claude_session_id,
            &seen.session_name,
            &seen.window_index,
            &seen.window_name,
        ));
    }
}

/// Remove a window from the snapshot by Claude session id (clean close / freeze).
pub fn remove_window(claude_session_id: &str) {
    let mut state = OpenWindowsState::load();
    if state.remove(claude_session_id) {
        let _ = state.save();
    }
}

/// Remove every window of a session from the snapshot (kill-session).
pub fn remove_windows_for_session(session_name: &str) {
    let mut state = OpenWindowsState::load();
    if state.remove_for_session(session_name) {
        let _ = state.save();
    }
}

// ─── Append-only activity log (metrics) ───────────────────────────────────────
//
// The second projection: an immutable, append-only history of discrete lifecycle and focus
// events. Where the snapshot answers "what's open now", this answers "what happened, when"
// — the basis for usage metrics (session counts, freeze/skip frequency, time per session).
//
// Only discrete, human-paced events are appended. The per-hook `window_seen` heartbeat is NOT
// logged (it would bloat the file); it only bumps the snapshot. Each event is one JSON line;
// lines stay well under `PIPE_BUF` (4096B) so `O_APPEND` writes from concurrent hook
// processes are atomic on POSIX without locking.

/// Lifecycle: a Claude window first appeared.
pub const EVENT_WINDOW_OPEN: &str = "window_open";
/// Lifecycle: a window ended cleanly (SessionEnd hook).
pub const EVENT_WINDOW_CLOSE: &str = "window_close";
/// Lifecycle: a window was frozen (moved to frozen.json).
pub const EVENT_WINDOW_FREEZE: &str = "window_freeze";
/// Lifecycle: a frozen window was thawed.
pub const EVENT_WINDOW_THAW: &str = "window_thaw";
/// Lifecycle: a whole session was killed.
pub const EVENT_SESSION_KILL: &str = "session_kill";
/// Focus: hive switched the active session/window (cycle/window commands). Note: only
/// captures switches made *through* hive's commands — native tmux switching (`prefix s`,
/// `prefix n`, clicking) is invisible here. Full coverage needs tmux focus hooks (Phase 3b).
pub const EVENT_FOCUS: &str = "focus";
/// Web: the browser is viewing a session (or the list, when `session` is absent). A *separate*
/// stream from tmux [`EVENT_FOCUS`] — remote viewing, not local tmux focus.
pub const EVENT_WEB_VIEW: &str = "web_view";
/// Web: the browser lost focus (tab hidden) or closed — stop counting web viewing time.
pub const EVENT_WEB_BLUR: &str = "web_blur";
/// tmux: the client stopped looking at tmux — detached (iTerm closed, `prefix+d`) or the
/// terminal lost OS focus (`client-focus-out`). Bounds a focus interval. From tmux hooks.
pub const EVENT_BLUR: &str = "blur";
/// A session was skipped / unskipped (set aside from cycling).
pub const EVENT_SKIP: &str = "skip";
pub const EVENT_UNSKIP: &str = "unskip";
/// iTerm panes spread into N / collapsed back to one.
pub const EVENT_SPREAD: &str = "spread";
pub const EVENT_COLLAPSE: &str = "collapse";

/// One line in `activity.jsonl`. Flat and permissive: unknown/optional fields are omitted on
/// write and tolerated on read, so the log survives format evolution across versions.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ActivityEntry {
    /// RFC3339 timestamp.
    pub ts: String,
    /// Event name — one of the `EVENT_*` constants (or a focus event added in Phase 3b).
    pub event: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sid: Option<String>,
    /// tmux client (focus events, Phase 3b).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client: Option<String>,
    /// Window title at the time (open events).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

impl ActivityEntry {
    /// A bare event stamped with `now`.
    pub fn new(event: &str, now: DateTime<Utc>) -> Self {
        Self {
            ts: now.to_rfc3339(),
            event: event.to_string(),
            session: None,
            window: None,
            sid: None,
            client: None,
            title: None,
        }
    }

    /// Attach window identity (sid/session/window/title). Empty strings become `None`.
    fn with(mut self, sid: &str, session: &str, window: &str, title: &str) -> Self {
        self.sid = non_empty(sid);
        self.session = non_empty(session);
        self.window = non_empty(window);
        self.title = non_empty(title);
        self
    }
}

fn non_empty(s: &str) -> Option<String> {
    (!s.is_empty()).then(|| s.to_string())
}

fn log_file_path() -> Option<PathBuf> {
    cache_dir().map(|p| p.join("activity.jsonl"))
}

/// Append one event as a JSON line. Best-effort — swallows errors so it never breaks a hook.
pub fn append_event(entry: &ActivityEntry) {
    use std::io::Write;
    let Some(path) = log_file_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let Ok(line) = serde_json::to_string(entry) else {
        return;
    };
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = writeln!(f, "{line}");
    }
}

/// Log a clean window close (SessionEnd).
pub fn log_window_close(sid: &str, session: &str) {
    append_event(&ActivityEntry::new(EVENT_WINDOW_CLOSE, Utc::now()).with(sid, session, "", ""));
}

/// Log a window freeze.
pub fn log_window_freeze(sid: &str, session: &str) {
    append_event(&ActivityEntry::new(EVENT_WINDOW_FREEZE, Utc::now()).with(sid, session, "", ""));
}

/// Log a frozen-window thaw.
pub fn log_window_thaw(sid: &str, session: &str) {
    append_event(&ActivityEntry::new(EVENT_WINDOW_THAW, Utc::now()).with(sid, session, "", ""));
}

/// Log a whole-session kill.
pub fn log_session_kill(session: &str) {
    append_event(&ActivityEntry::new(EVENT_SESSION_KILL, Utc::now()).with("", session, "", ""));
}

/// Log a hive-initiated focus change to `session` (and `window` index, when known).
pub fn log_focus(session: &str, window: Option<&str>) {
    append_event(&ActivityEntry::new(EVENT_FOCUS, Utc::now()).with(
        "",
        session,
        window.unwrap_or(""),
        "",
    ));
}

/// Log that the web dashboard is viewing `session` (None = the session list). Separate stream
/// from tmux [`log_focus`] — see [`EVENT_WEB_VIEW`].
pub fn log_web_view(session: Option<&str>) {
    append_event(&ActivityEntry::new(EVENT_WEB_VIEW, Utc::now()).with(
        "",
        session.unwrap_or(""),
        "",
        "",
    ));
}

/// Log that the web dashboard lost focus or closed (stop counting web viewing time).
pub fn log_web_blur() {
    append_event(&ActivityEntry::new(EVENT_WEB_BLUR, Utc::now()));
}

/// Log that the tmux client stopped looking at tmux (detach / terminal focus-out).
pub fn log_blur() {
    append_event(&ActivityEntry::new(EVENT_BLUR, Utc::now()));
}

/// Log a skip / unskip toggle for a session.
pub fn log_skip(session: &str, skipped: bool) {
    let event = if skipped { EVENT_SKIP } else { EVENT_UNSKIP };
    append_event(&ActivityEntry::new(event, Utc::now()).with("", session, "", ""));
}

/// Log an iTerm spread (into `count` panes) or collapse.
pub fn log_spread(count: usize) {
    append_event(&ActivityEntry::new(EVENT_SPREAD, Utc::now()).with(
        "",
        &format!("{count}"),
        "",
        "",
    ));
}

pub fn log_collapse() {
    append_event(&ActivityEntry::new(EVENT_COLLAPSE, Utc::now()));
}

/// Read the whole activity log, skipping any unparseable lines. Oldest-first (file order).
pub fn load_activity_entries() -> Vec<ActivityEntry> {
    let Some(path) = log_file_path() else {
        return Vec::new();
    };
    let Ok(content) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<ActivityEntry>(l).ok())
        .collect()
}

/// One day's window-open tally, for the stats bar chart.
#[derive(Debug, Clone, Serialize)]
pub struct DayCount {
    pub day: String,
    pub count: usize,
}

/// Aggregated usage over a window of days — shared by `hive stats` and the web `/api/stats`
/// endpoint so both surfaces compute identically. Phase 3a fields (lifecycle counts); focus
/// time is added in Phase 3b.
#[derive(Debug, Clone, Serialize)]
pub struct StatsSummary {
    pub days: i64,
    pub opened: usize,
    pub closed: usize,
    pub frozen: usize,
    pub thawed: usize,
    pub killed: usize,
    /// Hive-initiated session/window switches (cycle & window commands). Partial — see
    /// [`EVENT_FOCUS`].
    pub switches: usize,
    /// Web dashboard session views (separate stream from tmux focus — see [`EVENT_WEB_VIEW`]).
    pub web_views: usize,
    /// Distinct sessions viewed in the web dashboard.
    pub web_sessions: usize,
    /// Windows currently tracked as open (from the snapshot, not the log).
    pub open_now: usize,
    /// Window opens per calendar day (UTC), oldest first.
    pub opens_by_day: Vec<DayCount>,
    /// Most recent tmux/lifecycle events (open/close/freeze/thaw/kill/switch), newest first.
    pub recent_sessions: Vec<ActivityEntry>,
    /// Most recent web-viewing events (web_view/web_blur), newest first.
    pub recent_web: Vec<ActivityEntry>,
    /// Active time per session from the tmux focus stream (seconds, desc). Crash/sleep-robust
    /// via per-interval caps — see [`accumulate_time`].
    pub active_by_session: Vec<SessionTime>,
    /// Total active tmux time (seconds).
    pub active_total_secs: i64,
    /// Time per session viewed in the web dashboard (seconds, desc).
    pub web_by_session: Vec<SessionTime>,
    /// Total web viewing time (seconds).
    pub web_total_secs: i64,
    /// Machine sleep time within the window (seconds), subtracted from active time above.
    pub slept_secs: i64,
}

/// Backstop cap on a single focus/view interval *after* machine-sleep is subtracted. Actual
/// sleep comes from the OS log (see [`crate::common::machine`]); this only bounds the residue —
/// a hard shutdown/crash (not logged as sleep) or an awake-but-idle-focused stretch. Generous
/// (2h) because sleep, the main cause of huge gaps, is now removed precisely rather than capped.
const INTERVAL_CAP_SECS: i64 = 2 * 60 * 60;

/// Seconds of `[a_start, a_end)` that overlap `[b_start, b_end)`.
fn overlap_secs(
    a_start: DateTime<Utc>,
    a_end: DateTime<Utc>,
    b_start: DateTime<Utc>,
    b_end: DateTime<Utc>,
) -> i64 {
    let start = a_start.max(b_start);
    let end = a_end.min(b_end);
    (end - start).num_seconds().max(0)
}

/// Active seconds for one session.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SessionTime {
    pub session: String,
    pub secs: i64,
}

/// Sum active time per session from an ordered (oldest-first) event slice: each `focus_ev`
/// (which carries the session) opens an interval closed by the next focus or a `blur_ev`; a
/// still-open interval is closed at `now`. From each interval we subtract the machine-sleep
/// time that fell inside it (`sleeps`, from the OS log) and then clamp the remainder to
/// [`INTERVAL_CAP_SECS`]. Returns (per-session desc, total secs).
fn accumulate_time(
    events: &[ActivityEntry],
    focus_ev: &str,
    blur_ev: &str,
    sleeps: &[(DateTime<Utc>, DateTime<Utc>)],
    now: DateTime<Utc>,
) -> (Vec<SessionTime>, i64) {
    let mut totals: HashMap<String, i64> = HashMap::new();
    let mut cur: Option<(String, DateTime<Utc>)> = None;
    let close = |totals: &mut HashMap<String, i64>,
                 cur: Option<(String, DateTime<Utc>)>,
                 at: DateTime<Utc>| {
        if let Some((sess, start)) = cur {
            let raw = (at - start).num_seconds();
            let slept: i64 = sleeps
                .iter()
                .map(|(ss, se)| overlap_secs(start, at, *ss, *se))
                .sum();
            let dur = (raw - slept).clamp(0, INTERVAL_CAP_SECS);
            *totals.entry(sess).or_default() += dur;
        }
    };
    for e in events {
        let is_focus = e.event == focus_ev;
        if !is_focus && e.event != blur_ev {
            continue;
        }
        let Some(t) = DateTime::parse_from_rfc3339(&e.ts)
            .ok()
            .map(|x| x.with_timezone(&Utc))
        else {
            continue;
        };
        close(&mut totals, cur.take(), t);
        if is_focus {
            if let Some(sess) = e.session.clone() {
                cur = Some((sess, t));
            }
        }
    }
    close(&mut totals, cur.take(), now);

    let total: i64 = totals.values().sum();
    let mut per: Vec<SessionTime> = totals
        .into_iter()
        .map(|(session, secs)| SessionTime { session, secs })
        .collect();
    per.sort_by(|a, b| b.secs.cmp(&a.secs));
    (per, total)
}

/// Compute a usage summary over the last `days` days (clamped to ≥ 1).
pub fn compute_stats(days: i64) -> StatsSummary {
    use std::collections::BTreeMap;
    let days = days.max(1);
    let cutoff = Utc::now() - chrono::Duration::days(days);

    let recent_window: Vec<ActivityEntry> = load_activity_entries()
        .into_iter()
        .filter(|e| {
            DateTime::parse_from_rfc3339(&e.ts)
                .map(|t| t.with_timezone(&Utc) >= cutoff)
                .unwrap_or(false)
        })
        .collect();

    let count = |name: &str| recent_window.iter().filter(|e| e.event == name).count();

    let mut by_day: BTreeMap<String, usize> = BTreeMap::new();
    for e in recent_window
        .iter()
        .filter(|e| e.event == EVENT_WINDOW_OPEN)
    {
        if let Ok(t) = DateTime::parse_from_rfc3339(&e.ts) {
            let day = t.with_timezone(&Utc).format("%Y-%m-%d").to_string();
            *by_day.entry(day).or_default() += 1;
        }
    }

    let is_web = |e: &&ActivityEntry| e.event == EVENT_WEB_VIEW || e.event == EVENT_WEB_BLUR;
    let recent_sessions: Vec<ActivityEntry> = recent_window
        .iter()
        .rev()
        .filter(|e| !is_web(e))
        .take(20)
        .cloned()
        .collect();
    let recent_web: Vec<ActivityEntry> = recent_window
        .iter()
        .rev()
        .filter(is_web)
        .take(20)
        .cloned()
        .collect();

    // Distinct sessions viewed in the web dashboard.
    let web_sessions = recent_window
        .iter()
        .filter(|e| e.event == EVENT_WEB_VIEW)
        .filter_map(|e| e.session.as_deref())
        .collect::<std::collections::BTreeSet<_>>()
        .len();

    // Active time per session. Subtract actual machine-sleep (from the OS log, read once) so
    // closed-lid time isn't counted; the interval cap is only a backstop for the rest.
    let now = Utc::now();
    let sleeps = crate::common::machine::sleep_intervals(cutoff);
    let slept_secs: i64 = sleeps
        .iter()
        .map(|(s, e)| overlap_secs(*s, *e, cutoff, now))
        .sum();
    let (active_by_session, active_total_secs) =
        accumulate_time(&recent_window, EVENT_FOCUS, EVENT_BLUR, &sleeps, now);
    let (web_by_session, web_total_secs) =
        accumulate_time(&recent_window, EVENT_WEB_VIEW, EVENT_WEB_BLUR, &sleeps, now);

    StatsSummary {
        days,
        opened: count(EVENT_WINDOW_OPEN),
        closed: count(EVENT_WINDOW_CLOSE),
        frozen: count(EVENT_WINDOW_FREEZE),
        thawed: count(EVENT_WINDOW_THAW),
        killed: count(EVENT_SESSION_KILL),
        switches: count(EVENT_FOCUS),
        web_views: count(EVENT_WEB_VIEW),
        web_sessions,
        open_now: OpenWindowsState::load().windows.len(),
        opens_by_day: by_day
            .into_iter()
            .map(|(day, count)| DayCount { day, count })
            .collect(),
        recent_sessions,
        recent_web,
        active_by_session,
        active_total_secs,
        web_by_session,
        web_total_secs,
        slept_secs,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seen(sid: &str, session: &str, idx: &str, title: &str) -> WindowSeen {
        WindowSeen {
            claude_session_id: sid.to_string(),
            session_name: session.to_string(),
            window_index: idx.to_string(),
            window_name: title.to_string(),
            cwd: "/proj".to_string(),
            claude_config_dir: None,
        }
    }

    fn ts(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn upsert_sets_first_seen_once_and_bumps_last_seen_and_title() {
        let mut state = OpenWindowsState::default();
        state.upsert(
            &seen("sid-1", "🐝 hive", "1", ""),
            ts("2026-06-30T10:00:00Z"),
        );
        state.upsert(
            &seen("sid-1", "🐝 hive", "1", "Plan activity log"),
            ts("2026-06-30T11:00:00Z"),
        );

        let w = &state.windows["sid-1"];
        assert_eq!(w.first_seen, "2026-06-30T10:00:00+00:00");
        assert_eq!(w.last_seen, "2026-06-30T11:00:00+00:00");
        // The latest title wins.
        assert_eq!(w.window_name, "Plan activity log");
        assert_eq!(state.windows.len(), 1);
    }

    #[test]
    fn remove_and_remove_for_session() {
        let mut state = OpenWindowsState::default();
        state.upsert(&seen("a", "🐝 hive", "1", "x"), ts("2026-06-30T10:00:00Z"));
        state.upsert(&seen("b", "🐝 hive", "2", "y"), ts("2026-06-30T10:00:00Z"));
        state.upsert(&seen("c", "🌳 Clear", "1", "z"), ts("2026-06-30T10:00:00Z"));

        assert!(state.remove("a"));
        assert!(!state.remove("a")); // already gone
        assert_eq!(state.windows.len(), 2);

        assert!(state.remove_for_session("🐝 hive")); // removes b, leaves c
        assert_eq!(state.windows.len(), 1);
        assert!(state.windows.contains_key("c"));
    }

    #[test]
    fn prune_drops_old_and_unparseable() {
        let mut state = OpenWindowsState::default();
        state.upsert(&seen("recent", "s", "1", ""), ts("2026-06-30T10:00:00Z"));
        state.upsert(&seen("old", "s", "2", ""), ts("2026-06-20T10:00:00Z"));
        state.windows.get_mut("old").unwrap().last_seen = "garbage".to_string();

        state.prune(ts("2026-06-30T12:00:00Z"));
        assert!(state.windows.contains_key("recent"));
        assert!(!state.windows.contains_key("old"));
    }

    #[test]
    fn sorted_is_newest_last_seen_first() {
        let mut state = OpenWindowsState::default();
        state.upsert(&seen("a", "🐝 hive", "1", ""), ts("2026-06-30T10:00:00Z"));
        state.upsert(&seen("b", "🌳 Clear", "1", ""), ts("2026-06-30T11:00:00Z"));
        state.upsert(&seen("c", "🦀 rust", "1", ""), ts("2026-06-30T09:00:00Z"));

        let ids: Vec<&str> = state
            .sorted()
            .iter()
            .map(|w| w.claude_session_id.as_str())
            .collect();
        assert_eq!(ids, vec!["b", "a", "c"]);
    }

    #[test]
    fn roundtrip_serialization() {
        let mut state = OpenWindowsState::default();
        state.upsert(
            &seen("a", "🐝 hive", "1", "title"),
            ts("2026-06-30T10:00:00Z"),
        );
        let json = serde_json::to_string(&state).unwrap();
        let back: OpenWindowsState = serde_json::from_str(&json).unwrap();
        assert_eq!(back.windows, state.windows);
    }

    fn ev(event: &str, session: Option<&str>, at: &str) -> ActivityEntry {
        ActivityEntry {
            ts: at.to_string(),
            event: event.to_string(),
            session: session.map(|s| s.to_string()),
            window: None,
            sid: None,
            client: None,
            title: None,
        }
    }

    #[test]
    fn accumulate_time_attributes_switches_blur_and_caps() {
        let now = ts("2026-07-02T12:00:00Z");
        let events = vec![
            ev(EVENT_FOCUS, Some("A"), "2026-07-02T10:00:00Z"), // A opens
            ev(EVENT_FOCUS, Some("B"), "2026-07-02T10:05:00Z"), // switch → A gets 5m
            ev(EVENT_BLUR, None, "2026-07-02T10:07:00Z"),       // blur → B gets 2m, stop
            ev(EVENT_FOCUS, Some("A"), "2026-07-02T11:00:00Z"), // A again, no close → 60m capped to 30m
        ];
        let (per, total) = accumulate_time(&events, EVENT_FOCUS, EVENT_BLUR, &[], now);
        let secs = |s: &str| per.iter().find(|x| x.session == s).unwrap().secs;
        assert_eq!(secs("A"), 5 * 60 + 60 * 60); // 5m + the 60m open interval (under 2h cap)
        assert_eq!(secs("B"), 2 * 60);
        assert_eq!(total, secs("A") + secs("B"));
        assert_eq!(per[0].session, "A"); // sorted desc
    }

    #[test]
    fn accumulate_time_subtracts_machine_sleep() {
        let now = ts("2026-07-02T12:00:00Z");
        // One focus interval 10:00 → now(12:00) = 120m raw; machine slept 10:30–11:30 (60m).
        let events = vec![ev(EVENT_FOCUS, Some("A"), "2026-07-02T10:00:00Z")];
        let sleeps = vec![(ts("2026-07-02T10:30:00Z"), ts("2026-07-02T11:30:00Z"))];
        let (per, total) = accumulate_time(&events, EVENT_FOCUS, EVENT_BLUR, &sleeps, now);
        assert_eq!(per[0].secs, 60 * 60); // 120m − 60m slept = 60m
        assert_eq!(total, 60 * 60);
    }
}
