//! `hive stats` — usage summary computed from the append-only activity log.
//!
//! Phase 3a covers lifecycle events (open/close/freeze/thaw/kill) → counts and per-day opens.
//! Time-per-session lands in Phase 3b once focus events are logged. The aggregation lives in
//! `common::activity::compute_stats` so this and the web `/api/stats` endpoint agree.

use anyhow::Result;

use crate::common::activity::{
    compute_stats, ActivityEntry, EVENT_BLUR, EVENT_COLLAPSE, EVENT_FOCUS, EVENT_SESSION_KILL,
    EVENT_SKIP, EVENT_SPREAD, EVENT_UNSKIP, EVENT_WEB_BLUR, EVENT_WEB_VIEW, EVENT_WINDOW_CLOSE,
    EVENT_WINDOW_FREEZE, EVENT_WINDOW_OPEN, EVENT_WINDOW_THAW,
};

/// Format a duration in seconds as "1h 23m" / "45m" / "12s".
fn fmt_dur(secs: i64) -> String {
    let m = secs / 60;
    if m >= 60 {
        format!("{}h {}m", m / 60, m % 60)
    } else if m >= 1 {
        format!("{}m", m)
    } else {
        format!("{}s", secs)
    }
}
use crate::common::frozen::relative_time;

/// Print a usage summary over the last `days` days.
pub fn run_stats(days: i64) -> Result<()> {
    let s = compute_stats(days);

    println!("hive usage — last {} day(s)", s.days);
    println!();

    if s.recent_sessions.is_empty() && s.recent_web.is_empty() {
        println!("  No activity logged yet.");
        println!();
        println!("  The log fills as you work (the SessionEnd hook needs `hive setup`).");
        return Ok(());
    }

    println!("  Windows");
    println!("    opened    {:>4}", s.opened);
    println!("    closed    {:>4}   (clean exits)", s.closed);
    println!("    frozen    {:>4}", s.frozen);
    println!("    thawed    {:>4}", s.thawed);
    println!("    killed    {:>4}   (whole-session)", s.killed);
    println!("    open now  {:>4}   (currently tracked)", s.open_now);
    println!(
        "    switches  {:>4}   (focus changes, incl. native tmux)",
        s.switches
    );
    println!(
        "    web views {:>4}   (dashboard, separate stream)",
        s.web_views
    );
    println!();

    if !s.opens_by_day.is_empty() {
        let max = s
            .opens_by_day
            .iter()
            .map(|d| d.count)
            .max()
            .unwrap_or(1)
            .max(1);
        println!("  Windows opened by day");
        for d in &s.opens_by_day {
            let bar = "█".repeat((d.count * 20).div_ceil(max));
            println!("    {}  {bar} {}", d.day, d.count);
        }
        println!();
    }

    if !s.active_by_session.is_empty() {
        println!(
            "  Active time — {} total (tmux focus)",
            fmt_dur(s.active_total_secs)
        );
        if s.slept_secs > 0 {
            println!(
                "    ({} of machine sleep subtracted)",
                fmt_dur(s.slept_secs)
            );
        }
        for st in s.active_by_session.iter().take(8) {
            println!("    {:>7}  {}", fmt_dur(st.secs), st.session);
        }
        println!();
    }
    if !s.web_by_session.is_empty() {
        println!("  Web viewing — {} total", fmt_dur(s.web_total_secs));
        for st in s.web_by_session.iter().take(8) {
            println!("    {:>7}  {}", fmt_dur(st.secs), st.session);
        }
        println!();
    }

    if !s.recent_sessions.is_empty() {
        println!("  Recent");
        for e in &s.recent_sessions {
            print_event(e);
        }
        println!();
    }
    if !s.recent_web.is_empty() {
        println!("  Recent (web viewing)");
        for e in &s.recent_web {
            print_event(e);
        }
    }

    Ok(())
}

/// Print one activity-log line for the Recent sections.
fn print_event(e: &ActivityEntry) {
    let when = relative_time(&e.ts);
    let label = short_event(&e.event);
    let who = e.session.as_deref().unwrap_or("");
    let win = e
        .window
        .as_deref()
        .map(|w| format!(" · {w}"))
        .unwrap_or_default();
    let title = e
        .title
        .as_deref()
        .map(|t| format!("  “{t}”"))
        .unwrap_or_default();
    println!("    {when:>9}  {label:<8} {who}{win}{title}");
}

/// Compact display name for an event.
fn short_event(event: &str) -> &str {
    match event {
        EVENT_WINDOW_OPEN => "open",
        EVENT_WINDOW_CLOSE => "close",
        EVENT_WINDOW_FREEZE => "freeze",
        EVENT_WINDOW_THAW => "thaw",
        EVENT_SESSION_KILL => "kill",
        EVENT_FOCUS => "focus",
        EVENT_BLUR => "blur",
        EVENT_SKIP => "skip",
        EVENT_UNSKIP => "unskip",
        EVENT_SPREAD => "spread",
        EVENT_COLLAPSE => "collapse",
        EVENT_WEB_VIEW => "web view",
        EVENT_WEB_BLUR => "web away",
        other => other,
    }
}
