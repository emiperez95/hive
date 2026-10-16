//! The terminal emulator around tmux — iTerm2 or Ghostty — via AppleScript.
//!
//! hive itself only needs tmux; this is the thin layer for what tmux can't do:
//! spreading/collapsing the emulator's own split panes (`hive spread`, `L`) and
//! handing keyboard focus back to the terminal after a sidebar click.
//!
//! Which emulator is asked comes from [`detect`]. Only functional on macOS; on
//! other platforms every function returns its "nothing happened" value.

/// A terminal emulator hive knows how to script.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Terminal {
    ITerm,
    Ghostty,
}

impl Terminal {
    /// The application name AppleScript addresses.
    fn app(self) -> &'static str {
        match self {
            Terminal::ITerm => "iTerm2",
            Terminal::Ghostty => "Ghostty",
        }
    }
}

/// `$TERM_PROGRAM` → emulator. Inside tmux it reads `tmux`, so this only
/// answers for a process started straight from the terminal.
fn from_term_program(v: &str) -> Option<Terminal> {
    match v {
        "iTerm.app" => Some(Terminal::ITerm),
        "ghostty" => Some(Terminal::Ghostty),
        _ => None,
    }
}

/// A tmux client's `#{client_termtype}` (the terminal's XTVERSION reply, e.g.
/// `iTerm2 3.5.2`, `ghostty 1.3.1`) and `#{client_termname}` (its `$TERM`) →
/// emulator. The termtype is the reliable one: iTerm's `$TERM` is a generic
/// `xterm-256color`.
fn from_tmux_client(termtype: &str, termname: &str) -> Option<Terminal> {
    let t = termtype.to_ascii_lowercase();
    if t.starts_with("iterm2") {
        Some(Terminal::ITerm)
    } else if t.starts_with("ghostty") || termname == "xterm-ghostty" {
        Some(Terminal::Ghostty)
    } else {
        None
    }
}

/// Parse `list-clients -F '#{client_activity}\t#{client_control_mode}\t
/// #{client_termtype}\t#{client_termname}'` and classify the client a person is
/// most likely at — the same rule as `tmux::pick_client`: control-mode clients
/// are tools, and the most recently active of the rest wins.
fn from_client_list(output: &str) -> Option<Terminal> {
    output
        .lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split('\t').collect();
            if f.len() < 4 || f[1].trim() == "1" {
                return None;
            }
            let activity: u64 = f[0].trim().parse().unwrap_or(0);
            Some((activity, f[2].trim(), f[3].trim()))
        })
        .max_by_key(|(activity, _, _)| *activity)
        .and_then(|(_, termtype, termname)| from_tmux_client(termtype, termname))
}

/// Which emulator to script, in order of authority:
///
/// 1. `$HIVE_TERMINAL` (`iterm` / `ghostty`) — an explicit override.
/// 2. `$TERM_PROGRAM` — set when hive runs directly in the terminal.
/// 3. The most recently active tmux client's terminal type. This is the path
///    almost every call takes: hive runs inside tmux (popup, keybinding) or in
///    the detached `__hive_web` session, where the environment says nothing.
pub fn detect() -> Option<Terminal> {
    if let Ok(v) = std::env::var("HIVE_TERMINAL") {
        match v.to_ascii_lowercase().as_str() {
            "iterm" | "iterm2" => return Some(Terminal::ITerm),
            "ghostty" => return Some(Terminal::Ghostty),
            _ => {}
        }
    }
    if let Some(t) = std::env::var("TERM_PROGRAM")
        .ok()
        .and_then(|v| from_term_program(&v))
    {
        return Some(t);
    }
    let out = std::process::Command::new("tmux")
        .args([
            "list-clients",
            "-F",
            "#{client_activity}\t#{client_control_mode}\t#{client_termtype}\t#{client_termname}",
        ])
        .output()
        .ok()?;
    from_client_list(&String::from_utf8_lossy(&out.stdout))
}

#[cfg(target_os = "macos")]
fn osascript(script: &str) -> Option<String> {
    let out = std::process::Command::new("osascript")
        .arg("-e")
        .arg(script)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Number of split panes in the emulator's current tab.
///
/// Returns 0 when no supported emulator is detected or running, AppleScript
/// fails, or not on macOS.
#[cfg(target_os = "macos")]
pub fn pane_count() -> usize {
    let Some(term) = detect() else { return 0 };
    // `is running` is checked first so a query never launches the app.
    let body = match term {
        Terminal::ITerm => {
            "tell current window to tell current tab to return (count of sessions) as text"
        }
        Terminal::Ghostty => "return (count of terminals of selected tab of front window) as text",
    };
    let script = format!(
        r#"
if application "{app}" is not running then return "0"
tell application "{app}"
    {body}
end tell
"#,
        app = term.app()
    );
    osascript(&script).and_then(|s| s.parse().ok()).unwrap_or(0)
}

#[cfg(not(target_os = "macos"))]
pub fn pane_count() -> usize {
    0
}

/// Open N new side-by-side panes, each running `hive start`.
///
/// The current pane is untouched. Each new pane runs hive start, which
/// auto-attaches to the first available tmux session. PATH is passed along
/// explicitly because a new split starts with a minimal environment.
#[cfg(target_os = "macos")]
pub fn spread_panes(n: usize) -> bool {
    if n == 0 {
        return true;
    }
    let Some(term) = detect() else { return false };

    let hive = std::env::current_exe()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| "hive".to_string());
    let path = std::env::var("PATH").unwrap_or_default();
    let command = format!("env PATH='{}' {} start", path, hive);

    let script = match term {
        Terminal::ITerm => {
            let mut splits = String::new();
            for _ in 0..n {
                splits.push_str(&format!(
                    r#"
            tell lastSess
                set newSess to (split vertically with default profile command "{}")
            end tell
            set lastSess to newSess
"#,
                    command
                ));
            }
            format!(
                r#"
tell application "iTerm2"
    tell current window
        tell current tab
            set lastSess to current session
{}
        end tell
    end tell
end tell
"#,
                splits
            )
        }
        // Each split halves the one before it, so the widths come out 1/2,
        // 1/4, 1/8…; `equalize_splits` evens them afterwards.
        Terminal::Ghostty => format!(
            r#"
tell application "Ghostty"
    set t to focused terminal of selected tab of front window
    repeat {n} times
        set t to split t direction right with configuration {{command:"{command}"}}
    end repeat
    perform action "equalize_splits" on t
end tell
"#
        ),
    };
    osascript(&script).is_some()
}

#[cfg(not(target_os = "macos"))]
pub fn spread_panes(_n: usize) -> bool {
    false
}

/// Collapse every pane in the current tab except the focused one.
///
/// Tmux sessions stay alive (just detached). Returns true on success.
#[cfg(target_os = "macos")]
pub fn collapse_panes() -> bool {
    if pane_count() <= 1 {
        return true;
    }
    let Some(term) = detect() else { return false };

    let script = match term {
        Terminal::ITerm => r#"
tell application "iTerm2"
    tell current window
        tell current tab
            if (count of sessions) ≤ 1 then return
            set keepId to id of current session
            repeat while (count of sessions) > 1
                set found to false
                repeat with sess in sessions
                    if id of sess ≠ keepId then
                        close sess
                        set found to true
                        exit repeat
                    end if
                end repeat
                if not found then exit repeat
            end repeat
        end tell
    end tell
end tell
"#
        .to_string(),
        // Ids are collected first: closing while iterating `terminals` would
        // shift the live list under the loop.
        Terminal::Ghostty => r#"
tell application "Ghostty"
    set tb to selected tab of front window
    set keepId to id of focused terminal of tb
    set ids to id of every terminal of tb
    repeat with tid in ids
        if (tid as text) ≠ keepId then close (first terminal of tb whose id is (tid as text))
    end repeat
end tell
"#
        .to_string(),
    };
    osascript(&script).is_some()
}

#[cfg(not(target_os = "macos"))]
pub fn collapse_panes() -> bool {
    false
}

/// Hand keyboard focus back to the terminal.
///
/// Clicking a row in the sidebar leaves the sidebar as first responder, so the
/// tmux client moves but every keystroke — prefix bindings included — still
/// goes to the panel. In iTerm the sidebar is a Toolbelt webview inside the
/// same window, so `select` on the current session moves first responder off
/// it; anywhere else the sidebar is its own window and the terminal app just
/// needs activating.
#[cfg(target_os = "macos")]
pub fn focus_current_session() -> bool {
    let script = match detect() {
        Some(Terminal::ITerm) => {
            r#"tell application "iTerm2" to tell current session of current window to select"#
        }
        Some(Terminal::Ghostty) => r#"tell application "Ghostty" to activate"#,
        None => return false,
    };
    osascript(script).is_some()
}

#[cfg(not(target_os = "macos"))]
pub fn focus_current_session() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn term_program_names_each_emulator() {
        assert_eq!(from_term_program("iTerm.app"), Some(Terminal::ITerm));
        assert_eq!(from_term_program("ghostty"), Some(Terminal::Ghostty));
        // Inside tmux $TERM_PROGRAM is tmux's own, which says nothing.
        assert_eq!(from_term_program("tmux"), None);
    }

    // iTerm's $TERM is a generic xterm-256color, so only the XTVERSION reply
    // identifies it; Ghostty is identifiable by either.
    #[test]
    fn tmux_client_classified_by_termtype_then_termname() {
        assert_eq!(
            from_tmux_client("iTerm2 3.5.2", "xterm-256color"),
            Some(Terminal::ITerm)
        );
        assert_eq!(
            from_tmux_client("ghostty 1.3.1", "xterm-ghostty"),
            Some(Terminal::Ghostty)
        );
        assert_eq!(
            from_tmux_client("", "xterm-ghostty"),
            Some(Terminal::Ghostty)
        );
        assert_eq!(from_tmux_client("", "xterm-256color"), None);
    }

    #[test]
    fn client_list_picks_most_recent_non_control_client() {
        let out = "100\t0\tiTerm2 3.5.2\txterm-256color\n\
                   300\t0\tghostty 1.3.1\txterm-ghostty\n\
                   900\t1\tiTerm2 3.5.2\txterm-256color\n";
        // The control-mode client is the most recent but is a tool, not a person.
        assert_eq!(from_client_list(out), Some(Terminal::Ghostty));
        assert_eq!(from_client_list(""), None);
        assert_eq!(from_client_list("garbage\n"), None);
    }
}
