//! Process detection and resource monitoring.

use crate::common::types::ProcessInfo;
use std::collections::HashMap;
use sysinfo::{Pid, System};

/// Check if a process is Claude Code based on name/command
pub fn is_claude_process(proc: &ProcessInfo) -> bool {
    let name_lower = proc.name.to_lowercase();
    let cmd_lower = proc.command.to_lowercase();

    // Exclude hive itself
    if cmd_lower.contains("hive") && !cmd_lower.contains("hive hook") {
        // Only exclude the hive TUI binary, not "hive hook" subcommand
        if name_lower == "hive" {
            return false;
        }
    }

    // Check for claude in command
    if cmd_lower.contains("claude") {
        return true;
    }

    // Check for version number pattern (e.g., "2.1.20") which is how claude shows in tmux
    if proc
        .name
        .chars()
        .next()
        .map(|c| c.is_ascii_digit())
        .unwrap_or(false)
        && proc.name.contains('.')
        && proc.name.chars().filter(|&c| c == '.').count() >= 1
    {
        return true;
    }

    // Check if it's node running something with claude
    if name_lower == "node" && cmd_lower.contains("claude") {
        return true;
    }

    false
}

/// Build a parent→children PID map by running `ps -eo pid,ppid` once.
/// Used on macOS for accurate parent-child data (sysinfo can report phantom relationships).
/// Build this once per gather pass and reuse via `collect_descendants` to avoid
/// re-spawning `ps` for every pane.
pub fn build_children_map() -> HashMap<u32, Vec<u32>> {
    let output = match std::process::Command::new("ps")
        .args(["-eo", "pid,ppid"])
        .output()
    {
        Ok(o) if o.status.success() => o,
        _ => return HashMap::new(),
    };

    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for line in String::from_utf8_lossy(&output.stdout).lines().skip(1) {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 2 {
            if let (Ok(pid), Ok(ppid)) = (parts[0].parse::<u32>(), parts[1].parse::<u32>()) {
                children.entry(ppid).or_default().push(pid);
            }
        }
    }
    children
}

/// Build a pid→full-command-line map via one `ps -axww -o pid=,command=` call.
/// Unlike `sysinfo`'s `cmd()` (empty for Claude on macOS, which is why claude is
/// detected by its version-string process name), `ps` reports the real argv, so
/// this recovers `claude --resume <session_id>` — the per-window conversation id
/// that survives hook-state pruning. `-ww` disables width truncation.
pub fn build_cmdline_map() -> HashMap<u32, String> {
    let output = match std::process::Command::new("ps")
        .args(["-axww", "-o", "pid=,command="])
        .output()
    {
        Ok(o) if o.status.success() => o,
        _ => return HashMap::new(),
    };
    let mut map = HashMap::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let line = line.trim_start();
        if let Some((pid_str, cmd)) = line.split_once(char::is_whitespace) {
            if let Ok(pid) = pid_str.parse::<u32>() {
                map.insert(pid, cmd.trim().to_string());
            }
        }
    }
    map
}

/// Extract the `--resume <id>` (or `--resume=<id>`) Claude session id from a command
/// line, validated as UUID-shaped. None for `claude -c` / plain `claude` (no id in argv).
pub fn parse_resume_id(cmd: &str) -> Option<String> {
    let toks: Vec<&str> = cmd.split_whitespace().collect();
    for (i, t) in toks.iter().enumerate() {
        let id = if let Some(rest) = t.strip_prefix("--resume=") {
            Some(rest.to_string())
        } else if *t == "--resume" {
            toks.get(i + 1).map(|s| s.to_string())
        } else {
            None
        };
        if let Some(id) = id {
            if is_uuidish(&id) {
                return Some(id);
            }
        }
    }
    None
}

/// Canonical 8-4-4-4-12 hex UUID shape (a Claude session id), so a stray `--resume`
/// argument can't be mistaken for one.
fn is_uuidish(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() != 36 {
        return false;
    }
    b.iter().enumerate().all(|(i, &c)| match i {
        8 | 13 | 18 | 23 => c == b'-',
        _ => c.is_ascii_hexdigit(),
    })
}

/// Walk a pre-built children map to collect all descendant PIDs of `parent_pid`.
pub fn collect_descendants(
    children: &HashMap<u32, Vec<u32>>,
    parent_pid: u32,
    descendants: &mut Vec<u32>,
) {
    let mut queue = vec![parent_pid];
    while let Some(pid) = queue.pop() {
        if let Some(kids) = children.get(&pid) {
            for &kid in kids {
                descendants.push(kid);
                queue.push(kid);
            }
        }
    }
}

/// Get process info from sysinfo
pub fn get_process_info(sys: &System, pid: u32) -> Option<ProcessInfo> {
    sys.process(Pid::from_u32(pid)).map(|p| {
        let cmd = p
            .cmd()
            .iter()
            .map(|s| s.to_string_lossy().to_string())
            .collect::<Vec<_>>()
            .join(" ");

        ProcessInfo {
            pid,
            name: p.name().to_string_lossy().to_string(),
            cpu_percent: p.cpu_usage(),
            memory_kb: p.memory() / 1024,
            command: cmd,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_proc(name: &str, command: &str) -> ProcessInfo {
        ProcessInfo {
            pid: 1,
            name: name.to_string(),
            cpu_percent: 0.0,
            memory_kb: 0,
            command: command.to_string(),
        }
    }

    #[test]
    fn test_is_claude_version_pattern() {
        assert!(is_claude_process(&make_proc("2.1.20", "")));
        assert!(is_claude_process(&make_proc("2.1.23", "")));
        assert!(is_claude_process(&make_proc("3.0.0", "")));
    }

    #[test]
    fn test_is_claude_command_contains() {
        assert!(is_claude_process(&make_proc("node", "/path/to/claude")));
        assert!(is_claude_process(&make_proc("node", "claude -c")));
    }

    #[test]
    fn test_parse_resume_id() {
        let id = "d67ea1c0-cd45-4c2e-b8f9-d531265e8dac";
        assert_eq!(
            parse_resume_id(&format!("claude --resume {id}")).as_deref(),
            Some(id)
        );
        assert_eq!(
            parse_resume_id(&format!("/usr/bin/claude --resume={id} --foo")).as_deref(),
            Some(id)
        );
        // No id in argv → None (fresh session / continue).
        assert_eq!(parse_resume_id("claude"), None);
        assert_eq!(parse_resume_id("claude -c"), None);
        // A non-UUID resume argument must not be mistaken for a session id.
        assert_eq!(parse_resume_id("claude --resume latest"), None);
        assert_eq!(parse_resume_id("claude --resume"), None);
    }

    #[test]
    fn test_is_not_claude_regular_process() {
        assert!(!is_claude_process(&make_proc("bash", "ls")));
        assert!(!is_claude_process(&make_proc("vim", "vim file.txt")));
    }

    #[test]
    fn test_is_not_claude_hive() {
        // hive itself should not match
        assert!(!is_claude_process(&make_proc("hive", "")));
    }
}
