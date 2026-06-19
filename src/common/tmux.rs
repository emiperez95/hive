//! tmux command helpers.

use crate::common::types::{TmuxPane, TmuxSession, TmuxWindow};
use anyhow::{Context, Result};
use std::collections::HashSet;
use std::process::Command;

/// Get all tmux sessions with their windows and panes
pub fn get_tmux_sessions() -> Result<Vec<TmuxSession>> {
    let output = Command::new("tmux")
        .args(["list-sessions", "-F", "#{session_name}"])
        .output()
        .context("Failed to list tmux sessions")?;

    let session_names = String::from_utf8_lossy(&output.stdout);
    let mut sessions = Vec::new();

    for session_name in session_names.lines() {
        if session_name.is_empty() {
            continue;
        }

        let windows = get_tmux_windows(session_name)?;
        sessions.push(TmuxSession {
            name: session_name.to_string(),
            windows,
        });
    }

    Ok(sessions)
}

/// Get all windows in a tmux session
pub fn get_tmux_windows(session: &str) -> Result<Vec<TmuxWindow>> {
    let output = Command::new("tmux")
        .args([
            "list-windows",
            "-t",
            session,
            "-F",
            "#{window_index}:#{window_name}",
        ])
        .output()
        .context("Failed to list tmux windows")?;

    let window_list = String::from_utf8_lossy(&output.stdout);
    let mut windows = Vec::new();

    for line in window_list.lines() {
        if line.is_empty() {
            continue;
        }

        let parts: Vec<&str> = line.split(':').collect();
        if parts.len() >= 2 {
            let index = parts[0].to_string();
            let name = parts[1..].join(":");
            let panes = get_tmux_panes(session, &index)?;

            windows.push(TmuxWindow { index, name, panes });
        }
    }

    Ok(windows)
}

/// Get all panes in a tmux window
pub fn get_tmux_panes(session: &str, window_index: &str) -> Result<Vec<TmuxPane>> {
    let target = format!("{}:{}", session, window_index);
    let output = Command::new("tmux")
        .args([
            "list-panes",
            "-t",
            &target,
            "-F",
            "#{pane_index}\t#{pane_id}\t#{pane_pid}\t#{pane_current_path}",
        ])
        .output()
        .context("Failed to list tmux panes")?;

    let pane_list = String::from_utf8_lossy(&output.stdout);
    let mut panes = Vec::new();

    for line in pane_list.lines() {
        if line.is_empty() {
            continue;
        }

        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() >= 4 {
            if let Ok(pid) = parts[2].parse::<u32>() {
                panes.push(TmuxPane {
                    index: parts[0].to_string(),
                    id: parts[1].to_string(),
                    pid,
                    cwd: parts[3].to_string(),
                });
            }
        }
    }

    Ok(panes)
}

/// Switch to a tmux session
pub fn switch_to_session(session_name: &str) {
    let _ = Command::new("tmux")
        .args(["switch-client", "-t", session_name])
        .output();
}

/// Select a window within a session (does not switch the attached client).
pub fn select_window(session: &str, window_index: &str) {
    let target = format!("{}:{}", session, window_index);
    let _ = Command::new("tmux")
        .args(["select-window", "-t", &target])
        .output();
}

/// Send a key to a tmux pane
pub fn send_key_to_pane(session: &str, window: &str, pane: &str, key: &str) {
    let target = format!("{}:{}.{}", session, window, pane);
    let _ = Command::new("tmux")
        .args(["send-keys", "-t", &target, key])
        .output();
}

/// Get list of currently running tmux session names
pub fn get_current_tmux_session_names() -> Vec<String> {
    Command::new("tmux")
        .args(["list-sessions", "-F", "#{session_name}"])
        .output()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .filter(|l| !l.is_empty())
                .map(|s| s.to_string())
                .collect()
        })
        .unwrap_or_default()
}

/// Get the current active tmux session name
pub fn get_current_tmux_session() -> Option<String> {
    Command::new("tmux")
        .args(["display-message", "-p", "#{session_name}"])
        .output()
        .ok()
        .and_then(|o| {
            let name = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if name.is_empty() {
                None
            } else {
                Some(name)
            }
        })
}

/// Get the current active tmux window index (matches `WindowView.window_index`).
pub fn get_current_tmux_window() -> Option<String> {
    Command::new("tmux")
        .args(["display-message", "-p", "#{window_index}"])
        .output()
        .ok()
        .and_then(|o| {
            let idx = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if idx.is_empty() {
                None
            } else {
                Some(idx)
            }
        })
}

/// Resolve a format string against a specific pane (e.g. the pane that triggered
/// a key binding, passed in as `#{pane_id}`). The unscoped `display-message`
/// helpers above are unreliable from a `run-shell` child — the child has no
/// `TMUX_PANE`, so tmux falls back to a server-global "current" that may not be
/// the pane the user is actually on. Pass the pane explicitly to avoid that.
pub fn display_message_for_pane(pane_id: &str, format: &str) -> Option<String> {
    Command::new("tmux")
        .args(["display-message", "-t", pane_id, "-p", format])
        .output()
        .ok()
        .and_then(|o| {
            let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if s.is_empty() {
                None
            } else {
                Some(s)
            }
        })
}

/// Get the session name attached to the caller's tmux client.
pub fn get_current_session() -> Option<String> {
    Command::new("tmux")
        .args(["display-message", "-p", "#{client_session}"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Get session names attached to tmux clients other than the caller's.
pub fn get_other_client_sessions() -> HashSet<String> {
    let my_tty = Command::new("tmux")
        .args(["display-message", "-p", "#{client_tty}"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();

    let clients_output = Command::new("tmux")
        .args(["list-clients", "-F", "#{client_tty} #{client_session}"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default();

    let mut other_sessions = HashSet::new();
    for line in clients_output.lines() {
        if let Some((tty, session)) = line.split_once(' ') {
            if !my_tty.is_empty() && tty != my_tty {
                other_sessions.insert(session.to_string());
            }
        }
    }
    other_sessions
}

/// Resolve the absolute path to tmux by searching PATH.
/// Needed for exec() and iTerm split panes which may have minimal PATH.
pub fn resolve_tmux_path() -> String {
    if let Ok(path) = std::env::var("PATH") {
        for dir in path.split(':') {
            let candidate = std::path::PathBuf::from(dir).join("tmux");
            if candidate.exists() {
                return candidate.to_string_lossy().to_string();
            }
        }
    }
    for p in [
        "/opt/homebrew/bin/tmux",
        "/usr/local/bin/tmux",
        "/usr/bin/tmux",
    ] {
        if std::path::Path::new(p).exists() {
            return p.to_string();
        }
    }
    "tmux".to_string()
}

/// Rearrange panes in every tmux session's windows for spread/collapse.
///
/// `mode` is either "spread" or "collapse":
/// - **spread**: stack panes vertically (top-bottom), pane 0 gets 70% height
/// - **collapse**: arrange panes horizontally (side-by-side), pane 0 gets 70% width
///
/// Only handles windows with 2 or 3 panes. Windows with 1 or 4+ panes are left untouched.
/// For 3 panes: pane 0 is the main pane (70%), panes 1-2 split the remaining 30%.
pub fn set_all_sessions_layout(mode: &str) {
    let sessions = get_current_tmux_session_names();

    for session in &sessions {
        if let Ok(output) = Command::new("tmux")
            .args([
                "list-windows",
                "-t",
                session,
                "-F",
                "#{window_index}:#{window_panes}",
            ])
            .output()
        {
            let window_list = String::from_utf8_lossy(&output.stdout);
            for line in window_list.lines() {
                if let Some((idx, count_str)) = line.split_once(':') {
                    let pane_count: usize = count_str.parse().unwrap_or(0);
                    let target = format!("{}:{}", session, idx);
                    match pane_count {
                        2 => layout_2_panes(&target, mode),
                        3 => layout_3_panes(&target, mode),
                        _ => {} // 0-1 or 4+: leave untouched
                    }
                }
            }
        }
    }
}

/// 2 panes: main pane (70%) + secondary pane (30%).
/// spread: top/bottom, collapse: left/right.
fn layout_2_panes(target: &str, mode: &str) {
    let (layout, flag) = if mode == "spread" {
        ("even-vertical", "-y")
    } else {
        ("even-horizontal", "-x")
    };
    let _ = Command::new("tmux")
        .args(["select-layout", "-t", target, layout])
        .output();
    let pane0 = format!("{}.0", target);
    let _ = Command::new("tmux")
        .args(["resize-pane", "-t", &pane0, flag, "70%"])
        .output();
}

/// 3 panes: main pane 0 (70%) + panes 1-2 split in the remaining 30%.
/// spread: pane 0 on top (70% height), panes 1-2 side-by-side below.
/// collapse: pane 0 on left (70% width), panes 1-2 stacked on right.
fn layout_3_panes(target: &str, mode: &str) {
    let layout = if mode == "spread" {
        "main-horizontal"
    } else {
        "main-vertical"
    };
    let _ = Command::new("tmux")
        .args(["select-layout", "-t", target, layout])
        .output();
    // main-horizontal/main-vertical use pane 0 as the main pane by default.
    // Resize it to 70%.
    let pane0 = format!("{}.0", target);
    let flag = if mode == "spread" { "-y" } else { "-x" };
    let _ = Command::new("tmux")
        .args(["resize-pane", "-t", &pane0, flag, "70%"])
        .output();
}

/// Send literal text to a tmux pane followed by Enter
pub fn send_text_to_pane(session: &str, window: &str, pane: &str, text: &str) {
    let target = format!("{}:{}.{}", session, window, pane);
    // Send the text literally (-l flag prevents interpretation of special keys)
    let _ = Command::new("tmux")
        .args(["send-keys", "-t", &target, "-l", text])
        .output();
    // Then send Enter
    let _ = Command::new("tmux")
        .args(["send-keys", "-t", &target, "Enter"])
        .output();
}

/// Kill a tmux session
pub fn kill_tmux_session(name: &str) -> bool {
    Command::new("tmux")
        .args(["kill-session", "-t", name])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Mirror a Claude pane's title into its tmux window name.
///
/// Claude Code writes the conversation title (set by `/rename` or auto-generated) to the
/// pane title, but tmux's `automatic-rename` keeps naming the *window* after the running
/// process (the version-named `claude` binary), so window lists show useless `2.1.x`.
/// This takes the pane title, strips Claude's leading status glyph, and renames the window
/// to match — turning off `automatic-rename` so the name sticks.
///
/// Called from the hook handler, which runs inside the Claude pane (so `pane` is its
/// `$TMUX_PANE`). Event-driven: every hook fire keeps the window name current, no polling.
/// Cheap when nothing changed — a single `display-message` query and an early return if the
/// cleaned title already equals the current window name.
pub fn sync_window_name_for_pane(pane: &str) {
    // One query: the pane's title plus the window it lives in and that window's current name.
    let output = match Command::new("tmux")
        .args([
            "display-message",
            "-p",
            "-t",
            pane,
            "-F",
            "#{pane_title}\t#{window_id}\t#{window_name}",
        ])
        .output()
    {
        Ok(o) if o.status.success() => o,
        _ => return,
    };

    let line = String::from_utf8_lossy(&output.stdout);
    let line = line.trim_end_matches('\n');
    let mut parts = line.splitn(3, '\t');
    let (Some(title), Some(window_id), Some(current_name)) =
        (parts.next(), parts.next(), parts.next())
    else {
        return;
    };

    let clean = clean_claude_title(title);
    // Nothing usable, or already correct — leave tmux alone (no churn from the spinner glyph,
    // which animates every frame but cleans to the same stable text).
    if clean.is_empty() || clean == current_name {
        return;
    }

    // Take ownership of the window name so tmux's automatic-rename doesn't revert it.
    let _ = Command::new("tmux")
        .args([
            "set-window-option",
            "-t",
            window_id,
            "automatic-rename",
            "off",
        ])
        .output();
    let _ = Command::new("tmux")
        .args(["rename-window", "-t", window_id, &clean])
        .output();
}

/// Strip Claude's leading status glyph from a pane title and cap its length, producing a
/// clean tmux window name. Returns empty when there's nothing usable.
///
/// Claude prefixes the title with a status glyph (`✳`, `✻`, or an animating braille spinner
/// frame like `⠂`) followed by a space. The glyph is only stripped when the first character
/// isn't part of a normal word, so a plainly-titled pane is left intact.
pub fn clean_claude_title(title: &str) -> String {
    /// Longest window name we'll set, in characters (keeps the tmux status line tidy).
    const MAX_LEN: usize = 60;

    let trimmed = title.trim();
    let first = match trimmed.chars().next() {
        Some(c) => c,
        None => return String::new(),
    };

    let stripped = if first.is_alphanumeric() {
        trimmed
    } else {
        // Drop the leading glyph token (up to the first whitespace) and the spaces after it.
        // A lone glyph with nothing after it cleans to empty (so we skip the rename).
        trimmed
            .split_once(char::is_whitespace)
            .map(|(_, rest)| rest.trim_start())
            .unwrap_or("")
    };

    stripped.chars().take(MAX_LEN).collect::<String>()
}

#[cfg(test)]
mod tests {
    use super::clean_claude_title;

    #[test]
    fn strips_leading_glyph() {
        assert_eq!(clean_claude_title("✳ Session preview"), "Session preview");
        assert_eq!(clean_claude_title("⠂ Rename tmux"), "Rename tmux");
        assert_eq!(
            clean_claude_title("✻ Cycle to next active"),
            "Cycle to next active"
        );
    }

    #[test]
    fn leaves_plain_titles_intact() {
        assert_eq!(
            clean_claude_title("Refactor session handling"),
            "Refactor session handling"
        );
        assert_eq!(clean_claude_title("  padded title  "), "padded title");
    }

    #[test]
    fn empty_or_glyph_only() {
        assert_eq!(clean_claude_title(""), "");
        assert_eq!(clean_claude_title("   "), "");
        // A lone glyph with no following text strips to empty.
        assert_eq!(clean_claude_title("✳ "), "");
    }

    #[test]
    fn caps_length() {
        let long = format!("✳ {}", "a".repeat(100));
        assert_eq!(clean_claude_title(&long).chars().count(), 60);
    }
}
