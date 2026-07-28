//! Machine power-state (sleep/wake), read from the OS at report time — no daemon.
//!
//! macOS records every sleep/wake transition in `pmset -g log`. Reading it when generating a
//! usage report lets us subtract time the machine was actually asleep from focus intervals,
//! which is far more accurate than the blunt interval cap alone (it catches the closed-lid
//! overnight case precisely). macOS-only; a no-op stub elsewhere (hive's launch scope is mac).

use chrono::{DateTime, Utc};

/// Intervals during which the machine was asleep, ending at or after `since`. Empty on
/// non-macOS or any error — callers degrade to the interval cap.
#[cfg(target_os = "macos")]
pub fn sleep_intervals(since: DateTime<Utc>) -> Vec<(DateTime<Utc>, DateTime<Utc>)> {
    parse_sleep_intervals(&pmset_log(), since)
}

/// `pmset -g log` output, memoized per process.
///
/// The call costs ~1.8s (it renders ~45k lines of OS power log), and every `compute_stats`
/// needs it — so the web `/api/stats` and the scraped `/metrics` endpoint would each pay that
/// in full on every request. Sleep history only ever grows at the tail and old spans never
/// change, so a slightly stale read costs at most `TTL` of accuracy on the newest interval.
///
/// One-shot CLI runs (`hive stats`) see a cold cache and behave exactly as before.
#[cfg(target_os = "macos")]
fn pmset_log() -> String {
    use std::sync::{Mutex, OnceLock};
    use std::time::{Duration, Instant};

    const TTL: Duration = Duration::from_secs(300);
    static CACHE: OnceLock<Mutex<Option<(Instant, String)>>> = OnceLock::new();

    let cache = CACHE.get_or_init(|| Mutex::new(None));

    if let Ok(guard) = cache.lock() {
        if let Some((read_at, text)) = guard.as_ref() {
            if read_at.elapsed() < TTL {
                return text.clone();
            }
        }
    }

    // Only a successful read is cached — a transient failure shouldn't blind us for 5 minutes.
    let out = match std::process::Command::new("pmset")
        .args(["-g", "log"])
        .output()
    {
        Ok(o) if o.status.success() => o.stdout,
        _ => return String::new(),
    };
    let text = String::from_utf8_lossy(&out).into_owned();
    if let Ok(mut guard) = cache.lock() {
        *guard = Some((Instant::now(), text.clone()));
    }
    text
}

#[cfg(not(target_os = "macos"))]
pub fn sleep_intervals(_since: DateTime<Utc>) -> Vec<(DateTime<Utc>, DateTime<Utc>)> {
    Vec::new()
}

/// Parse `pmset -g log` text into asleep spans. State machine: a `Sleep` domain line begins an
/// asleep span; the next `Wake`/`DarkWake` ends it (background maintenance darkwakes are only
/// seconds long, so their tiny awake gaps are negligible). Only spans ending at/after `since`
/// are returned. Exact domain match (splitting on the tab after the fixed-width timestamp) so
/// lookalikes like `Wake Requests` / `WakeTime` are ignored.
#[cfg(any(target_os = "macos", test))]
fn parse_sleep_intervals(text: &str, since: DateTime<Utc>) -> Vec<(DateTime<Utc>, DateTime<Utc>)> {
    let mut spans = Vec::new();
    let mut asleep_since: Option<DateTime<Utc>> = None;
    for line in text.lines() {
        let Some((ts, domain)) = parse_line(line) else {
            continue;
        };
        match domain {
            "Sleep" => {
                if asleep_since.is_none() {
                    asleep_since = Some(ts);
                }
            }
            "Wake" | "DarkWake" => {
                if let Some(start) = asleep_since.take() {
                    if ts > start {
                        spans.push((start, ts));
                    }
                }
            }
            _ => {}
        }
    }
    spans.retain(|(_, end)| *end >= since);
    spans
}

/// Parse one pmset log line → (UTC timestamp, domain). The line is
/// `YYYY-MM-DD HH:MM:SS ±HHMM <Domain padded>\t<details>`; the domain is the field between the
/// 25-char timestamp and the first tab. `None` for non-timestamped / tab-less lines.
#[cfg(any(target_os = "macos", test))]
fn parse_line(line: &str) -> Option<(DateTime<Utc>, &str)> {
    let tab = line.find('\t')?;
    let head = &line[..tab];
    if head.len() < 26 {
        return None;
    }
    let ts = DateTime::parse_from_str(&head[..25], "%Y-%m-%d %H:%M:%S %z")
        .ok()?
        .with_timezone(&Utc);
    Some((ts, head[25..].trim()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn parses_sleep_spans_and_ignores_lookalikes() {
        // Real pmset shape: tab between the padded domain and the details.
        let log = "\
2026-07-02 09:00:00 -0300 Wake                \tDarkWake to FullWake due to UserActivity
2026-07-02 09:20:00 -0300 Sleep               \tEntering Sleep state due to 'Maintenance Sleep'
2026-07-02 09:20:05 -0300 Wake Requests       \t[process=dasd ...]
2026-07-02 09:20:10 -0300 WakeTime            \tWakeTime metadata
2026-07-02 09:50:00 -0300 DarkWake            \tDarkWake from Deep Idle
2026-07-02 10:00:00 -0300 Sleep               \tEntering Sleep state
2026-07-02 10:30:00 -0300 Wake                \tFullWake due to UserActivity
not a log line
";
        let spans = parse_sleep_intervals(log, ts("2026-07-02T00:00:00-03:00"));
        // Two spans: 09:20→09:50 (30m, ended by DarkWake) and 10:00→10:30 (30m). The
        // `Wake Requests` / `WakeTime` lines in between must NOT end the first span early.
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].0, ts("2026-07-02T09:20:00-03:00"));
        assert_eq!(spans[0].1, ts("2026-07-02T09:50:00-03:00"));
        assert_eq!(spans[1].0, ts("2026-07-02T10:00:00-03:00"));
        assert_eq!(spans[1].1, ts("2026-07-02T10:30:00-03:00"));
    }

    #[test]
    fn drops_spans_before_the_window() {
        let log = "\
2026-07-01 22:00:00 -0300 Sleep               \tsleep
2026-07-01 22:30:00 -0300 Wake                \twake
";
        let spans = parse_sleep_intervals(log, ts("2026-07-02T00:00:00-03:00"));
        assert!(spans.is_empty());
    }
}
