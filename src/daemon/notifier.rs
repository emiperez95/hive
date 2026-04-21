//! Platform-native notifications.

use std::process::Command;

/// Send a notification when a session needs attention.
///
/// Respects `HIVE_NO_NOTIFY=1` — when set (in CI, test harnesses, or
/// scripted invocations), all notification paths (native, tmux) are
/// short-circuited so hook calls don't produce desktop popups.
pub fn notify_needs_attention(session_name: &str, status: &str) {
    if std::env::var("HIVE_NO_NOTIFY").is_ok_and(|v| !v.is_empty() && v != "0") {
        return;
    }

    let title = "hive";
    let message = format!("{}: {}", session_name, status);

    // Try platform-specific notification
    #[cfg(target_os = "macos")]
    {
        if notify_macos(title, &message) {
            return;
        }
    }

    #[cfg(target_os = "linux")]
    {
        if notify_linux(title, &message) {
            return;
        }
    }

    // Fallback: tmux display-message
    notify_tmux(&message);
}

/// macOS notification using osascript
#[cfg(target_os = "macos")]
fn notify_macos(title: &str, message: &str) -> bool {
    // Try terminal-notifier first (better UX)
    if Command::new("terminal-notifier")
        .args(["-title", title, "-message", message, "-sound", "default"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
    {
        return true;
    }

    // Fallback to osascript
    let script = format!(
        "display notification \"{}\" with title \"{}\"",
        message.replace('"', "\\\""),
        title.replace('"', "\\\"")
    );
    Command::new("osascript")
        .args(["-e", &script])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Linux notification using notify-send
#[cfg(target_os = "linux")]
fn notify_linux(title: &str, message: &str) -> bool {
    Command::new("notify-send")
        .args([title, message])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Fallback notification via tmux display-message
fn notify_tmux(message: &str) {
    let _ = Command::new("tmux")
        .args(["display-message", "-d", "3000", message])
        .output();
}
