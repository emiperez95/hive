//! The terminal emulator around tmux — Ghostty — via AppleScript.
//!
//! hive itself only needs tmux; this is the thin layer for what tmux can't do:
//! spreading/collapsing the emulator's own split panes (`hive spread`, `L`) and
//! hosting the terminal sidebar (`hive sidebar`) in a split of its own.
//!
//! Which emulator is asked comes from [`detect`]. Ghostty is the only backend,
//! but [`Terminal`] stays an enum on purpose: adding one (kitty and WezTerm
//! both ship a remote-control CLI) is then compile-guided — every `match`
//! below gains an arm. Only functional on macOS; on other platforms every
//! function returns its "nothing happened" value.

/// A terminal emulator hive knows how to script.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Terminal {
    Ghostty,
}

impl Terminal {
    /// The application name AppleScript addresses.
    fn app(self) -> &'static str {
        match self {
            Terminal::Ghostty => "Ghostty",
        }
    }
}

/// `$TERM_PROGRAM` → emulator. Inside tmux it reads `tmux`, so this only
/// answers for a process started straight from the terminal.
fn from_term_program(v: &str) -> Option<Terminal> {
    match v {
        "ghostty" => Some(Terminal::Ghostty),
        _ => None,
    }
}

/// A tmux client's `#{client_termtype}` (the terminal's XTVERSION reply, e.g.
/// `ghostty 1.3.1`) and `#{client_termname}` (its `$TERM`) → emulator. Either
/// identifies Ghostty; a terminal whose `$TERM` is a generic `xterm-256color`
/// can only be told apart by the termtype.
fn from_tmux_client(termtype: &str, termname: &str) -> Option<Terminal> {
    let t = termtype.to_ascii_lowercase();
    if t.starts_with("ghostty") || termname == "xterm-ghostty" {
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
/// 1. `$HIVE_TERMINAL` (`ghostty`) — an explicit override.
/// 2. `$TERM_PROGRAM` — set when hive runs directly in the terminal.
/// 3. The most recently active tmux client's terminal type. This is the path
///    almost every call takes: hive runs inside tmux (popup, keybinding) or in
///    the detached `__hive_web` session, where the environment says nothing.
pub fn detect() -> Option<Terminal> {
    if let Ok(v) = std::env::var("HIVE_TERMINAL") {
        if v.eq_ignore_ascii_case("ghostty") {
            return Some(Terminal::Ghostty);
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

/// The terminal title `hive sidebar` sets on itself. Ghostty's AppleScript has
/// no other handle on "which split is the sidebar", so the title is the marker:
/// it is how a tab is checked for an existing sidebar, how the sidebar finds its
/// own split to resize, and how it finds the work split to hand focus back to.
pub const SIDEBAR_TITLE: &str = "hive sidebar";

/// Escape a string for an AppleScript double-quoted literal.
#[cfg(target_os = "macos")]
fn as_quote(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// What [`open_sidebar_split`] did.
#[derive(Debug, PartialEq, Eq)]
pub enum SidebarSplit {
    Opened,
    AlreadyOpen,
    /// Not in Ghostty, or the AppleScript failed.
    Unavailable,
}

/// Open `command` as a split on the right of the current Ghostty tab, unless
/// the tab already has a sidebar. Focus stays on the split it was opened from —
/// the caller is usually `hive start`, about to become the tmux client there.
#[cfg(target_os = "macos")]
pub fn open_sidebar_split(command: &str) -> SidebarSplit {
    if detect() != Some(Terminal::Ghostty) {
        return SidebarSplit::Unavailable;
    }
    let script = format!(
        r#"
tell application "Ghostty"
    set tb to selected tab of front window
    if (count of (terminals of tb whose name is "{title}")) > 0 then return "open"
    set t to focused terminal of tb
    split t direction right with configuration {{command:"{command}"}}
    focus t
    return "opened"
end tell
"#,
        title = SIDEBAR_TITLE,
        command = as_quote(command),
    );
    match osascript(&script).as_deref() {
        Some("opened") => SidebarSplit::Opened,
        Some("open") => SidebarSplit::AlreadyOpen,
        _ => SidebarSplit::Unavailable,
    }
}

#[cfg(not(target_os = "macos"))]
pub fn open_sidebar_split(_command: &str) -> SidebarSplit {
    SidebarSplit::Unavailable
}

/// Move the sidebar split's divider by `points`: positive shrinks the sidebar
/// (divider right), negative grows it (divider left). Ghostty measures in
/// screen points, not the pixels the pty reports — see `sidebar::fit_width`.
#[cfg(target_os = "macos")]
pub fn resize_sidebar_split(points: i32) -> bool {
    let (dir, amount) = if points >= 0 {
        ("right", points)
    } else {
        ("left", -points)
    };
    let script = format!(
        r#"
tell application "Ghostty"
    set tb to selected tab of front window
    set s to first terminal of tb whose name is "{SIDEBAR_TITLE}"
    perform action "resize_split:{dir},{amount}" on s
end tell
"#
    );
    osascript(&script).is_some()
}

#[cfg(not(target_os = "macos"))]
pub fn resize_sidebar_split(_points: i32) -> bool {
    false
}

/// Move keyboard focus from the sidebar to the work split beside it, so a click
/// in the sidebar doesn't leave every keystroke going to the sidebar.
#[cfg(target_os = "macos")]
pub fn focus_work_split() -> bool {
    let script = format!(
        r#"
tell application "Ghostty"
    set tb to selected tab of front window
    focus (first terminal of tb whose name is not "{SIDEBAR_TITLE}")
end tell
"#
    );
    osascript(&script).is_some()
}

#[cfg(not(target_os = "macos"))]
pub fn focus_work_split() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn term_program_names_the_emulator() {
        assert_eq!(from_term_program("ghostty"), Some(Terminal::Ghostty));
        // Inside tmux $TERM_PROGRAM is tmux's own, which says nothing.
        assert_eq!(from_term_program("tmux"), None);
        // A terminal hive cannot script is not mistaken for one it can.
        assert_eq!(from_term_program("iTerm.app"), None);
    }

    // Either the XTVERSION reply or Ghostty's own $TERM identifies it; a
    // generic xterm-256color with no termtype identifies nothing.
    #[test]
    fn tmux_client_classified_by_termtype_then_termname() {
        assert_eq!(
            from_tmux_client("ghostty 1.3.1", "xterm-ghostty"),
            Some(Terminal::Ghostty)
        );
        assert_eq!(
            from_tmux_client("ghostty 1.3.1", "xterm-256color"),
            Some(Terminal::Ghostty)
        );
        assert_eq!(
            from_tmux_client("", "xterm-ghostty"),
            Some(Terminal::Ghostty)
        );
        assert_eq!(from_tmux_client("", "xterm-256color"), None);
        assert_eq!(from_tmux_client("kitty(0.35.2)", "xterm-kitty"), None);
    }

    #[test]
    fn client_list_picks_most_recent_non_control_client() {
        let out = "100\t0\tkitty(0.35.2)\txterm-kitty\n\
                   300\t0\tghostty 1.3.1\txterm-ghostty\n\
                   900\t1\tkitty(0.35.2)\txterm-kitty\n";
        // The control-mode client is the most recent but is a tool, not a person.
        assert_eq!(from_client_list(out), Some(Terminal::Ghostty));
        assert_eq!(from_client_list(""), None);
        assert_eq!(from_client_list("garbage\n"), None);
    }

    // The client a person is at decides, even when an older Ghostty client is
    // still attached somewhere: scripting Ghostty then would split the wrong
    // window.
    #[test]
    fn client_list_does_not_fall_back_to_an_older_scriptable_client() {
        let out = "300\t0\tghostty 1.3.1\txterm-ghostty\n\
                   900\t0\tkitty(0.35.2)\txterm-kitty\n";
        assert_eq!(from_client_list(out), None);
    }
}
