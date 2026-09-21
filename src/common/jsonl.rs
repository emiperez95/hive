//! JSONL parsing for Claude status detection.

use crate::common::debug::{debug_log, is_debug_enabled};
use crate::common::types::{truncate_command, ClaudeStatus};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};

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
    /// The tool_use block id (e.g. "toolu_…"). Used to correlate a background launch
    /// with its later `<task-notification>` (whose `<tool-use-id>` is this same id).
    #[serde(default)]
    pub id: Option<String>,
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

/// Convert a project working directory to the Claude projects path (primary: ~/.claude/projects/).
pub fn cwd_to_claude_projects_path(cwd: &str) -> PathBuf {
    let home = dirs::home_dir().unwrap_or_default();
    let encoded = cwd.replace('/', "-");
    home.join(".claude").join("projects").join(encoded)
}

/// Return all candidate Claude projects paths for a cwd across all config profiles.
/// Searches `~/.claude/projects/<slug>` and `~/.claude-*/projects/<slug>` (e.g. `~/.claude-work`).
/// Only returns paths that actually exist.
pub fn candidate_projects_paths(cwd: &str) -> Vec<PathBuf> {
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    let encoded = cwd.replace('/', "-");
    let mut paths: Vec<PathBuf> = Vec::new();

    // Primary profile
    paths.push(home.join(".claude").join("projects").join(&encoded));

    // Alt profiles: ~/.claude-*/projects/<slug>
    if let Ok(entries) = fs::read_dir(&home) {
        for entry in entries.filter_map(|e| e.ok()) {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str.starts_with(".claude-") && entry.path().is_dir() {
                paths.push(entry.path().join("projects").join(&encoded));
            }
        }
    }

    paths.into_iter().filter(|p| p.exists()).collect()
}

/// Scan the given directories for `<session_id>.jsonl` files and return their
/// basenames (session ids), deduplicated and sorted. Path-injectable so the
/// registry's existence scan is unit-testable without touching a real home dir.
pub fn scan_conversation_ids_in(dirs: &[PathBuf]) -> Vec<String> {
    let mut ids = std::collections::BTreeSet::new();
    for dir in dirs {
        let Ok(entries) = fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                    ids.insert(stem.to_string());
                }
            }
        }
    }
    ids.into_iter().collect()
}

/// All `~/.claude*/projects/<slug>` directories across every auth profile.
fn claude_slug_dirs() -> Vec<PathBuf> {
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    let mut slug_dirs: Vec<PathBuf> = Vec::new();
    if let Ok(entries) = fs::read_dir(&home) {
        for entry in entries.filter_map(|e| e.ok()) {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            let is_profile = name_str == ".claude" || name_str.starts_with(".claude-");
            if is_profile && entry.path().is_dir() {
                let projects = entry.path().join("projects");
                if let Ok(slugs) = fs::read_dir(&projects) {
                    for slug in slugs.filter_map(|e| e.ok()) {
                        if slug.path().is_dir() {
                            slug_dirs.push(slug.path());
                        }
                    }
                }
            }
        }
    }
    slug_dirs
}

/// Enumerate every Claude conversation id on disk across all auth profiles
/// (`~/.claude/projects/<slug>/<uuid>.jsonl` + `~/.claude-*/projects/...`).
/// On-disk existence is the source of truth for Closed/remote sessions.
pub fn scan_all_conversation_ids() -> Vec<String> {
    scan_conversation_ids_in(&claude_slug_dirs())
}

/// A conversation discovered on disk, enriched with the cwd (read from the
/// transcript) and last-activity (the jsonl file mtime) needed to place + bound
/// a Closed session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiskConversation {
    pub id: String,
    pub cwd: Option<String>,
    pub last_activity: Option<String>,
    /// Conversation title: the user's `custom-title`, else Claude's `ai-title`.
    pub title: Option<String>,
    /// CLAUDE_CONFIG_DIR (auth profile) the transcript lives under; None = default `~/.claude`.
    pub config_dir: Option<String>,
}

/// The `CLAUDE_CONFIG_DIR` for a slug dir `<profile>/projects/<slug>`, or None for
/// the default `~/.claude` (which needs no explicit env). This is how we resume a
/// conversation under the same auth profile it was created in.
fn profile_config_dir(slug_dir: &Path) -> Option<String> {
    let profile_root = slug_dir.parent()?.parent()?; // <slug> → projects → <profile>
    let name = profile_root.file_name()?.to_str()?;
    if name == ".claude" {
        None
    } else {
        Some(profile_root.to_string_lossy().into_owned())
    }
}

/// Read `(cwd, title)` from a transcript, scanning both the head (cwd + early
/// titles) and the tail (titles set deep in a long conversation), so the most
/// recent title wins. Title prefers the user's `custom-title`, falling back to
/// Claude's auto-generated `ai-title`. Bounded reads keep it cheap.
pub fn read_conversation_meta(path: &Path) -> (Option<String>, Option<String>) {
    let mut cwd = None;
    let mut custom = None; // user-set `custom-title`
    let mut ai = None; // Claude's auto-generated `ai-title`

    // Head: cwd (stamped on early entries) + any titles set near the start
    // (resumed sessions re-stamp their title at the top of the transcript).
    //
    // Bounded by BYTES, not by a line count. A transcript can open with a long
    // metadata preamble — `file-history-snapshot`, `mode`, `agent-name` and friends
    // carry no cwd — and a fixed window then misses the cwd entirely. That was real:
    // one conversation's first cwd sat on line 45 behind 35 snapshot entries, so it
    // scanned as cwd-less, which leaves it unplaceable (no parent → "(unassigned)")
    // and unrecoverable (nothing to `-c` into). Keep reading past the preamble, but
    // stop as soon as the cwd is known and the title window is behind us.
    const HEAD_BYTES: usize = 256 * 1024;
    const TITLE_LINES: usize = 40;
    if let Ok(file) = fs::File::open(path) {
        let mut budget = HEAD_BYTES;
        for (i, line) in BufReader::new(file)
            .lines()
            .map_while(Result::ok)
            .enumerate()
        {
            // Read the line BEFORE spending its bytes. Decrementing first meant a
            // single line bigger than the remaining budget was skipped whole rather
            // than ending the scan after it — and the first substantive entry is
            // exactly where the cwd lives. Measured: one transcript opens with a
            // 576KB line 4 carrying the cwd, against a 256KB budget, so it scanned
            // as cwd-less. That is not cosmetic: no cwd means no `resolve_parent`,
            // which puts the conversation under "(unassigned)" AND makes it
            // unrecoverable, since there is nothing to `-c` into.
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) {
                if cwd.is_none() {
                    if let Some(c) = v.get("cwd").and_then(|c| c.as_str()) {
                        cwd = Some(c.to_string());
                    }
                }
                if let Some(t) = title_field(&v, "custom-title", "customTitle") {
                    custom = Some(t);
                }
                if let Some(t) = title_field(&v, "ai-title", "aiTitle") {
                    ai = Some(t);
                }
            }
            budget = budget.saturating_sub(line.len() + 1);
            if budget == 0 {
                break;
            }
            if cwd.is_some() && i >= TITLE_LINES {
                break;
            }
        }
    }

    // Tail: a title set/updated deep in a long conversation lands well past the
    // head window — scan the tail so the most recent one wins.
    for line in read_tail_lines(&path.to_path_buf(), 65_536) {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if let Some(t) = title_field(&v, "custom-title", "customTitle") {
            custom = Some(t);
        }
        if let Some(t) = title_field(&v, "ai-title", "aiTitle") {
            ai = Some(t);
        }
    }

    // Prefer the user's title; fall back to Claude's auto-generated one.
    (cwd, custom.or(ai))
}

/// The non-empty title string carried by an entry of the given `entry_type`
/// (e.g. `("custom-title", "customTitle")` or `("ai-title", "aiTitle")`).
fn title_field(v: &serde_json::Value, entry_type: &str, field: &str) -> Option<String> {
    if v.get("type").and_then(|t| t.as_str()) != Some(entry_type) {
        return None;
    }
    v.get(field)
        .and_then(|t| t.as_str())
        .filter(|t| !t.is_empty())
        .map(str::to_string)
}

/// Read just the cwd from a transcript (see [`read_conversation_meta`]).
pub fn read_cwd_from_jsonl(path: &Path) -> Option<String> {
    read_conversation_meta(path).0
}

/// Scan the given dirs for `<id>.jsonl` transcripts, capturing id + cwd + mtime.
pub fn scan_disk_conversations_in(dirs: &[PathBuf]) -> Vec<DiskConversation> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for dir in dirs {
        let Ok(entries) = fs::read_dir(dir) else {
            continue;
        };
        // Every conversation in this slug dir shares the dir's auth profile.
        let config_dir = profile_config_dir(dir);
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let Some(id) = path
                .file_stem()
                .and_then(|s| s.to_str())
                .map(str::to_string)
            else {
                continue;
            };
            if !seen.insert(id.clone()) {
                continue;
            }
            let last_activity = fs::metadata(&path)
                .and_then(|m| m.modified())
                .ok()
                .map(|t| chrono::DateTime::<chrono::Utc>::from(t).to_rfc3339());
            let (cwd, title) = read_conversation_meta(&path);
            out.push(DiskConversation {
                id,
                cwd,
                last_activity,
                title,
                config_dir: config_dir.clone(),
            });
        }
    }
    out
}

/// Like [`scan_all_conversation_ids`] but with cwd + mtime for each conversation.
pub fn scan_all_disk_conversations() -> Vec<DiskConversation> {
    scan_disk_conversations_in(&claude_slug_dirs())
}

/// Bump whenever the parse changes what a scan *yields* from an unchanged transcript.
///
/// Reuse is keyed on the transcript's mtime, which answers "did the file change?" — not
/// "did our reading of it change?". Without this, a parser fix is invisible on exactly the
/// conversations it repairs: the file is untouched, so the stale result is served forever.
/// (v2: the head scan is byte-bounded, so a cwd behind a long metadata preamble is found.)
/// (v3: the byte budget is spent AFTER reading a line, so a single line larger than the
/// budget is no longer skipped whole — one transcript's cwd sits on a 576KB line 4.)
const SCAN_PARSER_VERSION: u32 = 3;

/// The on-disk scan cache: id → last scan result. Keyed reuse hinges on `mtime`
/// (== `last_activity`), so an unchanged transcript is never re-parsed.
#[derive(Default, Serialize, Deserialize)]
struct DiskScanCache {
    /// [`SCAN_PARSER_VERSION`] that produced `entries`; a mismatch discards them.
    #[serde(default)]
    version: u32,
    #[serde(default)]
    entries: std::collections::HashMap<String, DiskConversation>,
}

fn scan_cache_path() -> Option<PathBuf> {
    crate::common::persistence::cache_dir().map(|p| p.join("conversation-scan.json"))
}

fn load_scan_cache() -> DiskScanCache {
    let Some(path) = scan_cache_path() else {
        return DiskScanCache::default();
    };
    let Ok(content) = fs::read_to_string(&path) else {
        return DiskScanCache::default();
    };
    let cache: DiskScanCache = serde_json::from_str(&content).unwrap_or_default();
    // Results from an older parser are discarded wholesale — cheaper and more honest than
    // trying to work out which fields the change affected.
    if cache.version != SCAN_PARSER_VERSION {
        return DiskScanCache::default();
    }
    cache
}

fn save_scan_cache(cache: &DiskScanCache) {
    let Some(path) = scan_cache_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(content) = serde_json::to_string(cache) {
        let tmp = path.with_extension("json.tmp");
        if fs::write(&tmp, &content).is_ok() {
            let _ = fs::rename(&tmp, &path);
        }
    }
}

/// Cached counterpart of [`scan_all_disk_conversations`]: stats every transcript
/// (cheap) but only re-parses ones whose mtime changed since the last scan, so a
/// warm scan skips the ~230 head+tail reads. The cache is rebuilt from the current
/// file set each call, so deleted transcripts drop out (no unbounded growth).
pub fn scan_all_disk_conversations_cached() -> Vec<DiskConversation> {
    let dirs = claude_slug_dirs();
    let cache = load_scan_cache();
    let mut out = Vec::new();
    let mut next = DiskScanCache::default();
    let mut seen = std::collections::HashSet::new();

    for dir in &dirs {
        let Ok(entries) = fs::read_dir(dir) else {
            continue;
        };
        let config_dir = profile_config_dir(dir);
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let Some(id) = path
                .file_stem()
                .and_then(|s| s.to_str())
                .map(str::to_string)
            else {
                continue;
            };
            if !seen.insert(id.clone()) {
                continue;
            }
            let mtime = fs::metadata(&path)
                .and_then(|m| m.modified())
                .ok()
                .map(|t| DateTime::<Utc>::from(t).to_rfc3339());

            // Reuse the cached parse only when the mtime is present and unchanged.
            let cached = cache.entries.get(&id).filter(|c| {
                mtime.is_some() && c.last_activity == mtime && c.config_dir == config_dir
            });
            let dc = match cached {
                Some(c) => DiskConversation {
                    id: id.clone(),
                    last_activity: mtime.clone(),
                    ..c.clone()
                },
                None => {
                    let (cwd, title) = read_conversation_meta(&path);
                    DiskConversation {
                        id: id.clone(),
                        cwd,
                        last_activity: mtime.clone(),
                        title,
                        config_dir: config_dir.clone(),
                    }
                }
            };
            next.entries.insert(id, dc.clone());
            out.push(dc);
        }
    }
    next.version = SCAN_PARSER_VERSION;
    save_scan_cache(&next);
    out
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

/// Find the most recently modified jsonl across all profile candidate dirs for a cwd.
pub fn find_latest_jsonl_for_cwd(cwd: &str) -> Option<PathBuf> {
    candidate_projects_paths(cwd)
        .iter()
        .filter_map(find_latest_jsonl)
        .max_by_key(|p| fs::metadata(p).and_then(|m| m.modified()).ok())
}

/// Every transcript id (jsonl basename) for a cwd across all profile dirs, most
/// recently modified first. Used to assign the newest unclaimed conversation to a
/// live Claude window sharing a cwd, when its exact id can't be read from argv.
pub fn list_jsonls_for_cwd_by_recency(cwd: &str) -> Vec<String> {
    let mut entries: Vec<(String, std::time::SystemTime)> = Vec::new();
    for dir in candidate_projects_paths(cwd) {
        let Ok(rd) = fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().map(|x| x == "jsonl").unwrap_or(false) {
                if let (Some(stem), Ok(mtime)) = (
                    p.file_stem().map(|s| s.to_string_lossy().into_owned()),
                    e.metadata().and_then(|m| m.modified()),
                ) {
                    entries.push((stem, mtime));
                }
            }
        }
    }
    entries.sort_by_key(|a| std::cmp::Reverse(a.1));
    entries.into_iter().map(|(id, _)| id).collect()
}

/// Find a specific session's jsonl file (`<session_id>.jsonl`) across profile dirs for a cwd.
/// The Claude `session_id` is exactly the jsonl basename, so this resolves an instance to
/// its own conversation even when several Claude instances share the same working directory.
pub fn find_jsonl_by_session_id(cwd: &str, session_id: &str) -> Option<PathBuf> {
    let filename = format!("{session_id}.jsonl");
    candidate_projects_paths(cwd)
        .into_iter()
        .map(|dir| dir.join(&filename))
        .find(|p| p.is_file())
}

/// Find `<session_id>.jsonl` in any of `dirs`. Path-injectable half of
/// [`find_jsonl_by_session_id_anywhere`], so the cwd-independent lookup is unit-testable
/// without touching a real home dir.
pub fn find_jsonl_by_session_id_in(dirs: &[PathBuf], session_id: &str) -> Option<PathBuf> {
    let filename = format!("{session_id}.jsonl");
    dirs.iter()
        .map(|dir| dir.join(&filename))
        .find(|p| p.is_file())
}

/// Find `<session_id>.jsonl` anywhere under `~/.claude*/projects/`, ignoring the cwd.
///
/// The conversation id is stable; the cwd is not. Claude names the transcript's directory
/// after the dir it was *launched* in, but reports its *current* shell dir in hook payloads —
/// and the agent's own `cd` moves that. Once they diverge, every cwd-derived path is wrong
/// while the id still resolves. One `is_file()` per slug dir, so this is cheap enough to
/// run on the miss path.
pub fn find_jsonl_by_session_id_anywhere(session_id: &str) -> Option<PathBuf> {
    find_jsonl_by_session_id_in(&claude_slug_dirs(), session_id)
}

/// Resolve the jsonl path for a conversation.
///
/// With a known `session_id`: the cwd's own profile dirs first (the common case, one stat),
/// then the same id anywhere on disk (covers a cwd that has drifted from the launch dir).
/// A known id that resolves to nothing returns `None` — **never** the recency fallback, which
/// would silently serve a *different* conversation that happens to share the directory.
/// Without an id there's nothing to be exact about, so the newest transcript for the cwd stands.
pub fn resolve_jsonl_path(cwd: &str, session_id: Option<&str>) -> Option<PathBuf> {
    if let Some(sid) = session_id {
        return find_jsonl_by_session_id(cwd, sid)
            .or_else(|| find_jsonl_by_session_id_anywhere(sid));
    }
    find_latest_jsonl_for_cwd(cwd)
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
        // No hook-event signal. Current native Claude transcripts no longer emit
        // `progress` entries, so fall back to inferring status from the conversation flow.
        _ => infer_status_from_conversation(entries),
    };

    (status, timestamp)
}

/// Infer status from the conversation flow when no hook-event `progress` entries
/// are present (the current native Claude transcript format).
///
/// - Last turn is an assistant message ending in a `text` block → the turn finished
///   and Claude is waiting for input → `Waiting`.
/// - Last turn ends in a `tool_use`, or the last message is a user/tool_result →
///   Claude is still working → `Unknown`.
///
/// Permission/edit/plan states are intentionally not inferred here — those rely on
/// live hook state (state.json); this fallback only distinguishes idle from working.
fn infer_status_from_conversation(entries: &[JsonlEntry]) -> ClaudeStatus {
    // Find the last user/assistant message, skipping system/attachment/mode/etc. entries.
    let last_msg = entries
        .iter()
        .rev()
        .find(|e| e.entry_type == "user" || e.entry_type == "assistant");

    match last_msg {
        Some(e) if e.entry_type == "assistant" => {
            let ends_with_text = e
                .message
                .as_ref()
                .and_then(|m| m.content.as_ref())
                .and_then(|c| c.as_array())
                .and_then(|arr| arr.last())
                .and_then(|block| block.get("type"))
                .and_then(|t| t.as_str())
                == Some("text");
            if ends_with_text {
                ClaudeStatus::Waiting
            } else {
                ClaudeStatus::Unknown
            }
        }
        // Last message is a user/tool_result (Claude is processing) or none found.
        _ => ClaudeStatus::Unknown,
    }
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
    /// A parsed `<task-notification>`, when this user entry is the harness reporting a
    /// background launch's outcome. The raw XML is stripped from `text` — it is ~10KB
    /// of markup per notification and unreadable as a chat bubble.
    pub task: Option<TaskNotification>,
}

/// Compact summary of a tool use.
#[derive(Debug, Clone)]
pub struct ToolSummary {
    pub name: String,
    pub summary: String,
    pub detail: String,
    /// Set for `Workflow` launches: the script's `meta` block, so the card can name the
    /// workflow and list its phases instead of showing 30KB of escaped JavaScript.
    pub workflow: Option<WorkflowMeta>,
}

/// A workflow's `export const meta = {…}`, read from the launch's script source.
#[derive(Debug, Clone, Default)]
pub struct WorkflowMeta {
    pub name: String,
    pub description: String,
    pub phases: Vec<WorkflowPhase>,
}

#[derive(Debug, Clone)]
pub struct WorkflowPhase {
    pub title: String,
    pub detail: String,
}

/// The harness's completion report for a background launch, parsed out of the
/// `<task-notification>` block it injects into the main transcript.
#[derive(Debug, Clone, Default)]
pub struct TaskNotification {
    pub task_id: String,
    /// Matches the launching `tool_use` block's id.
    pub tool_use_id: String,
    /// `completed` | `failed` | `killed` | …
    pub status: String,
    pub summary: String,
    /// The launch's return value — JSON for a workflow, plain text otherwise.
    pub result: String,
    /// Path holding the untruncated output, when the inline result was cut.
    pub output_file: String,
    pub usage: Option<TaskUsage>,
}

/// The `<usage>` block of a workflow notification. Everything here is otherwise
/// invisible in the UI — `agents_error` most of all: a workflow reports `completed`
/// while some of its agents died.
#[derive(Debug, Clone, Default)]
pub struct TaskUsage {
    pub agent_count: u32,
    pub agents_done: u32,
    pub agents_error: u32,
    pub agents_skipped: u32,
    pub subagent_tokens: u64,
    pub tool_uses: u32,
    pub duration_ms: u64,
}

/// A background launch with no `<task-notification>` yet — i.e. still running.
///
/// There is no transcript entry for it (the launch is wherever Claude called the tool,
/// often hundreds of messages back), so the web renders these as synthetic cards
/// appended after the last message: the only honest place for work with no end time.
#[derive(Debug, Clone)]
pub struct RunningTask {
    pub tool_use_id: String,
    pub kind: BackgroundKind,
    /// Workflow name from `meta`, or the launch description.
    pub label: String,
    pub description: String,
    /// ISO 8601 launch time. The elapsed clock is computed client-side from this —
    /// a server-rendered duration would change on every poll and defeat the chat's
    /// byte-comparison re-render guard.
    pub started_at: String,
    pub phases: Vec<WorkflowPhase>,
    /// Agents spawned / finished, counted from the run's journal. Absent for
    /// non-workflow launches and for a run whose journal isn't readable.
    pub agents_started: Option<u32>,
    pub agents_done: Option<u32>,
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
                (desc.to_string(), truncate_str(prompt, 2000))
            }
            "Workflow" => {
                // The card names the workflow and lists its phases (see `workflow`
                // below); the detail is the script itself rather than the input JSON,
                // which renders as one escaped line tens of KB long.
                let meta = workflow_meta_from_input(input);
                let summary = if meta.description.is_empty() {
                    meta.name.clone()
                } else {
                    meta.description.clone()
                };
                (
                    summary,
                    input_str(input, "script").unwrap_or("").to_string(),
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

        let workflow = (name == "Workflow").then(|| workflow_meta_from_input(input));
        tools.push(ToolSummary {
            name,
            summary,
            detail,
            workflow,
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
pub fn extract_conversation_messages(
    lines: &[String],
    max_messages: usize,
) -> Vec<ConversationMessage> {
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

        let mut text = extract_text_from_content(&content).unwrap_or_default();
        let tools = if entry.entry_type == "assistant" {
            extract_tools_from_content(&content)
        } else {
            Vec::new()
        };

        // A completion report, not something the user said: lift it into `task` and
        // drop the markup from `text`. In a workflow-heavy conversation these blocks
        // are ~10KB each and were being rendered verbatim as chat bubbles.
        let task = if entry.entry_type == "user" && text.contains("<task-notification>") {
            let parsed = parse_task_notification(&text);
            text = strip_task_notification(&text);
            parsed
        } else {
            None
        };

        // Skip entries with nothing to show
        if text.is_empty() && tools.is_empty() && task.is_none() {
            continue;
        }

        messages.push(ConversationMessage {
            role: entry.entry_type,
            text,
            tools,
            task,
        });
    }

    // Keep only the last N messages
    if messages.len() > max_messages {
        messages.drain(..messages.len() - max_messages);
    }

    messages
}

/// Get conversation messages for a given project working directory.
/// Reads the full JSONL file and returns all messages.
/// Searches all auth profiles (`~/.claude/projects/` + `~/.claude-*/projects/`).
pub fn get_conversation_messages(cwd: &str) -> Vec<ConversationMessage> {
    get_conversation_messages_for(cwd, None)
}

/// Like [`get_conversation_messages`], but resolves the conversation for a specific Claude
/// `session_id` when known (so multiple instances sharing a cwd map to their own transcripts).
pub fn get_conversation_messages_for(
    cwd: &str,
    session_id: Option<&str>,
) -> Vec<ConversationMessage> {
    let jsonl_path = match resolve_jsonl_path(cwd, session_id) {
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

/// Parse Claude status from jsonl file.
/// Searches all auth profiles (`~/.claude/projects/` + `~/.claude-*/projects/`).
pub fn get_claude_status_from_jsonl(cwd: &str) -> Option<JsonlStatus> {
    get_claude_status_from_jsonl_for(cwd, None)
}

/// Like [`get_claude_status_from_jsonl`], but resolves status for a specific Claude
/// `session_id` when known.
pub fn get_claude_status_from_jsonl_for(
    cwd: &str,
    session_id: Option<&str>,
) -> Option<JsonlStatus> {
    let jsonl_path = resolve_jsonl_path(cwd, session_id)?;

    // Read a generous tail: current transcripts interleave many system/attachment/mode
    // entries between conversational turns, so 10 lines can miss the last assistant message.
    let last_lines = read_last_lines(&jsonl_path, 40);
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

// ---------------------------------------------------------------------------
// Background task detection (workflows / background agents / background bash)
//
// Claude can launch work that runs in the background while the main thread goes
// idle (a `Workflow`, or an `Agent`/`Bash` tool call with `run_in_background`).
// When that happens the main transcript's last entries are the launch followed by
// a `Stop`, so naive status detection reports the session as idle even though work
// is in flight. The harness re-injects a `<task-notification>` into the main
// transcript when the task finishes.
//
// We pair each background launch (a `tool_use` whose name is `Workflow`, or whose
// input carries `run_in_background: true`) with its completion notification using
// the tool-use id: every `<task-notification>` carries a `<tool-use-id>` equal to
// the launching tool_use's id. A launch with no matching notification is still
// running. This is windowing-safe: a completion always follows its launch, so if a
// launch is present in the tail, its completion (if any) is too.
// ---------------------------------------------------------------------------

/// What kind of background work was launched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackgroundKind {
    Workflow,
    Agent,
    Bash,
}

/// A background task launched from the main thread that has not yet reported completion.
#[derive(Debug, Clone)]
pub struct BackgroundTask {
    pub kind: BackgroundKind,
    /// Human-friendly label (workflow name / agent description / bash command).
    pub label: String,
}

/// Pull `<tag>…</tag>` inner values out of a text blob (all occurrences).
fn extract_tagged(text: &str, tag: &str) -> Vec<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(i) = rest.find(&open) {
        let after = &rest[i + open.len()..];
        match after.find(&close) {
            Some(j) => {
                out.push(after[..j].trim().to_string());
                rest = &after[j + close.len()..];
            }
            None => break,
        }
    }
    out
}

/// Best-effort searchable text for a message `content` value (string or block array).
fn content_search_text(content: &serde_json::Value) -> String {
    if let Some(s) = content.as_str() {
        return s.to_string();
    }
    serde_json::to_string(content).unwrap_or_default()
}

/// The quoted string following the first `key` at or after `from` in a JS object
/// literal, plus the offset just past the closing quote (so a scan can continue).
/// Deliberately loose — this reads a `meta` block, not JavaScript.
fn js_string_field(src: &str, key: &str, from: usize) -> Option<(String, usize)> {
    let rel = src.get(from..)?.find(key)?;
    let mut i = from + rel + key.len();
    while i < src.len() && src.as_bytes()[i].is_ascii_whitespace() {
        i += 1;
    }
    let q = src[i..].chars().next()?;
    if q != '\'' && q != '"' && q != '`' {
        return None;
    }
    i += q.len_utf8();
    let end = src[i..].find(q)?;
    Some((src[i..i + end].trim().to_string(), i + end + q.len_utf8()))
}

/// Extract a workflow's `meta.name` from its script source (best effort).
fn extract_js_meta_name(script: &str) -> Option<String> {
    let (name, _) = js_string_field(script, "name:", 0)?;
    if name.is_empty() || name.len() > 80 {
        None
    } else {
        Some(name)
    }
}

/// Extract `meta.phases` — the workflow's plan, which is what makes a launch card
/// worth reading. Bounded to the first `]` after `phases:`; a detail string
/// containing a bracket just truncates the list, which beats scanning JS properly.
fn extract_js_meta_phases(script: &str) -> Vec<WorkflowPhase> {
    let Some(start) = script.find("phases:") else {
        return Vec::new();
    };
    let end = script[start..]
        .find(']')
        .map(|i| start + i)
        .unwrap_or(script.len());
    let slice = &script[start..end];

    let mut out = Vec::new();
    let mut cursor = 0;
    while let Some((title, next)) = js_string_field(slice, "title:", cursor) {
        // A `detail` belongs to this phase only if it precedes the next `title`.
        let next_title = slice[next..].find("title:").map(|i| next + i);
        let detail_at = slice[next..].find("detail:").map(|i| next + i);
        let detail = match (detail_at, next_title) {
            (Some(d), Some(t)) if d > t => String::new(),
            (Some(_), _) => js_string_field(slice, "detail:", next)
                .map(|(v, _)| v)
                .unwrap_or_default(),
            (None, _) => String::new(),
        };
        out.push(WorkflowPhase { title, detail });
        cursor = next;
        if out.len() >= 16 {
            break;
        }
    }
    out
}

/// The workflow `meta` behind a `Workflow` launch's script input.
fn workflow_meta_from_input(input: Option<&serde_json::Value>) -> WorkflowMeta {
    let script = input_str(input, "script").unwrap_or("");
    WorkflowMeta {
        name: extract_js_meta_name(script)
            .or_else(|| input_str(input, "name").map(str::to_string))
            .unwrap_or_else(|| "workflow".to_string()),
        description: input_str(input, "description").unwrap_or("").to_string(),
        phases: extract_js_meta_phases(script),
    }
}

/// The first `<tag>…</tag>` value in `text`, untrimmed of inner markup.
fn tag_value(text: &str, tag: &str) -> Option<String> {
    extract_tagged(text, tag).into_iter().next()
}

fn tag_num<T: std::str::FromStr + Default>(text: &str, tag: &str) -> T {
    tag_value(text, tag)
        .and_then(|v| v.trim().parse::<T>().ok())
        .unwrap_or_default()
}

/// Parse a `<task-notification>` block. Returns None when `text` has no such block.
pub fn parse_task_notification(text: &str) -> Option<TaskNotification> {
    let block = tag_value(text, "task-notification")?;
    let usage = tag_value(&block, "usage").map(|u| TaskUsage {
        agent_count: tag_num(&u, "agent_count"),
        agents_done: tag_num(&u, "agents_done"),
        agents_error: tag_num(&u, "agents_error"),
        agents_skipped: tag_num(&u, "agents_skipped"),
        subagent_tokens: tag_num(&u, "subagent_tokens"),
        tool_uses: tag_num(&u, "tool_uses"),
        duration_ms: tag_num(&u, "duration_ms"),
    });
    Some(TaskNotification {
        task_id: tag_value(&block, "task-id").unwrap_or_default(),
        tool_use_id: tag_value(&block, "tool-use-id").unwrap_or_default(),
        status: tag_value(&block, "status").unwrap_or_default(),
        summary: tag_value(&block, "summary").unwrap_or_default(),
        result: tag_value(&block, "result").unwrap_or_default(),
        output_file: tag_value(&block, "output-file").unwrap_or_default(),
        usage,
    })
}

/// `text` with the `<task-notification>…</task-notification>` block removed. What's
/// left is whatever the harness wrote around it (usually nothing).
fn strip_task_notification(text: &str) -> String {
    let (Some(start), Some(end)) = (
        text.find("<task-notification>"),
        text.find("</task-notification>"),
    ) else {
        return text.to_string();
    };
    let mut out = String::with_capacity(text.len());
    out.push_str(&text[..start]);
    out.push_str(&text[end + "</task-notification>".len()..]);
    out.trim().to_string()
}

fn input_str<'a>(input: Option<&'a serde_json::Value>, key: &str) -> Option<&'a str> {
    input.and_then(|i| i.get(key)).and_then(|v| v.as_str())
}

/// Classify a `tool_use` block as a background launch, returning the task if so.
fn background_task_from_tool_use(tool: &ToolUse) -> Option<BackgroundTask> {
    if tool.content_type != "tool_use" {
        return None;
    }
    let name = tool.name.as_deref().unwrap_or("");
    let run_in_background = tool
        .input
        .as_ref()
        .and_then(|i| i.get("run_in_background"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    if name == "Workflow" {
        let label = input_str(tool.input.as_ref(), "script")
            .and_then(extract_js_meta_name)
            .unwrap_or_else(|| "workflow".to_string());
        Some(BackgroundTask {
            kind: BackgroundKind::Workflow,
            label,
        })
    } else if run_in_background && name == "Bash" {
        let label = input_str(tool.input.as_ref(), "description")
            .or_else(|| input_str(tool.input.as_ref(), "command"))
            .map(|s| truncate_str(s, 50))
            .unwrap_or_else(|| "command".to_string());
        Some(BackgroundTask {
            kind: BackgroundKind::Bash,
            label,
        })
    } else if run_in_background {
        // Agent (the common case) or any other backgrounded tool.
        let label = input_str(tool.input.as_ref(), "description")
            .or_else(|| input_str(tool.input.as_ref(), "subagent_type"))
            .map(|s| truncate_str(s, 50))
            .unwrap_or_else(|| "agent".to_string());
        Some(BackgroundTask {
            kind: BackgroundKind::Agent,
            label,
        })
    } else {
        None
    }
}

/// Detect background tasks that are still running, given transcript entries in
/// chronological order. Pure (no IO) for testability.
pub fn detect_active_background_tasks(entries: &[JsonlEntry]) -> Vec<BackgroundTask> {
    use std::collections::HashSet;

    let mut launched: Vec<(String, BackgroundTask)> = Vec::new();
    let mut done: HashSet<String> = HashSet::new();

    for entry in entries {
        match entry.entry_type.as_str() {
            "assistant" => {
                let Some(blocks) = entry
                    .message
                    .as_ref()
                    .and_then(|m| m.content.as_ref())
                    .and_then(|c| c.as_array())
                else {
                    continue;
                };
                for block in blocks {
                    let Ok(tool) = serde_json::from_value::<ToolUse>(block.clone()) else {
                        continue;
                    };
                    if let (Some(id), Some(task)) =
                        (tool.id.clone(), background_task_from_tool_use(&tool))
                    {
                        launched.push((id, task));
                    }
                }
            }
            "user" => {
                let Some(content) = entry.message.as_ref().and_then(|m| m.content.as_ref()) else {
                    continue;
                };
                let text = content_search_text(content);
                if text.contains("<task-notification>") {
                    for id in extract_tagged(&text, "tool-use-id") {
                        done.insert(id);
                    }
                }
            }
            _ => {}
        }
    }

    launched
        .into_iter()
        .filter(|(id, _)| !done.contains(id))
        .map(|(_, task)| task)
        .collect()
}

/// One-line summary of in-flight background tasks (for status display).
pub fn background_tasks_summary(tasks: &[BackgroundTask]) -> String {
    match tasks {
        [] => String::new(),
        [t] => match t.kind {
            BackgroundKind::Workflow => format!("workflow: {}", t.label),
            BackgroundKind::Agent => format!("agent: {}", t.label),
            BackgroundKind::Bash => format!("bg: {}", t.label),
        },
        many => {
            if many.iter().all(|t| t.kind == BackgroundKind::Workflow) {
                format!("{} workflows", many.len())
            } else {
                format!("{} background tasks", many.len())
            }
        }
    }
}

/// The run's transcript dir, named by the launch's tool_result stub:
/// `Workflow launched in background. Task ID: … \n Transcript dir: <path> \n Script file: …`.
/// This is the only place the runId appears while the workflow is still in flight —
/// the run record (`workflows/wf_<id>.json`) isn't written until it finishes.
fn transcript_dir_from_stub(body: &str) -> Option<String> {
    const KEY: &str = "Transcript dir:";
    let idx = body.find(KEY)?;
    let rest = &body[idx + KEY.len()..];
    // `body` may be a JSON-serialized block, so the line can end at an escaped
    // newline and carry a closing quote.
    let line = rest.lines().next()?;
    let line = line.split("\\n").next().unwrap_or(line);
    let dir = line.trim().trim_end_matches(['"', ',']).trim();
    (!dir.is_empty()).then(|| dir.to_string())
}

/// Agents spawned / finished for a workflow run, from its journal — one `started`
/// line per agent launched and one `result` line per agent that returned.
pub fn journal_progress(run_dir: &Path) -> Option<(u32, u32)> {
    let content = fs::read_to_string(run_dir.join("journal.jsonl")).ok()?;
    let (mut started, mut done) = (0, 0);
    for line in content.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        match v.get("type").and_then(|t| t.as_str()) {
            Some("started") => started += 1,
            Some("result") => done += 1,
            _ => {}
        }
    }
    Some((started, done))
}

/// Background launches with no `<task-notification>` yet, enriched for display:
/// workflow name and phases from the script, launch time, and agent progress read
/// from the run's journal. Same launch/completion pairing as
/// [`detect_active_background_tasks`] — see that function for why it is windowing-safe.
pub fn detect_running_tasks(entries: &[JsonlEntry]) -> Vec<RunningTask> {
    use std::collections::{HashMap, HashSet};

    let mut launched: Vec<RunningTask> = Vec::new();
    let mut done: HashSet<String> = HashSet::new();
    let mut run_dirs: HashMap<String, String> = HashMap::new();

    for entry in entries {
        let Some(content) = entry.message.as_ref().and_then(|m| m.content.as_ref()) else {
            continue;
        };
        match entry.entry_type.as_str() {
            "assistant" => {
                let Some(blocks) = content.as_array() else {
                    continue;
                };
                for block in blocks {
                    let Ok(tool) = serde_json::from_value::<ToolUse>(block.clone()) else {
                        continue;
                    };
                    let (Some(id), Some(task)) =
                        (tool.id.clone(), background_task_from_tool_use(&tool))
                    else {
                        continue;
                    };
                    let input = tool.input.as_ref();
                    let (label, description, phases) = if task.kind == BackgroundKind::Workflow {
                        let meta = workflow_meta_from_input(input);
                        (meta.name, meta.description, meta.phases)
                    } else {
                        let desc = input_str(input, "description").unwrap_or("").to_string();
                        (task.label.clone(), desc, Vec::new())
                    };
                    launched.push(RunningTask {
                        tool_use_id: id,
                        kind: task.kind,
                        label,
                        description,
                        started_at: entry.timestamp.clone().unwrap_or_default(),
                        phases,
                        agents_started: None,
                        agents_done: None,
                    });
                }
            }
            "user" => {
                let text = content_search_text(content);
                if text.contains("<task-notification>") {
                    for id in extract_tagged(&text, "tool-use-id") {
                        done.insert(id);
                    }
                }
                let Some(blocks) = content.as_array() else {
                    continue;
                };
                for block in blocks {
                    if block.get("type").and_then(|t| t.as_str()) != Some("tool_result") {
                        continue;
                    }
                    let Some(id) = block.get("tool_use_id").and_then(|v| v.as_str()) else {
                        continue;
                    };
                    let body = block
                        .get("content")
                        .map(content_search_text)
                        .unwrap_or_default();
                    if let Some(dir) = transcript_dir_from_stub(&body) {
                        run_dirs.insert(id.to_string(), dir);
                    }
                }
            }
            _ => {}
        }
    }

    launched.retain(|t| !done.contains(&t.tool_use_id));
    for task in &mut launched {
        if let Some(dir) = run_dirs.get(&task.tool_use_id) {
            if let Some((started, finished)) = journal_progress(Path::new(dir)) {
                task.agents_started = Some(started);
                task.agents_done = Some(finished);
            }
        }
    }
    launched
}

/// Read the transcript tail and return in-flight background launches for display.
pub fn get_running_tasks_for(cwd: &str, session_id: Option<&str>) -> Vec<RunningTask> {
    let Some(path) = resolve_jsonl_path(cwd, session_id) else {
        return Vec::new();
    };
    // Same bounded tail as the status detection: a running launch is near the end by
    // definition, and this runs on every 2s chat poll.
    let lines = read_tail_lines(&path, 256 * 1024);
    let entries: Vec<JsonlEntry> = lines
        .iter()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    detect_running_tasks(&entries)
}

/// Read the transcript tail and return in-flight background tasks for a conversation.
/// Resolves the jsonl by `session_id` when known, else the latest jsonl for `cwd`.
pub fn get_active_background_tasks_for(cwd: &str, session_id: Option<&str>) -> Vec<BackgroundTask> {
    let Some(path) = resolve_jsonl_path(cwd, session_id) else {
        return Vec::new();
    };
    // A running task's launch sits at the tail (main thread idle after it), so a
    // bounded byte-tail is sufficient and avoids reading huge transcripts in full.
    let lines = read_tail_lines(&path, 256 * 1024);
    if lines.is_empty() {
        return Vec::new();
    }
    let entries: Vec<JsonlEntry> = lines
        .iter()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    detect_active_background_tasks(&entries)
}

/// Convenience: a status summary string if a conversation has in-flight background
/// work, else `None`. Used to override an otherwise-idle status with a busy one.
pub fn background_running_summary(cwd: &str, session_id: Option<&str>) -> Option<String> {
    let tasks = get_active_background_tasks_for(cwd, session_id);
    if tasks.is_empty() {
        None
    } else {
        Some(background_tasks_summary(&tasks))
    }
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
    fn test_candidate_projects_paths_filters_nonexistent() {
        // With a cwd that definitely has no conversation history, we should get zero candidates
        // (the primary path doesn't exist either). This verifies the .exists() filter works.
        let paths = candidate_projects_paths("/definitely/does/not/exist/42");
        assert!(paths.is_empty(), "expected no candidates, got: {:?}", paths);
    }

    #[test]
    fn test_scan_returns_session_ids() {
        let dir = std::env::temp_dir().join(format!("hive-scan-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("11111111-aaaa.jsonl"), "{}").unwrap();
        std::fs::write(dir.join("22222222-bbbb.jsonl"), "{}").unwrap();
        std::fs::write(dir.join("notes.txt"), "ignore me").unwrap();

        let ids = scan_conversation_ids_in(std::slice::from_ref(&dir));
        std::fs::remove_dir_all(&dir).ok();

        // Only the two .jsonl basenames, sorted; the .txt file is ignored.
        assert_eq!(
            ids,
            vec!["11111111-aaaa".to_string(), "22222222-bbbb".to_string()]
        );
    }

    #[test]
    fn test_scan_disk_sessions_reads_cwd_and_mtime() {
        let dir = std::env::temp_dir().join(format!("hive-disk-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("abc-123.jsonl"),
            "{\"type\":\"custom-title\",\"customTitle\":\"My Task\"}\n{\"type\":\"user\",\"cwd\":\"/home/u/hive\"}\n",
        )
        .unwrap();

        let sessions = scan_disk_conversations_in(std::slice::from_ref(&dir));
        std::fs::remove_dir_all(&dir).ok();

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "abc-123");
        assert_eq!(sessions[0].cwd.as_deref(), Some("/home/u/hive"));
        assert_eq!(sessions[0].title.as_deref(), Some("My Task"));
        assert!(sessions[0].last_activity.is_some()); // file mtime → RFC3339
    }

    #[test]
    fn test_read_conversation_meta_finds_late_title() {
        // A rename deep in a long conversation lands past the 40-line head window;
        // the tail scan must still pick it up (the "Market watcher" bug).
        let dir = std::env::temp_dir().join(format!("hive-latetitle-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("late.jsonl");
        let mut body = String::new();
        for _ in 0..60 {
            body.push_str("{\"type\":\"user\",\"cwd\":\"/home/u/eve\"}\n");
        }
        body.push_str("{\"type\":\"custom-title\",\"customTitle\":\"Market watcher\"}\n");
        std::fs::write(&path, body).unwrap();

        let (cwd, title) = read_conversation_meta(&path);
        std::fs::remove_dir_all(&dir).ok();

        assert_eq!(cwd.as_deref(), Some("/home/u/eve"));
        assert_eq!(title.as_deref(), Some("Market watcher"));
    }

    #[test]
    fn test_read_conversation_meta_looks_past_a_metadata_preamble() {
        // A transcript can open with dozens of `file-history-snapshot` entries before the
        // first one carrying a cwd. Observed in the wild: the first cwd on line 45, behind
        // 35 snapshot entries — the old 40-line head window stopped just short, so the
        // conversation scanned as cwd-less and became unplaceable AND unrecoverable.
        let dir = std::env::temp_dir().join(format!("hive-preamble-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("preamble.jsonl");
        let mut body = String::new();
        body.push_str("{\"type\":\"ai-title\",\"aiTitle\":\"Claude code artifact review\"}\n");
        for _ in 0..44 {
            body.push_str("{\"type\":\"file-history-snapshot\",\"snapshot\":{}}\n");
        }
        body.push_str("{\"type\":\"user\",\"cwd\":\"/home/u/wt/live-avatar/voice-agent\"}\n");
        std::fs::write(&path, body).unwrap();

        let (cwd, title) = read_conversation_meta(&path);
        std::fs::remove_dir_all(&dir).ok();

        assert_eq!(
            cwd.as_deref(),
            Some("/home/u/wt/live-avatar/voice-agent"),
            "cwd must be found behind the preamble"
        );
        assert_eq!(title.as_deref(), Some("Claude code artifact review"));
    }

    #[test]
    fn test_read_conversation_meta_reads_a_line_larger_than_the_head_budget() {
        // A single entry can dwarf the whole head budget — a pasted file, a big tool
        // result. Observed: a transcript whose line 4 is 576KB against a 256KB budget,
        // and that line is the first one carrying a cwd. Spending the budget *before*
        // reading skipped it whole, so the conversation scanned as cwd-less: unplaceable
        // under "(unassigned)" and unrecoverable. The budget must bound how far the scan
        // CONTINUES, never which lines it looks at.
        let dir = std::env::temp_dir().join(format!("hive-bigline-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bigline.jsonl");

        let filler = "x".repeat(600 * 1024); // comfortably over the 256KB head budget
        let mut body = String::new();
        body.push_str("{\"type\":\"mode\",\"mode\":\"normal\"}\n");
        body.push_str(&format!(
            "{{\"type\":\"user\",\"cwd\":\"/home/u/projects/media\",\"pasted\":\"{filler}\"}}\n"
        ));
        body.push_str("{\"type\":\"ai-title\",\"aiTitle\":\"Message routing\"}\n");
        std::fs::write(&path, body).unwrap();

        let (cwd, title) = read_conversation_meta(&path);
        std::fs::remove_dir_all(&dir).ok();

        assert_eq!(
            cwd.as_deref(),
            Some("/home/u/projects/media"),
            "an oversized line must still be parsed, not skipped"
        );
        // The tail scan picks the title up even though the head budget ran out on the
        // big line, which is exactly the division of labour the two passes are for.
        assert_eq!(title.as_deref(), Some("Message routing"));
    }

    #[test]
    fn test_scan_captures_auth_profile_config_dir() {
        let base = std::env::temp_dir().join(format!("hive-cfg-{}", std::process::id()));
        // Alt profile: <base>/.claude-work/projects/<slug>/<id>.jsonl
        let work_slug = base.join(".claude-work").join("projects").join("slugw");
        std::fs::create_dir_all(&work_slug).unwrap();
        std::fs::write(
            work_slug.join("w1.jsonl"),
            "{\"type\":\"user\",\"cwd\":\"/x\"}\n",
        )
        .unwrap();
        // Default profile: <base>/.claude/projects/<slug>/<id>.jsonl
        let def_slug = base.join(".claude").join("projects").join("slugd");
        std::fs::create_dir_all(&def_slug).unwrap();
        std::fs::write(
            def_slug.join("d1.jsonl"),
            "{\"type\":\"user\",\"cwd\":\"/y\"}\n",
        )
        .unwrap();

        let work = scan_disk_conversations_in(std::slice::from_ref(&work_slug));
        let def = scan_disk_conversations_in(std::slice::from_ref(&def_slug));
        std::fs::remove_dir_all(&base).ok();

        assert_eq!(work.len(), 1);
        assert_eq!(
            work[0].config_dir.as_deref(),
            Some(base.join(".claude-work").to_string_lossy().as_ref())
        );
        assert_eq!(def.len(), 1);
        assert_eq!(def[0].config_dir, None); // default profile → no CLAUDE_CONFIG_DIR
    }

    #[test]
    fn test_parse_task_notification_reads_usage_and_ignores_prose() {
        let text = "<task-notification>\n<task-id>w1</task-id>\n<tool-use-id>toolu_a</tool-use-id>\
            \n<status>completed</status>\n<summary>Dynamic workflow \"Ship it\" completed</summary>\
            \n<result>{\"a\":1}</result>\n<usage><agent_count>5</agent_count><agents_done>4</agents_done>\
            <agents_error>1</agents_error><subagent_tokens>1274902</subagent_tokens>\
            <tool_uses>598</tool_uses><duration_ms>10327450</duration_ms></usage>\n</task-notification>";
        let n = parse_task_notification(text).expect("parses");
        assert_eq!(n.tool_use_id, "toolu_a");
        assert_eq!(n.status, "completed");
        assert_eq!(n.result, "{\"a\":1}");
        let u = n.usage.expect("usage block");
        assert_eq!(u.agent_count, 5);
        // Reported "completed" with a dead agent inside — the number with no other surface.
        assert_eq!(u.agents_error, 1);
        assert_eq!(u.subagent_tokens, 1_274_902);
        assert_eq!(u.duration_ms, 10_327_450);
        assert!(
            strip_task_notification(text).is_empty(),
            "markup leaves the bubble"
        );

        // A skill's instructions that merely *mention* the tag must not be mistaken for
        // one (the corpus has such a message): no closing tag, so nothing parses and
        // nothing is stripped.
        let prose = "the harness injects a <task-notification> when the task finishes";
        assert!(parse_task_notification(prose).is_none());
        assert_eq!(strip_task_notification(prose), prose);
    }

    #[test]
    fn test_detect_running_tasks_until_the_notification_arrives() {
        let launch = r#"{"type":"assistant","timestamp":"2026-08-14T01:00:00Z","message":{"content":[{"type":"tool_use","id":"toolu_wf1","name":"Workflow","input":{"description":"Sweep the rewrite","script":"export const meta = {\n  name: 'spa-sweep',\n  phases: [{ title: 'Build', detail: 'do it' }, { title: 'Review' }],\n}\n"}}]}}"#;
        // The launch's tool_result stub is the only place the run dir appears while the
        // workflow is in flight — the run record isn't written until it finishes.
        let stub = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_wf1","content":"Workflow launched in background. Task ID: w1\nTranscript dir: /tmp/hive-no-such-run\nScript file: /tmp/x.js"}]}}"#;
        let notif = r#"{"type":"user","message":{"content":"<task-notification><tool-use-id>toolu_wf1</tool-use-id><status>completed</status></task-notification>"}}"#;

        let running = detect_running_tasks(&[parse_entry(launch), parse_entry(stub)]);
        assert_eq!(running.len(), 1);
        let t = &running[0];
        assert_eq!(t.label, "spa-sweep", "name comes from the script's meta");
        assert_eq!(t.description, "Sweep the rewrite");
        assert_eq!(
            t.started_at, "2026-08-14T01:00:00Z",
            "the client's elapsed clock"
        );
        let titles: Vec<&str> = t.phases.iter().map(|p| p.title.as_str()).collect();
        assert_eq!(titles, ["Build", "Review"]);
        assert_eq!(t.phases[0].detail, "do it");
        assert_eq!(t.phases[1].detail, "", "a phase without a detail gets none");
        assert!(
            t.agents_started.is_none(),
            "no journal on disk → no agent counts"
        );

        let after =
            detect_running_tasks(&[parse_entry(launch), parse_entry(stub), parse_entry(notif)]);
        assert!(after.is_empty(), "a notified launch is no longer running");
    }

    #[test]
    fn test_journal_progress_counts_spawned_and_returned() {
        let dir = std::env::temp_dir().join(format!("hive-journal-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("journal.jsonl"),
            "{\"type\":\"started\",\"agentId\":\"a1\"}\n\
             {\"type\":\"started\",\"agentId\":\"a2\"}\n\
             {\"type\":\"result\",\"agentId\":\"a1\",\"result\":{}}\n",
        )
        .unwrap();
        let got = journal_progress(&dir);
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(got, Some((2, 1)), "2 spawned, 1 returned → 1 still running");
    }

    #[test]
    fn test_find_jsonl_by_session_id_in_survives_cwd_drift() {
        // The transcript is filed under the dir Claude was LAUNCHED in. When the agent cd's
        // into a subdir, the cwd-derived slug points at a sibling that holds no transcript —
        // but Claude does create a same-named *directory* there for its own sidecars, so the
        // lookup must insist on a file. (The `experiment-crisis-colombia` blank-page bug.)
        let base = std::env::temp_dir().join(format!("hive-drift-{}", std::process::id()));
        let launch = base.join(".claude").join("projects").join("-w-proj");
        let drifted = base.join(".claude").join("projects").join("-w-proj-sub");
        std::fs::create_dir_all(&launch).unwrap();
        std::fs::create_dir_all(drifted.join("conv-1.jsonl")).unwrap(); // a DIRECTORY
        std::fs::write(
            launch.join("conv-1.jsonl"),
            "{\"type\":\"user\",\"cwd\":\"/w/proj\"}\n",
        )
        .unwrap();

        // Drifted dir first: the id must still resolve to the real transcript behind it.
        let dirs = vec![drifted.clone(), launch.clone()];
        let found = find_jsonl_by_session_id_in(&dirs, "conv-1");
        let missing = find_jsonl_by_session_id_in(&dirs, "conv-2");
        std::fs::remove_dir_all(&base).ok();

        assert_eq!(found, Some(launch.join("conv-1.jsonl")));
        // A known id that isn't on disk resolves to nothing — it never substitutes the
        // neighbouring transcript, which is how the wrong conversation would get served.
        assert_eq!(missing, None);
    }

    #[test]
    fn test_ai_title_fallback_and_custom_precedence() {
        let dir = std::env::temp_dir().join(format!("hive-aititle-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        // Only an ai-title → used as the fallback.
        let ai_only = dir.join("ai.jsonl");
        std::fs::write(
            &ai_only,
            "{\"type\":\"user\",\"cwd\":\"/x\"}\n{\"type\":\"ai-title\",\"aiTitle\":\"Repo structure\"}\n",
        )
        .unwrap();
        assert_eq!(
            read_conversation_meta(&ai_only).1.as_deref(),
            Some("Repo structure")
        );

        // Both present → the user's custom title wins.
        let both = dir.join("both.jsonl");
        std::fs::write(
            &both,
            "{\"type\":\"ai-title\",\"aiTitle\":\"Auto name\"}\n{\"type\":\"custom-title\",\"customTitle\":\"My name\"}\n",
        )
        .unwrap();
        assert_eq!(read_conversation_meta(&both).1.as_deref(), Some("My name"));

        std::fs::remove_dir_all(&dir).ok();
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
    fn test_idle_inferred_from_trailing_assistant_text() {
        // Current native transcripts have no `progress` entries. An assistant message
        // ending in a text block means the turn finished → Waiting.
        let user = r#"{"type":"user","message":{"content":"do the thing"},"timestamp":"2026-01-29T10:00:00Z"}"#;
        let thinking =
            r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":"hmm"}]}}"#;
        let text = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"All done."}]},"timestamp":"2026-01-29T10:00:05Z"}"#;
        let system = r#"{"type":"system","timestamp":"2026-01-29T10:00:06Z"}"#;
        let entries = vec![
            parse_entry(user),
            parse_entry(thinking),
            parse_entry(text),
            parse_entry(system),
        ];
        let (status, _) = parse_status_from_entries(&entries);
        assert!(
            matches!(status, ClaudeStatus::Waiting),
            "expected Waiting, got {:?}",
            status
        );
    }

    #[test]
    fn test_working_inferred_from_trailing_tool_use() {
        // No progress entries; last assistant block is a tool_use → still working.
        let text = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Let me check."}]}}"#;
        let tool = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"ls"}}]}}"#;
        let entries = vec![parse_entry(text), parse_entry(tool)];
        let (status, _) = parse_status_from_entries(&entries);
        assert!(
            matches!(status, ClaudeStatus::Unknown),
            "expected Unknown/working, got {:?}",
            status
        );
    }

    #[test]
    fn test_working_inferred_from_trailing_user_message() {
        // No progress entries; last conversational message is a user/tool_result → working.
        let text = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hi"}]}}"#;
        let user = r#"{"type":"user","message":{"content":"next request"}}"#;
        let entries = vec![parse_entry(text), parse_entry(user)];
        let (status, _) = parse_status_from_entries(&entries);
        assert!(
            matches!(status, ClaudeStatus::Unknown),
            "expected Unknown/working, got {:?}",
            status
        );
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
    fn test_no_progress_trailing_text_is_waiting() {
        // Without progress entries, a trailing assistant text block means the turn
        // finished and Claude is idle → Waiting (current native transcript format).
        let assistant =
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Hello"}]}}"#;
        let entries = vec![parse_entry(assistant)];
        let (status, _) = parse_status_from_entries(&entries);
        assert!(matches!(status, ClaudeStatus::Waiting));
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
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Reply 1"}]}}"#
                .to_string(),
            r#"{"type":"user","message":{"content":[{"type":"text","text":"Second"}]}}"#
                .to_string(),
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Reply 2"}]}}"#
                .to_string(),
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

    // --- background task detection ---

    fn lines_to_entries(lines: &[&str]) -> Vec<JsonlEntry> {
        lines.iter().map(|l| parse_entry(l)).collect()
    }

    #[test]
    fn test_background_workflow_running() {
        let launch = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_wf1","name":"Workflow","input":{"script":"export const meta = {\n  name: 'audit-flows',\n}\n"}}]}}"#;
        let stop = r#"{"type":"user","message":{"content":"Workflow launched in background. Task ID: w5fpwhv2l\nSummary: audit"}}"#;
        let entries = lines_to_entries(&[launch, stop]);
        let tasks = detect_active_background_tasks(&entries);
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].kind, BackgroundKind::Workflow);
        assert_eq!(tasks[0].label, "audit-flows");
        assert_eq!(background_tasks_summary(&tasks), "workflow: audit-flows");
    }

    #[test]
    fn test_background_workflow_completed_not_flagged() {
        let launch = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_wf1","name":"Workflow","input":{"script":"name: 'audit'"}}]}}"#;
        let notif = r#"{"type":"user","message":{"content":"<task-notification>\n<task-id>w5fpwhv2l</task-id>\n<tool-use-id>toolu_wf1</tool-use-id>\n<status>completed</status>\n</task-notification>"}}"#;
        let entries = lines_to_entries(&[launch, notif]);
        assert!(detect_active_background_tasks(&entries).is_empty());
    }

    #[test]
    fn test_background_agent_running() {
        let launch = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_ag1","name":"Agent","input":{"description":"Fix the build","run_in_background":true}}]}}"#;
        let entries = lines_to_entries(&[launch]);
        let tasks = detect_active_background_tasks(&entries);
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].kind, BackgroundKind::Agent);
        assert_eq!(tasks[0].label, "Fix the build");
    }

    #[test]
    fn test_background_failed_status_marks_done() {
        // Any terminal status (failed/killed/completed) means the task is no longer running.
        let launch = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_ag1","name":"Agent","input":{"description":"x","run_in_background":true}}]}}"#;
        let notif = r#"{"type":"user","message":{"content":"<task-notification>\n<tool-use-id>toolu_ag1</tool-use-id>\n<status>failed</status>\n</task-notification>"}}"#;
        let entries = lines_to_entries(&[launch, notif]);
        assert!(detect_active_background_tasks(&entries).is_empty());
    }

    #[test]
    fn test_background_mixed_one_done_one_running() {
        let wf_done = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_wf1","name":"Workflow","input":{"script":"name: 'first'"}}]}}"#;
        let notif = r#"{"type":"user","message":{"content":"<task-notification><tool-use-id>toolu_wf1</tool-use-id><status>completed</status></task-notification>"}}"#;
        let wf_running = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_wf2","name":"Workflow","input":{"script":"name: 'second'"}}]}}"#;
        let entries = lines_to_entries(&[wf_done, notif, wf_running]);
        let tasks = detect_active_background_tasks(&entries);
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].label, "second");
    }

    #[test]
    fn test_foreground_bash_ignored() {
        // A Bash call without run_in_background is foreground work, not a background task.
        let bash = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_b1","name":"Bash","input":{"command":"ls"}}]}}"#;
        let entries = lines_to_entries(&[bash]);
        assert!(detect_active_background_tasks(&entries).is_empty());
    }

    #[test]
    fn test_background_bash_running() {
        let bash = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_b1","name":"Bash","input":{"command":"npm run dev","description":"dev server","run_in_background":true}}]}}"#;
        let entries = lines_to_entries(&[bash]);
        let tasks = detect_active_background_tasks(&entries);
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].kind, BackgroundKind::Bash);
        assert_eq!(background_tasks_summary(&tasks), "bg: dev server");
    }

    #[test]
    fn test_background_summary_multiple_workflows() {
        let tasks = vec![
            BackgroundTask {
                kind: BackgroundKind::Workflow,
                label: "a".into(),
            },
            BackgroundTask {
                kind: BackgroundKind::Workflow,
                label: "b".into(),
            },
        ];
        assert_eq!(background_tasks_summary(&tasks), "2 workflows");
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
