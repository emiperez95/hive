//! Process detection and resource monitoring.

use crate::common::types::ProcessInfo;
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

/// Get all descendant PIDs of a parent process.
/// Uses `ps` on macOS for accurate parent-child data (sysinfo can report phantom relationships).
pub fn get_all_descendants(_sys: &System, parent_pid: u32, descendants: &mut Vec<u32>) {
    // Build parent→children map from ps (authoritative on macOS)
    let output = match std::process::Command::new("ps")
        .args(["-eo", "pid,ppid"])
        .output()
    {
        Ok(o) if o.status.success() => o,
        _ => return,
    };

    let mut children: std::collections::HashMap<u32, Vec<u32>> = std::collections::HashMap::new();
    for line in String::from_utf8_lossy(&output.stdout).lines().skip(1) {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 2 {
            if let (Ok(pid), Ok(ppid)) = (parts[0].parse::<u32>(), parts[1].parse::<u32>()) {
                children.entry(ppid).or_default().push(pid);
            }
        }
    }

    // BFS to collect all descendants
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
