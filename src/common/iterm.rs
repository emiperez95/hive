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

/// Open N new vertical iTerm2 panes, each running `hive start`.
///
/// The current pane is untouched. Each new pane runs hive start which
/// auto-attaches to the first available tmux session.
#[cfg(target_os = "macos")]
pub fn spread_panes(n: usize) -> bool {
    if n == 0 {
        return true;
    }

    use std::process::Command;

    let hive = std::env::current_exe()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| "hive".to_string());
    let path = std::env::var("PATH").unwrap_or_default();

    let mut splits = String::new();
    for _ in 0..n {
        splits.push_str(&format!(
            r#"
            tell lastSess
                set newSess to (split vertically with default profile command "env PATH='{}' {} start")
            end tell
            set lastSess to newSess
"#,
            path, hive
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
pub fn spread_panes(_n: usize) -> bool {
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
