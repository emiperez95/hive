//! JSONL parsing for Claude status detection.

use crate::common::debug::{debug_log, is_debug_enabled};
use crate::common::types::{truncate_command, ClaudeStatus};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use std::fs;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::PathBuf;

/// Partial structure for parsing jsonl entries - we only need specific fields
#[derive(Debug, Deserialize)]
pub struct JsonlEntry {
    #[serde(rename = "type")]
    pub entry_type: String,
    #[serde(default)]
    pub timestamp: Option<String>,
    #[serde(default)]
    pub message: Option<JsonlMessage>,
    #[serde(default)]
    pub data: Option<JsonlProgressData>,
}

#[derive(Debug, Deserialize)]
pub struct JsonlMessage {
    #[serde(default)]
    pub content: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
pub struct JsonlProgressData {
    #[serde(rename = "hookEvent")]
    #[serde(default)]
    pub hook_event: Option<String>,
    #[serde(rename = "hookName")]
    #[serde(default)]
    pub hook_name: Option<String>, // e.g., "PreToolUse:Write" - contains tool name
}

#[derive(Debug, Deserialize)]
pub struct ToolUse {
    #[serde(rename = "type")]
    pub content_type: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub input: Option<serde_json::Value>,
}

/// Extract filename from a full path
fn extract_filename(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).to_string()
}

/// Result of parsing jsonl for Claude status
#[derive(Debug)]
pub struct JsonlStatus {
    pub status: ClaudeStatus,
    pub timestamp: Option<DateTime<Utc>>,
}

/// Convert a project working directory to the Claude projects path
pub fn cwd_to_claude_projects_path(cwd: &str) -> PathBuf {
    let home = dirs::home_dir().unwrap_or_default();
    let encoded = cwd.replace('/', "-");
    home.join(".claude").join("projects").join(encoded)
}

/// Find the most recently modified jsonl file in a Claude projects directory
pub fn find_latest_jsonl(projects_path: &PathBuf) -> Option<PathBuf> {
    let entries = fs::read_dir(projects_path).ok()?;

    entries
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.path()
                .extension()
                .map(|ext| ext == "jsonl")
                .unwrap_or(false)
        })
        .max_by_key(|e| e.metadata().and_then(|m| m.modified()).ok())
        .map(|e| e.path())
}

/// Read the last N lines of a file efficiently
pub fn read_last_lines(path: &PathBuf, n: usize) -> Vec<String> {
    let file = match fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };

    let reader = BufReader::new(file);
    let lines: Vec<String> = reader.lines().map_while(Result::ok).collect();

    lines.into_iter().rev().take(n).collect()
}

/// Parse Claude status from a list of jsonl entries (pure function, testable)
/// Entries should be in chronological order (oldest first)
pub fn parse_status_from_entries(entries: &[JsonlEntry]) -> (ClaudeStatus, Option<DateTime<Utc>>) {
    // Find the last timestamp
    let timestamp = entries
        .iter()
        .rev()
        .find_map(|e| e.timestamp.as_ref())
        .and_then(|ts| DateTime::parse_from_rfc3339(ts).ok())
        .map(|dt| dt.with_timezone(&Utc));

    // Find the last progress entry to check hook state
    let last_progress_entry = entries
        .iter()
        .rev()
        .find(|e| e.entry_type == "progress")
        .and_then(|e| e.data.as_ref());

    let hook_event = last_progress_entry.and_then(|d| d.hook_event.as_deref());

    // Extract tool name from hook_name (e.g., "PreToolUse:Write" -> "Write")
    let hook_tool_name = last_progress_entry
        .and_then(|d| d.hook_name.as_deref())
        .and_then(|name| name.split(':').nth(1));

    // Find the matching tool_use from assistant message for details (file path, command, etc.)
    let find_tool_use = |target_name: &str| -> Option<ToolUse> {
        entries
            .iter()
            .rev()
            .filter(|e| e.entry_type == "assistant")
            .filter_map(|e| e.message.as_ref())
            .filter_map(|m| m.content.as_ref())
            .filter_map(|c| c.as_array())
            .flat_map(|arr| arr.iter())
            .filter_map(|v| serde_json::from_value::<ToolUse>(v.clone()).ok())
            .find(|t| t.content_type == "tool_use" && t.name.as_deref() == Some(target_name))
    };

    // Determine status based on patterns
    let status = match (hook_event, hook_tool_name) {
        // Tool called, PreToolUse fired - use hook_tool_name as the authoritative source
        (Some("PreToolUse"), Some(tool_name)) => {
            match tool_name {
                "Bash" | "Task" => {
                    // Find matching Bash/Task tool_use for command details
                    let (cmd, desc) = find_tool_use(tool_name)
                        .and_then(|tool| tool.input)
                        .map(|input| {
                            let command = input
                                .get("command")
                                .and_then(|v| v.as_str())
                                .unwrap_or("unknown command")
                                .to_string();
                            let description = input
                                .get("description")
                                .and_then(|v| v.as_str())
                                .map(|s| s.to_string());
                            (
                                format!("Bash: {}", truncate_command(&command, 60)),
                                description,
                            )
                        })
                        .unwrap_or(("Bash: ...".to_string(), None));
                    ClaudeStatus::NeedsPermission(cmd, desc)
                }
                "Write" | "Edit" => {
                    let file = find_tool_use(tool_name)
                        .and_then(|tool| tool.input)
                        .and_then(|input| input.get("file_path").cloned())
                        .and_then(|v| v.as_str().map(|s| s.to_string()))
                        .map(|s| extract_filename(&s))
                        .unwrap_or_else(|| "file".to_string());
                    ClaudeStatus::EditApproval(file)
                }
                "ExitPlanMode" => ClaudeStatus::PlanReview,
                "AskUserQuestion" => ClaudeStatus::QuestionAsked,
                // Auto-approved tools (Read, Grep, Glob, etc.) - show as working
                "Read" | "Grep" | "Glob" | "LS" => ClaudeStatus::Unknown,
                _ => ClaudeStatus::NeedsPermission(format!("{}: ...", tool_name), None),
            }
        }
        // Turn completed, waiting for input
        (Some("Stop"), _) => ClaudeStatus::Waiting,
        (Some("PostToolUse"), _) => ClaudeStatus::Unknown, // Processing/working
        // No clear signal, assume working
        _ => ClaudeStatus::Unknown,
    };

    (status, timestamp)
}

/// Read lines from the last `max_bytes` of a file (efficient tail read).
/// Skips the first partial line if we didn't start at offset 0.
fn read_tail_lines(path: &PathBuf, max_bytes: u64) -> Vec<String> {
    let mut file = match fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };

    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    if len == 0 {
        return Vec::new();
    }

    let seek_pos = len.saturating_sub(max_bytes);
    if seek_pos > 0 && file.seek(SeekFrom::Start(seek_pos)).is_err() {
        return Vec::new();
    }

    let reader = BufReader::new(file);
    let mut lines: Vec<String> = reader.lines().map_while(Result::ok).collect();

    // Skip the first partial line if we didn't start at the beginning
    if seek_pos > 0 && !lines.is_empty() {
        lines.remove(0);
    }

    lines
}

/// A message in the conversation (user or assistant).
#[derive(Debug, Clone)]
pub struct ConversationMessage {
    pub role: String,
    pub text: String,
    pub tools: Vec<ToolSummary>,
}

/// Compact summary of a tool use.
#[derive(Debug, Clone)]
pub struct ToolSummary {
    pub name: String,
    pub summary: String,
    pub detail: String,
}

/// Extract text content from a JSONL content field.
/// Handles both plain strings (user messages) and arrays of content blocks (assistant messages).
fn extract_text_from_content(content: &serde_json::Value) -> Option<String> {
    // User messages can be plain strings
    if let Some(s) = content.as_str() {
        if s.is_empty() {
            return None;
        }
        return Some(s.to_string());
    }

    let arr = content.as_array()?;
    let mut text_parts = Vec::new();
    for block in arr {
        if block.get("type").and_then(|t| t.as_str()) == Some("text") {
            if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                text_parts.push(text.to_string());
            }
        }
    }
    if text_parts.is_empty() {
        None
    } else {
        Some(text_parts.join("\n\n"))
    }
}

/// Extract tool_use blocks from a JSONL content array into compact summaries.
fn extract_tools_from_content(content: &serde_json::Value) -> Vec<ToolSummary> {
    let arr = match content.as_array() {
        Some(a) => a,
        None => return Vec::new(),
    };

    let mut tools = Vec::new();
    for block in arr {
        if block.get("type").and_then(|t| t.as_str()) != Some("tool_use") {
            continue;
        }

        let name = block
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("Unknown")
            .to_string();
        let input = block.get("input");

        let (summary, detail) = match name.as_str() {
            "Bash" => {
                let cmd = input
                    .and_then(|i| i.get("command"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let desc = input
                    .and_then(|i| i.get("description"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let summary = if desc.is_empty() {
                    truncate_str(cmd, 80)
                } else {
                    desc.to_string()
                };
                (summary, cmd.to_string())
            }
            "Write" => {
                let path = input
                    .and_then(|i| i.get("file_path"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let content_text = input
                    .and_then(|i| i.get("content"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                (
                    extract_filename(path),
                    format!("{}\n\n{}", path, truncate_str(content_text, 2000)),
                )
            }
            "Edit" => {
                let path = input
                    .and_then(|i| i.get("file_path"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let old = input
                    .and_then(|i| i.get("old_string"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let new = input
                    .and_then(|i| i.get("new_string"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                (
                    extract_filename(path),
                    format!(
                        "{}\n\n--- old ---\n{}\n\n+++ new +++\n{}",
                        path,
                        truncate_str(old, 1000),
                        truncate_str(new, 1000)
                    ),
                )
            }
            "Read" => {
                let path = input
                    .and_then(|i| i.get("file_path"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                (extract_filename(path), path.to_string())
            }
            "Grep" => {
                let pattern = input
                    .and_then(|i| i.get("pattern"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let path = input
                    .and_then(|i| i.get("path"))
                    .and_then(|v| v.as_str())
                    .unwrap_or(".");
                (
                    format!("/{}/", truncate_str(pattern, 40)),
                    format!("pattern: {}\npath: {}", pattern, path),
                )
            }
            "Glob" => {
                let pattern = input
                    .and_then(|i| i.get("pattern"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                (pattern.to_string(), pattern.to_string())
            }
            "Agent" => {
                let desc = input
                    .and_then(|i| i.get("description"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let prompt = input
                    .and_then(|i| i.get("prompt"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                (
                    desc.to_string(),
                    truncate_str(prompt, 2000),
                )
            }
            _ => {
                // Generic: show first string field from input as summary
                let summary = input
                    .and_then(|i| i.as_object())
                    .and_then(|obj| {
                        obj.values()
                            .find_map(|v| v.as_str().map(|s| truncate_str(s, 60)))
                    })
                    .unwrap_or_default();
                let detail = input
                    .map(|i| serde_json::to_string_pretty(i).unwrap_or_default())
                    .unwrap_or_default();
                (summary, detail)
            }
        };

        tools.push(ToolSummary {
            name,
            summary,
            detail,
        });
    }

    tools
}

/// Truncate a string to approximately `max` bytes, appending "..." if truncated.
/// Respects UTF-8 char boundaries.
fn truncate_str(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    // Find the last char boundary at or before `max`
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    let mut result = s[..end].to_string();
    result.push_str("...");
    result
}

/// Extract conversation messages (user + assistant) from JSONL lines.
/// Returns messages in chronological order, limited to the last `max_messages`.
pub fn extract_conversation_messages(lines: &[String], max_messages: usize) -> Vec<ConversationMessage> {
    let mut messages = Vec::new();

    for line in lines {
        let entry: JsonlEntry = match serde_json::from_str(line) {
            Ok(e) => e,
            Err(_) => continue,
        };

        if entry.entry_type != "assistant" && entry.entry_type != "user" {
            continue;
        }

        let content = match entry.message.and_then(|m| m.content) {
            Some(c) => c,
            None => continue,
        };

        let text = extract_text_from_content(&content).unwrap_or_default();
        let tools = if entry.entry_type == "assistant" {
            extract_tools_from_content(&content)
        } else {
            Vec::new()
        };

        // Skip entries with no text and no tools
        if text.is_empty() && tools.is_empty() {
            continue;
        }

        messages.push(ConversationMessage {
            role: entry.entry_type,
            text,
            tools,
        });
    }

    // Keep only the last N messages
    if messages.len() > max_messages {
        messages.drain(..messages.len() - max_messages);
    }

    messages
}

/// Extract the last assistant text message from a list of JSONL lines.
pub fn extract_last_assistant_text(lines: &[String]) -> Option<String> {
    for line in lines.iter().rev() {
        let entry: JsonlEntry = match serde_json::from_str(line) {
            Ok(e) => e,
            Err(_) => continue,
        };

        if entry.entry_type != "assistant" {
            continue;
        }

        let text = entry
            .message
            .and_then(|m| m.content)
            .and_then(|c| extract_text_from_content(&c));

        if text.is_some() {
            return text;
        }
    }

    None
}

/// Get conversation messages for a given project working directory.
/// Reads the full JSONL file and returns all messages.
pub fn get_conversation_messages(cwd: &str) -> Vec<ConversationMessage> {
    let projects_path = cwd_to_claude_projects_path(cwd);
    let jsonl_path = match find_latest_jsonl(&projects_path) {
        Some(p) => p,
        None => return Vec::new(),
    };

    let file = match fs::File::open(&jsonl_path) {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };

    let reader = BufReader::new(file);
    let lines: Vec<String> = reader.lines().map_while(Result::ok).collect();
    extract_conversation_messages(&lines, usize::MAX)
}

/// Parse Claude status from jsonl file
pub fn get_claude_status_from_jsonl(cwd: &str) -> Option<JsonlStatus> {
    let projects_path = cwd_to_claude_projects_path(cwd);
    let jsonl_path = find_latest_jsonl(&projects_path)?;

    let last_lines = read_last_lines(&jsonl_path, 10);
    if last_lines.is_empty() {
        return None;
    }

    // Parse entries (they're in reverse order from read_last_lines)
    let mut entries: Vec<JsonlEntry> = last_lines
        .iter()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();

    // Reverse to get chronological order
    entries.reverse();

    let (status, timestamp) = parse_status_from_entries(&entries);

    // Debug logging
    if is_debug_enabled() {
        let session_name = cwd.rsplit('/').next().unwrap_or(cwd);
        let entry_summary: Vec<String> = entries
            .iter()
            .map(|e| {
                let hook_info = e
                    .data
                    .as_ref()
                    .map(|d| {
                        format!(
                            "{}:{}",
                            d.hook_event.as_deref().unwrap_or("-"),
                            d.hook_name.as_deref().unwrap_or("-")
                        )
                    })
                    .unwrap_or_default();
                format!("{}({})", e.entry_type, hook_info)
            })
            .collect();
        debug_log(&format!(
            "JSONL [{}]: entries=[{}] -> status={:?}",
            session_name,
            entry_summary.join(", "),
            status
        ));
    }

    Some(JsonlStatus { status, timestamp })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_entry(json: &str) -> JsonlEntry {
        serde_json::from_str(json).expect("Failed to parse test JSON")
    }

    #[test]
    fn test_cwd_to_claude_projects_path() {
        let path = cwd_to_claude_projects_path("/Users/test/project");
        let path_str = path.to_string_lossy();
        assert!(path_str.ends_with("-Users-test-project"));
        assert!(path_str.contains(".claude/projects"));
    }

    #[test]
    fn test_waiting_status_stop_hook() {
        let progress =
            r#"{"type":"progress","data":{"hookEvent":"Stop"},"timestamp":"2026-01-29T10:00:00Z"}"#;
        let entries = vec![parse_entry(progress)];
        let (status, _) = parse_status_from_entries(&entries);
        assert!(matches!(status, ClaudeStatus::Waiting));
    }

    #[test]
    fn test_needs_permission_bash() {
        let assistant = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"pnpm exec prettier --write file.json","description":"Format JSON files"}}]}}"#;
        let progress =
            r#"{"type":"progress","data":{"hookEvent":"PreToolUse","hookName":"PreToolUse:Bash"}}"#;
        let entries = vec![parse_entry(assistant), parse_entry(progress)];
        let (status, _) = parse_status_from_entries(&entries);
        match status {
            ClaudeStatus::NeedsPermission(cmd, desc) => {
                assert!(cmd.contains("Bash:"));
                assert!(cmd.contains("prettier"));
                assert_eq!(desc, Some("Format JSON files".to_string()));
            }
            _ => panic!("Expected NeedsPermission, got {:?}", status),
        }
    }

    #[test]
    fn test_edit_approval_write() {
        let assistant = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Write","input":{"file_path":"/Users/test/project/test_file.txt","content":"test"}}]}}"#;
        let progress = r#"{"type":"progress","data":{"hookEvent":"PreToolUse","hookName":"PreToolUse:Write"}}"#;
        let entries = vec![parse_entry(assistant), parse_entry(progress)];
        let (status, _) = parse_status_from_entries(&entries);
        match status {
            ClaudeStatus::EditApproval(file) => {
                assert_eq!(file, "test_file.txt");
            }
            _ => panic!("Expected EditApproval, got {:?}", status),
        }
    }

    #[test]
    fn test_edit_approval_edit() {
        let assistant = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Edit","input":{"file_path":"/path/to/main.rs","old_string":"foo","new_string":"bar"}}]}}"#;
        let progress =
            r#"{"type":"progress","data":{"hookEvent":"PreToolUse","hookName":"PreToolUse:Edit"}}"#;
        let entries = vec![parse_entry(assistant), parse_entry(progress)];
        let (status, _) = parse_status_from_entries(&entries);
        match status {
            ClaudeStatus::EditApproval(file) => {
                assert_eq!(file, "main.rs");
            }
            _ => panic!("Expected EditApproval, got {:?}", status),
        }
    }

    #[test]
    fn test_plan_review() {
        let assistant = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"ExitPlanMode","input":{}}]}}"#;
        let progress = r#"{"type":"progress","data":{"hookEvent":"PreToolUse","hookName":"PreToolUse:ExitPlanMode"}}"#;
        let entries = vec![parse_entry(assistant), parse_entry(progress)];
        let (status, _) = parse_status_from_entries(&entries);
        assert!(matches!(status, ClaudeStatus::PlanReview));
    }

    #[test]
    fn test_question_asked() {
        let assistant = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"AskUserQuestion","input":{"questions":[]}}]}}"#;
        let progress = r#"{"type":"progress","data":{"hookEvent":"PreToolUse","hookName":"PreToolUse:AskUserQuestion"}}"#;
        let entries = vec![parse_entry(assistant), parse_entry(progress)];
        let (status, _) = parse_status_from_entries(&entries);
        assert!(matches!(status, ClaudeStatus::QuestionAsked));
    }

    #[test]
    fn test_working_state_post_tool() {
        let progress = r#"{"type":"progress","data":{"hookEvent":"PostToolUse"}}"#;
        let entries = vec![parse_entry(progress)];
        let (status, _) = parse_status_from_entries(&entries);
        assert!(matches!(status, ClaudeStatus::Unknown));
    }

    #[test]
    fn test_unknown_no_progress() {
        let assistant =
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Hello"}]}}"#;
        let entries = vec![parse_entry(assistant)];
        let (status, _) = parse_status_from_entries(&entries);
        assert!(matches!(status, ClaudeStatus::Unknown));
    }

    #[test]
    fn test_task_tool_needs_permission() {
        let assistant = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Task","input":{"command":"run tests","description":"Run test suite"}}]}}"#;
        let progress =
            r#"{"type":"progress","data":{"hookEvent":"PreToolUse","hookName":"PreToolUse:Task"}}"#;
        let entries = vec![parse_entry(assistant), parse_entry(progress)];
        let (status, _) = parse_status_from_entries(&entries);
        match status {
            ClaudeStatus::NeedsPermission(cmd, desc) => {
                assert!(cmd.contains("Bash:"));
                assert_eq!(desc, Some("Run test suite".to_string()));
            }
            _ => panic!("Expected NeedsPermission, got {:?}", status),
        }
    }

    #[test]
    fn test_other_tool_needs_permission() {
        let assistant = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"WebFetch","input":{"url":"https://example.com"}}]}}"#;
        let progress = r#"{"type":"progress","data":{"hookEvent":"PreToolUse","hookName":"PreToolUse:WebFetch"}}"#;
        let entries = vec![parse_entry(assistant), parse_entry(progress)];
        let (status, _) = parse_status_from_entries(&entries);
        match status {
            ClaudeStatus::NeedsPermission(cmd, _) => {
                assert!(cmd.contains("WebFetch:"));
            }
            _ => panic!("Expected NeedsPermission, got {:?}", status),
        }
    }

    #[test]
    fn test_timestamp_parsing() {
        let progress =
            r#"{"type":"progress","data":{"hookEvent":"Stop"},"timestamp":"2026-01-29T10:30:45Z"}"#;
        let entries = vec![parse_entry(progress)];
        let (_, timestamp) = parse_status_from_entries(&entries);
        assert!(timestamp.is_some());
        let ts = timestamp.unwrap();
        assert_eq!(ts.format("%Y-%m-%d").to_string(), "2026-01-29");
    }

    #[test]
    fn test_empty_entries() {
        let entries: Vec<JsonlEntry> = vec![];
        let (status, timestamp) = parse_status_from_entries(&entries);
        assert!(matches!(status, ClaudeStatus::Unknown));
        assert!(timestamp.is_none());
    }

    #[test]
    fn test_auto_approved_read_shows_working() {
        let assistant = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Read","input":{"file_path":"/some/file.txt"}}]}}"#;
        let progress =
            r#"{"type":"progress","data":{"hookEvent":"PreToolUse","hookName":"PreToolUse:Read"}}"#;
        let entries = vec![parse_entry(assistant), parse_entry(progress)];
        let (status, _) = parse_status_from_entries(&entries);
        assert!(matches!(status, ClaudeStatus::Unknown));
    }

    #[test]
    fn test_auto_approved_grep_shows_working() {
        let progress =
            r#"{"type":"progress","data":{"hookEvent":"PreToolUse","hookName":"PreToolUse:Grep"}}"#;
        let entries = vec![parse_entry(progress)];
        let (status, _) = parse_status_from_entries(&entries);
        assert!(matches!(status, ClaudeStatus::Unknown));
    }

    #[test]
    fn test_hookname_prevents_false_edit_approval() {
        let old_assistant = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Write","input":{"file_path":"/old/file.txt"}}]}}"#;
        let progress =
            r#"{"type":"progress","data":{"hookEvent":"PreToolUse","hookName":"PreToolUse:Read"}}"#;
        let entries = vec![parse_entry(old_assistant), parse_entry(progress)];
        let (status, _) = parse_status_from_entries(&entries);
        assert!(matches!(status, ClaudeStatus::Unknown));
    }

    #[test]
    fn test_extract_last_assistant_text_simple() {
        let lines = vec![
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Hello, I updated the file."}]}}"#.to_string(),
        ];
        let result = extract_last_assistant_text(&lines);
        assert_eq!(result, Some("Hello, I updated the file.".to_string()));
    }

    #[test]
    fn test_extract_last_assistant_text_multiple_blocks() {
        let lines = vec![
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"First part."},{"type":"tool_use","name":"Write","input":{}},{"type":"text","text":"Second part."}]}}"#.to_string(),
        ];
        let result = extract_last_assistant_text(&lines);
        assert_eq!(result, Some("First part.\n\nSecond part.".to_string()));
    }

    #[test]
    fn test_extract_last_assistant_text_picks_last_entry() {
        let lines = vec![
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Old message"}]}}"#.to_string(),
            r#"{"type":"progress","data":{"hookEvent":"PostToolUse"}}"#.to_string(),
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"New message"}]}}"#.to_string(),
        ];
        let result = extract_last_assistant_text(&lines);
        assert_eq!(result, Some("New message".to_string()));
    }

    #[test]
    fn test_extract_last_assistant_text_no_text_blocks() {
        let lines = vec![
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{}}]}}"#.to_string(),
        ];
        let result = extract_last_assistant_text(&lines);
        assert!(result.is_none());
    }

    #[test]
    fn test_extract_last_assistant_text_empty() {
        let lines: Vec<String> = vec![];
        let result = extract_last_assistant_text(&lines);
        assert!(result.is_none());
    }

    #[test]
    fn test_extract_last_assistant_text_long() {
        let long_text = "x".repeat(5000);
        let line = format!(
            r#"{{"type":"assistant","message":{{"content":[{{"type":"text","text":"{}"}}]}}}}"#,
            long_text
        );
        let lines = vec![line];
        let result = extract_last_assistant_text(&lines).unwrap();
        assert_eq!(result.len(), 5000);
    }

    #[test]
    fn test_extract_conversation_messages_basic() {
        let lines = vec![
            r#"{"type":"user","message":{"content":[{"type":"text","text":"Hello"}]}}"#.to_string(),
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Hi there!"}]}}"#.to_string(),
            r#"{"type":"progress","data":{"hookEvent":"Stop"}}"#.to_string(),
            r#"{"type":"user","message":{"content":[{"type":"text","text":"Do something"}]}}"#.to_string(),
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Done."},{"type":"tool_use","name":"Write","input":{"file_path":"/path/to/file.txt","content":"hello"}}]}}"#.to_string(),
        ];
        let msgs = extract_conversation_messages(&lines, 50);
        assert_eq!(msgs.len(), 4);
        assert_eq!(msgs[0].role, "user");
        assert_eq!(msgs[0].text, "Hello");
        assert_eq!(msgs[1].role, "assistant");
        assert_eq!(msgs[1].text, "Hi there!");
        assert_eq!(msgs[2].role, "user");
        assert_eq!(msgs[3].role, "assistant");
        assert_eq!(msgs[3].text, "Done.");
        assert_eq!(msgs[3].tools.len(), 1);
        assert_eq!(msgs[3].tools[0].name, "Write");
        assert_eq!(msgs[3].tools[0].summary, "file.txt");
    }

    #[test]
    fn test_extract_conversation_messages_max_limit() {
        let lines = vec![
            r#"{"type":"user","message":{"content":[{"type":"text","text":"First"}]}}"#.to_string(),
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Reply 1"}]}}"#.to_string(),
            r#"{"type":"user","message":{"content":[{"type":"text","text":"Second"}]}}"#.to_string(),
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Reply 2"}]}}"#.to_string(),
        ];
        let msgs = extract_conversation_messages(&lines, 2);
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role, "user");
        assert_eq!(msgs[0].text, "Second");
        assert_eq!(msgs[1].role, "assistant");
        assert_eq!(msgs[1].text, "Reply 2");
    }

    #[test]
    fn test_extract_conversation_tool_only_assistant() {
        let lines = vec![
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"cargo test","description":"Run tests"}}]}}"#.to_string(),
        ];
        let msgs = extract_conversation_messages(&lines, 50);
        assert_eq!(msgs.len(), 1);
        assert!(msgs[0].text.is_empty());
        assert_eq!(msgs[0].tools.len(), 1);
        assert_eq!(msgs[0].tools[0].name, "Bash");
        assert_eq!(msgs[0].tools[0].summary, "Run tests");
        assert_eq!(msgs[0].tools[0].detail, "cargo test");
    }

    #[test]
    fn test_extract_conversation_bash_no_description() {
        let lines = vec![
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"ls -la /tmp"}}]}}"#.to_string(),
        ];
        let msgs = extract_conversation_messages(&lines, 50);
        assert_eq!(msgs[0].tools[0].summary, "ls -la /tmp");
    }

    #[test]
    fn test_hookname_matches_correct_tool() {
        let bash_assistant = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"ls","description":"List files"}}]}}"#;
        let write_assistant = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Write","input":{"file_path":"/new/file.txt"}}]}}"#;
        let progress = r#"{"type":"progress","data":{"hookEvent":"PreToolUse","hookName":"PreToolUse:Write"}}"#;
        let entries = vec![
            parse_entry(bash_assistant),
            parse_entry(write_assistant),
            parse_entry(progress),
        ];
        let (status, _) = parse_status_from_entries(&entries);
        match status {
            ClaudeStatus::EditApproval(file) => {
                assert_eq!(file, "file.txt");
            }
            _ => panic!("Expected EditApproval, got {:?}", status),
        }
    }
}
