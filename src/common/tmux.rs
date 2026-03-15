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
            "#{pane_index}\t#{pane_pid}\t#{pane_current_path}",
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
        if parts.len() >= 3 {
            if let Ok(pid) = parts[1].parse::<u32>() {
                panes.push(TmuxPane {
                    index: parts[0].to_string(),
                    pid,
                    cwd: parts[2].to_string(),
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

/// Kill a tmux session
pub fn kill_tmux_session(name: &str) -> bool {
    Command::new("tmux")
        .args(["kill-session", "-t", name])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}
