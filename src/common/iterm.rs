//! iTerm2 pane management via AppleScript — spread/collapse panes.
//!
//! Only functional on macOS. On other platforms, functions return default values.

/// Get the number of iTerm2 panes (sessions) in the current tab.
///
/// Returns 0 if iTerm2 is not running, AppleScript fails, or not on macOS.
#[cfg(target_os = "macos")]
pub fn get_iterm_pane_count() -> usize {
    use std::process::Command;

    let script = r#"
tell application "System Events"
    if not (exists process "iTerm2") then return "0"
end tell
tell application "iTerm2"
    tell current window
        tell current tab
            return (count of sessions) as text
        end tell
    end tell
end tell
"#;

    let output = match Command::new("osascript").arg("-e").arg(script).output() {
        Ok(out) => out,
        Err(_) => return 0,
    };

    if !output.status.success() {
        return 0;
    }

    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<usize>()
        .unwrap_or(0)
}

#[cfg(not(target_os = "macos"))]
pub fn get_iterm_pane_count() -> usize {
    0
}

/// Spread tmux sessions into new vertical iTerm2 panes.
///
/// Takes session names for NEW panes only (the current session stays in the existing pane).
/// Each new pane runs `tmux attach-session -t <name>`.
/// Returns true on success.
#[cfg(target_os = "macos")]
pub fn spread_sessions(sessions: &[String]) -> bool {
    if sessions.is_empty() {
        return true;
    }

    use std::process::Command;

    // Resolve tmux absolute path — iTerm split panes have minimal PATH
    let tmux = super::tmux::resolve_tmux_path();

    let mut splits = String::new();
    for name in sessions {
        splits.push_str(&format!(
            r#"
            tell lastSess
                set newSess to (split vertically with default profile command "{} attach-session -t \"{}\"")
            end tell
            set lastSess to newSess
"#,
            tmux, name
        ));
    }

    let script = format!(
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
    );

    Command::new("osascript")
        .arg("-e")
        .arg(&script)
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

#[cfg(not(target_os = "macos"))]
pub fn spread_sessions(_sessions: &[String]) -> bool {
    false
}

/// Collapse all iTerm2 panes in the current tab except the current one.
///
/// Tmux sessions stay alive (just detached). Returns true on success.
#[cfg(target_os = "macos")]
pub fn collapse_panes() -> bool {
    if get_iterm_pane_count() <= 1 {
        return true;
    }

    use std::process::Command;

    let script = r#"
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
"#;

    Command::new("osascript")
        .arg("-e")
        .arg(script)
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

#[cfg(not(target_os = "macos"))]
pub fn collapse_panes() -> bool {
    false
}
