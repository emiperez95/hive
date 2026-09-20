//! tmux command helpers.

use crate::common::config::WEB_SESSION;
use crate::common::types::{TmuxPane, TmuxSession, TmuxWindow};
use anyhow::{Context, Result};
use std::collections::HashSet;
use std::process::Command;

/// Exact-match tmux target for a session name.
///
/// tmux resolves `-t <name>` in three steps: exact match, then fnmatch pattern, then
/// **prefix**. So `-t "📊 Avateen"` silently resolves to `📊 Avateen Hub` whenever the
/// former isn't running — new windows land in the wrong session, `switch-client` jumps
/// to it, and `kill-session` kills it. A leading `=` forces a literal exact match; it
/// also stops the `[project]` in a worktree session name being read as an fnmatch
/// character class.
///
/// Every session-NAME target must go through this. Pane (`%12`) and window (`@34`) ids
/// are already unambiguous — never wrap those.
pub fn exact(session: &str) -> String {
    format!("={session}")
}

/// Exact-match target for a window within a session (`=session:window`).
pub fn exact_window(session: &str, window: &str) -> String {
    format!("={session}:{window}")
}

/// Exact-match target for a pane within a session (`=session:window.pane`).
pub fn exact_pane(session: &str, window: &str, pane: &str) -> String {
    format!("={session}:{window}.{pane}")
}

/// Exact-match target for the ACTIVE PANE of a session's current window
/// (`=session:` — empty window means "current", empty pane means "active").
///
/// [`exact`] is a session target and is **not** a valid pane target: `send-keys -t
/// "=name"` fails outright with `can't find pane: =name`, so the command is never
/// typed. That silently broke every "open a window and type a command into it" path
/// (resume a conversation, thaw, new conversation, startup commands) — the window
/// appeared, running a bare shell.
///
/// The trailing `:` is what makes it a pane target while keeping the `=` exactness:
/// `=foo:` still refuses to match a live `foo bar` (verified in an isolated server).
/// Use this for every `send-keys` whose target is a session NAME.
pub fn exact_active_pane(session: &str) -> String {
    format!("={session}:")
}

/// Get all tmux sessions with their windows and panes
pub fn get_tmux_sessions() -> Result<Vec<TmuxSession>> {
    let output = Command::new("tmux")
        .args(["list-sessions", "-F", "#{session_name}"])
        .output()
        .context("Failed to list tmux sessions")?;

    let session_names = String::from_utf8_lossy(&output.stdout);
    let mut sessions = Vec::new();

    for session_name in session_names.lines() {
        // `WEB_SESSION` hosts an autostarted web server — infrastructure, never a
        // work target, so it's hidden from every session listing.
        if session_name.is_empty() || session_name == WEB_SESSION {
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
            &exact(session),
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
    let target = exact_window(session, window_index);
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
    // Focus is logged by the tmux `client-session-changed` hook, which fires for this
    // `switch-client` and for native switches (`prefix s`, click, `switch-client` elsewhere)
    // alike — so no explicit focus log here.
    let _ = Command::new("tmux")
        .args(["switch-client", "-t", &exact(session_name)])
        .output();
}

/// Select a window within a session (does not switch the attached client).
pub fn select_window(session: &str, window_index: &str) {
    let target = exact_window(session, window_index);
    let _ = Command::new("tmux")
        .args(["select-window", "-t", &target])
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
                .filter(|l| !l.is_empty() && *l != WEB_SESSION)
                .map(|s| s.to_string())
                .collect()
        })
        .unwrap_or_default()
}

/// Every tmux window across all sessions, keyed by session name → list of
/// `(window_index, window_name)`. One `list-windows -a` call. Session and window
/// names may contain spaces (emoji names), so fields are tab-separated.
pub fn get_all_windows() -> std::collections::HashMap<String, Vec<(String, String)>> {
    let mut map: std::collections::HashMap<String, Vec<(String, String)>> =
        std::collections::HashMap::new();
    let output = Command::new("tmux")
        .args([
            "list-windows",
            "-a",
            "-F",
            "#{session_name}\t#{window_index}\t#{window_name}",
        ])
        .output();
    if let Ok(o) = output {
        for line in String::from_utf8_lossy(&o.stdout).lines() {
            let mut parts = line.splitn(3, '\t');
            if let (Some(session), Some(idx)) = (parts.next(), parts.next()) {
                // Hide the autostarted web server's session (see WEB_SESSION).
                if session == WEB_SESSION {
                    continue;
                }
                let name = parts.next().unwrap_or("");
                map.entry(session.to_string())
                    .or_default()
                    .push((idx.to_string(), name.to_string()));
            }
        }
    }
    map
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

/// Get the current active tmux pane's working directory. Read from tmux rather
/// than `current_dir()` because the caller may be a popup, whose own cwd says
/// nothing about the window the user pressed the key from.
pub fn get_current_tmux_pane_path() -> Option<String> {
    Command::new("tmux")
        .args(["display-message", "-p", "#{pane_current_path}"])
        .output()
        .ok()
        .and_then(|o| {
            let path = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if path.is_empty() {
                None
            } else {
                Some(path)
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

/// One attached tmux client.
///
/// `session`/`window_index` are where that client is looking *right now* — the
/// "you are here" the sidebar marks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientInfo {
    pub tty: String,
    pub session: String,
    pub window_index: String,
    pub activity: i64,
    pub control: bool,
}

/// Parse `list-clients` output. Split out from the command for unit testing.
pub(crate) fn parse_clients(output: &str) -> Vec<ClientInfo> {
    output
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|line| {
            let mut f = line.split('\t');
            let tty = f.next()?.trim().to_string();
            let session = f.next()?.trim().to_string();
            let window_index = f.next()?.trim().to_string();
            let activity = f.next().unwrap_or("0").trim().parse().unwrap_or(0);
            let control = f.next().unwrap_or("0").trim() == "1";
            if tty.is_empty() {
                return None;
            }
            Some(ClientInfo {
                tty,
                session,
                window_index,
                activity,
                control,
            })
        })
        .collect()
}

/// Every attached tmux client, with the window each one is currently on.
///
/// **This is the only honest source for a client's position.** `display-message
/// -c <tty>` does *not* scope format evaluation to that client — `-c` only picks
/// where the message is shown, so `#{session_name}` there resolves against the
/// CALLER's `$TMUX` (or the server's guess) and silently reports the wrong
/// window. `list-clients` evaluates each format in its own client's context.
pub fn list_clients() -> Vec<ClientInfo> {
    let out = Command::new("tmux")
        .args([
            "list-clients",
            "-F",
            "#{client_tty}\t#{client_session}\t#{window_index}\t#{client_activity}\t#{client_control_mode}",
        ])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default();
    parse_clients(&out)
}

/// The client a remote surface (sidebar, web dashboard) should drive.
///
/// Control-mode clients are tools, never people, so they are never a switch
/// target; among the rest the most recently active one wins, which is the right
/// answer both for the single-client case and after `hive spread`.
pub fn pick_client(clients: &[ClientInfo]) -> Option<&ClientInfo> {
    clients
        .iter()
        .filter(|c| !c.control)
        .max_by_key(|c| c.activity)
}

/// Move one specific client to a session + window.
///
/// Needed because a bare `switch-client` acts on "the current client", which is
/// derived from the caller's `$TMUX` — and the web server runs inside the
/// detached `__hive_web` session, where that resolves to nothing.
/// `window_index: None` switches to the session's current window.
pub fn switch_client_to(client_tty: &str, session: &str, window_index: Option<&str>) -> bool {
    let target = match window_index {
        Some(w) => exact_window(session, w),
        None => exact(session),
    };
    Command::new("tmux")
        .args(["switch-client", "-c", client_tty, "-t", &target])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
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
                &exact(session),
                "-F",
                "#{window_index}:#{window_panes}",
            ])
            .output()
        {
            let window_list = String::from_utf8_lossy(&output.stdout);
            for line in window_list.lines() {
                if let Some((idx, count_str)) = line.split_once(':') {
                    let pane_count: usize = count_str.parse().unwrap_or(0);
                    let target = exact_window(session, idx);
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
    // A global pane id ("%12") is a complete, rename-proof tmux target on its own;
    // otherwise address the pane positionally as session:window.paneindex.
    let target = if pane.starts_with('%') {
        pane.to_string()
    } else {
        exact_pane(session, window, pane)
    };
    // Send the text literally (-l flag prevents interpretation of special keys)
    let _ = Command::new("tmux")
        .args(["send-keys", "-t", &target, "-l", text])
        .output();
    // Then send Enter
    let _ = Command::new("tmux")
        .args(["send-keys", "-t", &target, "Enter"])
        .output();
}

/// Capture the visible text of a pane (`tmux capture-pane -p`). None on failure.
pub fn capture_pane(session: &str, window: &str, pane: &str) -> Option<String> {
    let target = exact_pane(session, window, pane);
    let out = Command::new("tmux")
        .args(["capture-pane", "-p", "-t", &target])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Kill a tmux session
pub fn kill_tmux_session(name: &str) -> bool {
    let killed = Command::new("tmux")
        .args(["kill-session", "-t", &exact(name)])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if killed {
        // The session is gone for good — drop its windows from the recovery snapshot and
        // log the kill.
        crate::common::activity::remove_windows_for_session(name);
        crate::common::activity::log_session_kill(name);
    }
    killed
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
    use super::{
        clean_claude_title, exact, exact_active_pane, exact_pane, exact_window, parse_clients,
        pick_client,
    };

    #[test]
    fn parse_clients_reads_tab_separated_fields() {
        let out = "/dev/ttys000\t📁 00-main\t2\t1789908696\t0\n";
        let c = parse_clients(out);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].tty, "/dev/ttys000");
        assert_eq!(c[0].session, "📁 00-main");
        assert_eq!(c[0].window_index, "2");
        assert_eq!(c[0].activity, 1789908696);
        assert!(!c[0].control);
    }

    // A session name can contain spaces and `[project]` tags, so the fields are
    // tab-separated and must not be split on whitespace.
    #[test]
    fn parse_clients_keeps_spaces_in_session_names() {
        let c = parse_clients("/dev/ttys1\t📊 [avateen] live-avatar\t3\t5\t0\n");
        assert_eq!(c[0].session, "📊 [avateen] live-avatar");
        assert_eq!(c[0].window_index, "3");
    }

    #[test]
    fn parse_clients_skips_blank_and_malformed_lines() {
        let c = parse_clients("\n\t\t\t\t\ngarbage\n/dev/ttys2\ts\t1\t9\t1\n");
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].tty, "/dev/ttys2");
        assert!(c[0].control);
    }

    // A control-mode client is a tool (our own `tmux -C`, an editor integration),
    // never a person — switching it would move nobody's screen.
    #[test]
    fn pick_client_never_picks_a_control_client() {
        let c = parse_clients("/dev/ttys1\ta\t1\t900\t1\n/dev/ttys2\tb\t1\t100\t0\n");
        assert_eq!(pick_client(&c).unwrap().tty, "/dev/ttys2");
    }

    #[test]
    fn pick_client_prefers_most_recently_active() {
        let c = parse_clients("/dev/ttys1\ta\t1\t100\t0\n/dev/ttys2\tb\t1\t900\t0\n");
        assert_eq!(pick_client(&c).unwrap().tty, "/dev/ttys2");
    }

    #[test]
    fn pick_client_is_none_when_only_control_clients_or_empty() {
        assert!(pick_client(&parse_clients("/dev/ttys1\ta\t1\t1\t1\n")).is_none());
        assert!(pick_client(&[]).is_none());
    }

    // tmux resolves a bare `-t` target by exact match, THEN fnmatch, THEN prefix — so
    // "📊 Avateen" silently resolves to a running "📊 Avateen Hub". Every session-name
    // target carries the `=` that forces a literal exact match.
    #[test]
    fn exact_prefixes_session_name() {
        assert_eq!(exact("📊 Avateen"), "=📊 Avateen");
    }

    #[test]
    fn exact_window_and_pane_targets() {
        assert_eq!(exact_window("📊 Avateen", "2"), "=📊 Avateen:2");
        assert_eq!(exact_pane("📊 Avateen", "2", "0"), "=📊 Avateen:2.0");
    }

    // send-keys takes a PANE target, and `=name` is not one — tmux answers "can't find
    // pane: =name" and the command is never typed, which left every resume / thaw /
    // new-conversation / startup-command window sitting at a bare shell. The trailing
    // `:` makes it the current window's active pane while keeping `=` exactness.
    #[test]
    fn exact_active_pane_is_a_pane_target() {
        assert_eq!(exact_active_pane("📊 Avateen"), "=📊 Avateen:");
        assert!(exact_active_pane("🐝 hive").starts_with('='));
        assert!(
            exact_active_pane("🐝 hive").ends_with(':'),
            "without the trailing colon this is a session target, which send-keys rejects"
        );
        assert_ne!(exact_active_pane("🐝 hive"), exact("🐝 hive"));
    }

    // Worktree session names embed `[project]`, which is an fnmatch character class in a
    // bare target. `=` makes it literal.
    #[test]
    fn exact_makes_worktree_brackets_literal() {
        assert_eq!(
            exact("🌳 [clear-session] CSD-2527"),
            "=🌳 [clear-session] CSD-2527"
        );
    }

    // The prefix goes on exactly once, at the front — a name that already looks like a
    // target is still just a name.
    #[test]
    fn exact_does_not_double_prefix_or_reorder() {
        assert!(exact("a").starts_with('='));
        assert_eq!(exact("=a"), "==a");
        assert_eq!(exact(""), "=");
    }

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
