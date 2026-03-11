//! Chrome tab detection via AppleScript — fetches tabs and matches to listening ports.
//!
//! Only functional on macOS. On other platforms, functions return empty results.

use crate::common::ports::ListeningPort;

/// A Chrome browser tab.
#[derive(Debug, Clone)]
pub struct ChromeTab {
    pub title: String,
    pub url: String,
    pub window_index: usize,
    pub tab_index: usize,
}

/// Get all Chrome tabs via AppleScript.
///
/// Returns empty vec if Chrome is not running, AppleScript fails, or not on macOS.
#[cfg(target_os = "macos")]
pub fn get_chrome_tabs() -> Vec<ChromeTab> {
    use std::process::Command;

    // Use JXA (JavaScript for Automation) instead of AppleScript because
    // Chrome's AppleScript dictionary only exposes windows from the main profile,
    // while JXA sees all windows across all profiles and incognito.
    let script = r#"
var app = Application('System Events');
if (!app.processes.whose({name: 'Google Chrome'}).length) { ''; }
else {
    var chrome = Application('Google Chrome');
    var wins = chrome.windows();
    var lines = [];
    for (var w = 0; w < wins.length; w++) {
        var tabs = wins[w].tabs();
        for (var t = 0; t < tabs.length; t++) {
            var url = tabs[t].url();
            if (url.indexOf('localhost') !== -1 || url.indexOf('127.0.0.1') !== -1 || url.indexOf('[::1]') !== -1) {
                lines.push((w+1) + '\t' + (t+1) + '\t' + tabs[t].title() + '\t' + url);
            }
        }
    }
    lines.join('\n');
}
"#;

    let output = match Command::new("osascript")
        .arg("-l")
        .arg("JavaScript")
        .arg("-e")
        .arg(script)
        .output()
    {
        Ok(out) => out,
        Err(_) => return Vec::new(),
    };

    if !output.status.success() {
        return Vec::new();
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_chrome_tabs(&stdout)
}

#[cfg(not(target_os = "macos"))]
pub fn get_chrome_tabs() -> Vec<ChromeTab> {
    Vec::new()
}

/// Parse AppleScript output into ChromeTab structs.
fn parse_chrome_tabs(output: &str) -> Vec<ChromeTab> {
    let mut tabs = Vec::new();

    for line in output.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let parts: Vec<&str> = line.splitn(4, '\t').collect();
        if parts.len() < 4 {
            continue;
        }

        let window_index = match parts[0].parse::<usize>() {
            Ok(w) => w,
            Err(_) => continue,
        };
        let tab_index = match parts[1].parse::<usize>() {
            Ok(t) => t,
            Err(_) => continue,
        };

        tabs.push(ChromeTab {
            title: parts[2].to_string(),
            url: parts[3].to_string(),
            window_index,
            tab_index,
        });
    }

    tabs
}

/// Match Chrome tabs to a set of listening ports.
///
/// A tab matches if its URL contains `localhost:PORT` or `127.0.0.1:PORT`.
pub fn match_tabs_to_ports(tabs: &[ChromeTab], ports: &[ListeningPort]) -> Vec<(ChromeTab, u16)> {
    let mut matched = Vec::new();

    for tab in tabs {
        for port in ports {
            let port_str = port.port.to_string();
            // Check common localhost patterns in URL
            if tab.url.contains(&format!("localhost:{}", port_str))
                || tab.url.contains(&format!("127.0.0.1:{}", port_str))
                || tab.url.contains(&format!("[::1]:{}", port_str))
            {
                matched.push((tab.clone(), port.port));
                break; // One match per tab is enough
            }
        }
    }

    matched
}

/// Open a URL in Chrome (new tab).
#[cfg(target_os = "macos")]
pub fn open_chrome_tab(url: &str) -> bool {
    std::process::Command::new("open")
        .args(["-a", "Google Chrome", url])
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

#[cfg(not(target_os = "macos"))]
pub fn open_chrome_tab(_url: &str) -> bool {
    false
}

/// Focus a specific Chrome tab by activating its window and setting the active tab index.
#[cfg(target_os = "macos")]
pub fn focus_chrome_tab(tab: &ChromeTab) -> bool {
    use std::process::Command;

    let script = format!(
        r#"
var chrome = Application('Google Chrome');
var se = Application('System Events');
var proc = se.processes.byName('Google Chrome');
chrome.windows[{}].activeTabIndex = {};
proc.windows[{}].actions.byName('AXRaise').perform();
proc.frontmost = true;
"#,
        tab.window_index - 1,
        tab.tab_index,
        tab.window_index - 1
    );

    Command::new("osascript")
        .arg("-l")
        .arg("JavaScript")
        .arg("-e")
        .arg(&script)
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

#[cfg(not(target_os = "macos"))]
pub fn focus_chrome_tab(_tab: &ChromeTab) -> bool {
    false
}

/// Focus all Chrome tabs matching a session's ports.
///
/// If any port has at least one matching tab, activates every matched tab across all windows
/// (including duplicates — two windows with the same port both get focused).
/// Does nothing if no ports have matching tabs.
#[cfg(target_os = "macos")]
pub fn focus_all_matched_tabs(matched: &[(ChromeTab, u16)]) -> bool {
    use std::collections::HashMap;
    use std::process::Command;

    if matched.is_empty() {
        return false;
    }

    // Group tabs by window — we need to pick one tab per window to set as active.
    // If a window has multiple matching tabs, pick the first one.
    let mut best_per_window: HashMap<usize, &ChromeTab> = HashMap::new();
    for (tab, _) in matched {
        best_per_window.entry(tab.window_index).or_insert(tab);
    }

    // Set active tabs via Chrome JXA, then raise only matched windows via System Events.
    // Using AXRaise + frontmost instead of chrome.activate() avoids bringing ALL windows forward.
    let mut script = String::from(
        "var chrome = Application('Google Chrome');\n\
         var se = Application('System Events');\n\
         var proc = se.processes.byName('Google Chrome');\n",
    );
    for (win_idx, tab) in &best_per_window {
        // JXA uses 0-based window index, 1-based activeTabIndex
        script.push_str(&format!(
            "chrome.windows[{}].activeTabIndex = {};\n\
             proc.windows[{}].actions.byName('AXRaise').perform();\n",
            win_idx - 1,
            tab.tab_index,
            win_idx - 1
        ));
    }
    // Make Chrome the frontmost app (matched windows are already raised to top)
    script.push_str("proc.frontmost = true;\n");

    Command::new("osascript")
        .arg("-l")
        .arg("JavaScript")
        .arg("-e")
        .arg(&script)
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

#[cfg(not(target_os = "macos"))]
pub fn focus_all_matched_tabs(_matched: &[(ChromeTab, u16)]) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_chrome_tabs_empty() {
        assert!(parse_chrome_tabs("").is_empty());
    }

    #[test]
    fn test_parse_chrome_tabs_single() {
        let input = "1\t1\tMy App\thttp://localhost:3000/\n";
        let tabs = parse_chrome_tabs(input);
        assert_eq!(tabs.len(), 1);
        assert_eq!(tabs[0].window_index, 1);
        assert_eq!(tabs[0].tab_index, 1);
        assert_eq!(tabs[0].title, "My App");
        assert_eq!(tabs[0].url, "http://localhost:3000/");
    }

    #[test]
    fn test_parse_chrome_tabs_multiple() {
        let input = "1\t1\tTab One\thttp://localhost:3000/\n\
                      1\t2\tTab Two\thttp://example.com\n\
                      2\t1\tTab Three\thttp://127.0.0.1:8080/api\n";
        let tabs = parse_chrome_tabs(input);
        assert_eq!(tabs.len(), 3);
        assert_eq!(tabs[2].window_index, 2);
        assert_eq!(tabs[2].url, "http://127.0.0.1:8080/api");
    }

    #[test]
    fn test_match_tabs_to_ports_localhost() {
        let tabs = vec![
            ChromeTab {
                title: "My App".into(),
                url: "http://localhost:3000/".into(),
                window_index: 1,
                tab_index: 1,
            },
            ChromeTab {
                title: "Google".into(),
                url: "https://google.com".into(),
                window_index: 1,
                tab_index: 2,
            },
        ];
        let ports = vec![ListeningPort {
            port: 3000,
            pid: 123,
            process_name: "node".into(),
        }];
        let matched = match_tabs_to_ports(&tabs, &ports);
        assert_eq!(matched.len(), 1);
        assert_eq!(matched[0].0.title, "My App");
        assert_eq!(matched[0].1, 3000);
    }

    #[test]
    fn test_match_tabs_to_ports_127() {
        let tabs = vec![ChromeTab {
            title: "API".into(),
            url: "http://127.0.0.1:8080/api".into(),
            window_index: 1,
            tab_index: 1,
        }];
        let ports = vec![ListeningPort {
            port: 8080,
            pid: 456,
            process_name: "java".into(),
        }];
        let matched = match_tabs_to_ports(&tabs, &ports);
        assert_eq!(matched.len(), 1);
    }

    #[test]
    fn test_match_tabs_no_match() {
        let tabs = vec![ChromeTab {
            title: "Google".into(),
            url: "https://google.com".into(),
            window_index: 1,
            tab_index: 1,
        }];
        let ports = vec![ListeningPort {
            port: 3000,
            pid: 123,
            process_name: "node".into(),
        }];
        let matched = match_tabs_to_ports(&tabs, &ports);
        assert!(matched.is_empty());
    }
}
