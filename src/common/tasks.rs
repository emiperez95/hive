//! Per-project task boards, backed by Apple Reminders.
//!
//! Not the conversation board: a *task* is a reminder in the project's Reminders list,
//! and the conversation working on it is optional (`conversation:` in its header).
//!
//! A reminder's notes start with a `key: value` header; `status` is the board column:
//!
//! ```text
//! status: doing
//! area: onboarding
//! ---
//! ## Goal …
//! ```
//!
//! Everything but the one key being changed is preserved byte-for-byte on a write —
//! the header grows its own dialects per list (Thesis: `epic`, `blocked-by`; AWS: `type`,
//! `size`, `repo`), and the body is free markdown.

use crate::common::projects::{ProjectRegistry, TasksConfig};
use crate::common::task_schema::{self, Issue};
use anyhow::{anyhow, bail, Context, Result};
use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Completed tasks shown in the done column (the list can hold years of them).
const MAX_DONE: usize = 15;
const CACHE_TTL: Duration = Duration::from_secs(4);
const CALL_TIMEOUT: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------- notes header

#[derive(Debug, Clone, PartialEq)]
struct HeaderLine {
    key: String,
    value: String,
    /// The line as written, so an untouched line round-trips exactly.
    raw: String,
}

/// A reminder's notes split into the `key: value` header and everything after it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Notes {
    header: Vec<HeaderLine>,
    /// Everything after the header (starting at the `---` line, if any), verbatim.
    rest: Option<String>,
}

/// `key: value` — the key is a word, and the colon is followed by a space or the end
/// of the line, so `https://…` or prose with a colon never reads as a field.
fn parse_kv(line: &str) -> Option<(&str, &str)> {
    let (k, v) = line.split_once(':')?;
    if k.is_empty()
        || !k
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '-')
    {
        return None;
    }
    if !(v.is_empty() || v.starts_with(' ')) {
        return None;
    }
    Some((k, v.trim()))
}

impl Notes {
    pub fn parse(s: &str) -> Notes {
        let lines: Vec<&str> = s.split('\n').collect();
        let mut header = Vec::new();
        for l in &lines {
            match parse_kv(l) {
                Some((k, v)) => header.push(HeaderLine {
                    key: k.to_string(),
                    value: v.to_string(),
                    raw: l.to_string(),
                }),
                None => break,
            }
        }
        let n = header.len();
        let rest = (n < lines.len()).then(|| lines[n..].join("\n"));
        Notes { header, rest }
    }

    pub fn render(&self) -> String {
        let mut out = self
            .header
            .iter()
            .map(|h| h.raw.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        if let Some(r) = &self.rest {
            if !self.header.is_empty() {
                out.push('\n');
            }
            out.push_str(r);
        }
        out
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.header
            .iter()
            .find(|h| h.key.eq_ignore_ascii_case(key))
            .map(|h| h.value.as_str())
    }

    pub fn fields(&self) -> Vec<(String, String)> {
        self.header
            .iter()
            .map(|h| (h.key.clone(), h.value.clone()))
            .collect()
    }

    /// Set `key`, in place when it exists, else appended to the header.
    pub fn set(&mut self, key: &str, value: &str) {
        let raw = format!("{key}: {value}");
        if let Some(h) = self
            .header
            .iter_mut()
            .find(|h| h.key.eq_ignore_ascii_case(key))
        {
            h.value = value.to_string();
            h.raw = raw;
            return;
        }
        // First header line over existing free text: fence the text off as a body, or
        // the new line would run straight into it.
        if self.header.is_empty() {
            if let Some(r) = &self.rest {
                if !r.is_empty() {
                    self.rest = Some(format!("---\n{r}"));
                }
            }
        }
        self.header.push(HeaderLine {
            key: key.to_string(),
            value: value.to_string(),
            raw,
        });
    }

    /// Drop a header line; true when there was one.
    pub fn remove(&mut self, key: &str) -> bool {
        let before = self.header.len();
        self.header.retain(|h| !h.key.eq_ignore_ascii_case(key));
        self.header.len() != before
    }

    /// Reorder the header by `rank(key)`, keeping equal ranks in place. Every line
    /// is kept as written — only the order changes.
    pub fn reorder(&mut self, rank: impl Fn(&str) -> usize) {
        self.header.sort_by_key(|h| rank(&h.key));
    }

    /// The free text after the header, without the `---` fence.
    pub fn body(&self) -> String {
        let Some(r) = &self.rest else {
            return String::new();
        };
        let r = r.trim_start_matches('\n');
        let r = r.strip_prefix("---").unwrap_or(r);
        r.trim().to_string()
    }

    /// The existing text through its `---` fence (kept as written, so a replaced body
    /// doesn't reformat the line above it), else a fresh fence. The fence is always
    /// there when a body is written: without it, a body whose first line reads
    /// `Summary: …` would be taken for a header field the next time it is parsed.
    fn fence(&self) -> String {
        if let Some(r) = &self.rest {
            let t = r.trim_start_matches('\n');
            if t.starts_with("---") {
                return r[..r.len() - t.len() + 3].to_string();
            }
        }
        "---".to_string()
    }

    /// Replace the body. Empty clears it, fence included. The header is untouched.
    pub fn set_body(&mut self, body: &str) {
        let body = body.trim();
        self.rest = (!body.is_empty()).then(|| format!("{}\n{body}", self.fence()));
    }

    /// Add `text` after the existing body, separated by a blank line. What is already
    /// there stays byte-for-byte (only trailing whitespace is dropped to place the gap).
    pub fn append_body(&mut self, text: &str) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        let has_body = !self.body().is_empty();
        match &mut self.rest {
            Some(r) if has_body => {
                r.truncate(r.trim_end().len());
                r.push_str("\n\n");
                r.push_str(text);
            }
            _ => self.set_body(text),
        }
    }
}

// ----------------------------------------------------------------------- tasks

#[derive(Debug, Clone, Serialize)]
pub struct Task {
    pub id: String,
    pub title: String,
    /// The raw `status:` value ("" when the header has none).
    pub status: String,
    /// The board column it sits in: a configured column, `status` normalised.
    pub column: String,
    pub due: Option<String>,
    pub priority: String,
    pub completed: bool,
    /// The notes header, in order.
    pub fields: Vec<(String, String)>,
    pub body: String,
    /// The conversation working on it, when one was linked.
    pub conversation: Option<String>,
    /// Reminders' own last-modified time (RFC 3339): the last edit by anyone,
    /// phone included. Not "last worked on" — any write, hive's own included, moves it.
    pub modified: Option<String>,
    /// When the reminder was completed (RFC 3339), from Reminders itself.
    pub completed_at: Option<String>,
    /// How this task breaks the board's schema (see `common/task_schema.rs`).
    pub issues: Vec<Issue>,
    #[serde(skip)]
    notes: Notes,
}

fn done_column(columns: &[String]) -> Option<&String> {
    columns.iter().find(|c| c.as_str() == "done")
}

impl Task {
    fn from_reminder(v: &Value, cfg: &TasksConfig) -> Option<Task> {
        let id = v.get("id")?.as_str()?.to_string();
        let notes = Notes::parse(v.get("notes").and_then(|n| n.as_str()).unwrap_or(""));
        let completed = v
            .get("completed")
            .and_then(|c| c.as_bool())
            .unwrap_or(false);
        let status = notes.get("status").unwrap_or("").to_lowercase();
        let columns = cfg.columns();
        let column = if completed && done_column(&columns).is_some() {
            "done".to_string()
        } else if columns.contains(&status) {
            status.clone()
        } else {
            columns.first().cloned().unwrap_or_default()
        };
        Some(Task {
            id,
            title: v.get("title")?.as_str()?.to_string(),
            status,
            column,
            due: v.get("due").and_then(|d| d.as_str()).map(str::to_string),
            priority: v
                .get("priority")
                .and_then(|p| p.as_str())
                .unwrap_or("none")
                .to_string(),
            completed,
            modified: v
                .get("modified")
                .and_then(|d| d.as_str())
                .map(str::to_string),
            completed_at: v
                .get("completedAt")
                .and_then(|d| d.as_str())
                .map(str::to_string),
            issues: task_schema::validate(cfg, &notes.fields(), completed),
            fields: notes.fields(),
            body: notes.body(),
            conversation: notes.get("conversation").map(str::to_string),
            notes,
        })
    }

    /// Whole days since the last edit (None when Reminders gave no date).
    pub fn days_since_modified(&self) -> Option<i64> {
        let m = chrono::DateTime::parse_from_rfc3339(self.modified.as_deref()?).ok()?;
        Some(
            (chrono::Utc::now() - m.with_timezone(&chrono::Utc))
                .num_days()
                .max(0),
        )
    }

    /// Open and untouched for `stale_days` or more.
    pub fn is_stale(&self, cfg: &TasksConfig) -> bool {
        !self.completed
            && self.column != "done"
            && self
                .days_since_modified()
                .is_some_and(|d| d >= i64::from(cfg.stale_days()))
    }

    /// Does this task belong to the project, given its `filter`?
    fn matches(&self, cfg: &TasksConfig) -> bool {
        cfg.filter.iter().all(|(k, want)| {
            self.notes
                .get(k)
                .is_some_and(|have| have.eq_ignore_ascii_case(want))
        })
    }
}

// ------------------------------------------------------- Reminders (the backend)

/// The EventKit CLI. `HIVE_REMINDERS_CMD` overrides; the default is the
/// `apple-reminders` skill's binary, which hive does not bundle yet.
fn reminders_cmd() -> PathBuf {
    if let Ok(p) = std::env::var("HIVE_REMINDERS_CMD") {
        return PathBuf::from(p);
    }
    dirs::home_dir()
        .unwrap_or_default()
        .join(".claude/skills/apple-reminders/bin/reminders")
}

/// Run the CLI and return its JSON. A first run may sit on a macOS permission prompt,
/// so the wait is bounded: a web request must not hang on it.
fn run(args: Vec<String>) -> Result<Value> {
    let cmd = reminders_cmd();
    if !cmd.exists() {
        bail!(
            "Reminders CLI not found at {} (set HIVE_REMINDERS_CMD)",
            cmd.display()
        );
    }
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let out = std::process::Command::new(cmd)
            .args(args)
            .stdin(std::process::Stdio::null())
            .output();
        let _ = tx.send(out);
    });
    let out = rx
        .recv_timeout(CALL_TIMEOUT)
        .map_err(|_| {
            anyhow!(
                "Reminders did not answer in {}s (permission prompt?)",
                CALL_TIMEOUT.as_secs()
            )
        })?
        .context("could not run the Reminders CLI")?;
    let v: Value = serde_json::from_slice(&out.stdout).with_context(|| {
        format!(
            "unreadable Reminders output: {}",
            String::from_utf8_lossy(&out.stdout)
        )
    })?;
    if v.get("ok").and_then(|o| o.as_bool()) != Some(true) {
        bail!(
            "Reminders: {}",
            v.get("error").and_then(|e| e.as_str()).unwrap_or("failed")
        );
    }
    Ok(v)
}

fn s(x: &str) -> String {
    x.to_string()
}

fn reminders_in(v: &Value) -> Vec<&Value> {
    v.get("reminders")
        .and_then(|r| r.as_array())
        .map(|a| a.iter().collect())
        .unwrap_or_default()
}

/// Fetch a project's tasks straight from Reminders (no cache): every open task, plus
/// the latest few completed ones when the board has a done column.
pub fn fetch(cfg: &TasksConfig) -> Result<Vec<Task>> {
    check_source(cfg)?;
    let open = run(vec![
        s("list"),
        s("--list"),
        cfg.list.clone(),
        s("--limit"),
        s("500"),
    ])?;
    let mut tasks: Vec<Task> = reminders_in(&open)
        .into_iter()
        .filter_map(|r| Task::from_reminder(r, cfg))
        .filter(|t| t.matches(cfg))
        .collect();
    if done_column(&cfg.columns()).is_some() {
        let all = run(vec![
            s("list"),
            s("--list"),
            cfg.list.clone(),
            s("--all"),
            s("--limit"),
            s("300"),
        ])?;
        tasks.extend(
            reminders_in(&all)
                .into_iter()
                .filter(|r| r.get("completed").and_then(|c| c.as_bool()) == Some(true))
                .filter_map(|r| Task::from_reminder(r, cfg))
                .filter(|t| t.matches(cfg))
                .take(MAX_DONE),
        );
    }
    Ok(tasks)
}

fn check_source(cfg: &TasksConfig) -> Result<()> {
    if cfg.source != "reminders" {
        bail!("unknown task source '{}' — only 'reminders'", cfg.source);
    }
    Ok(())
}

/// One reminder, refused unless it is in this project's list and passes its filter —
/// every write goes through here, so a request can't reach an arbitrary reminder.
fn get_owned(cfg: &TasksConfig, id: &str) -> Result<Task> {
    check_source(cfg)?;
    let v = run(vec![s("get"), id.to_string()])?;
    let r = v
        .get("reminder")
        .ok_or_else(|| anyhow!("no reminder '{id}'"))?;
    if r.get("list").and_then(|l| l.as_str()) != Some(cfg.list.as_str()) {
        bail!("task '{id}' is not in the '{}' list", cfg.list);
    }
    let t = Task::from_reminder(r, cfg).ok_or_else(|| anyhow!("unreadable reminder '{id}'"))?;
    if !t.matches(cfg) {
        bail!("task '{id}' does not belong to this project");
    }
    Ok(t)
}

fn write_notes(id: &str, notes: &Notes) -> Result<()> {
    run(vec![
        s("edit"),
        id.to_string(),
        s("--notes"),
        notes.render(),
    ])?;
    Ok(())
}

/// The reason to refuse `value` for `key` on a strict board (None = fine).
fn strict_writes(cfg: &TasksConfig, writes: &[(String, String)]) -> Result<Vec<(String, String)>> {
    if !cfg.strict {
        return Ok(writes.to_vec());
    }
    task_schema::validate_write(cfg, writes).map_err(|errs| {
        anyhow!(
            "refused by this board's schema:\n  - {}",
            errs.join("\n  - ")
        )
    })
}

/// Header position for sorting a strict board's header into canonical order.
fn rank_fn(cfg: &TasksConfig) -> impl Fn(&str) -> usize {
    let names: Vec<String> = task_schema::fields(cfg)
        .into_iter()
        .map(|f| f.name)
        .collect();
    move |k: &str| {
        names
            .iter()
            .position(|n| n == &k.to_lowercase())
            .unwrap_or(usize::MAX)
    }
}

/// Set (and remove) header fields on a task. Re-reads it first, so a phone edit made
/// in between survives. On a strict board each written value is validated and the
/// header is left in canonical order.
pub fn set_fields(
    cfg: &TasksConfig,
    id: &str,
    sets: &[(String, String)],
    removes: &[String],
) -> Result<Task> {
    let sets = strict_writes(cfg, sets)?;
    let t = get_owned(cfg, id)?;
    let mut notes = t.notes.clone();
    for (k, v) in &sets {
        notes.set(k, v);
    }
    for k in removes {
        notes.remove(k);
    }
    if cfg.strict {
        notes.reorder(rank_fn(cfg));
    }
    write_notes(id, &notes)?;
    invalidate(cfg);
    get_owned(cfg, id)
}

pub fn set_field(cfg: &TasksConfig, id: &str, key: &str, value: &str) -> Result<Task> {
    set_fields(cfg, id, &[(key.to_string(), value.to_string())], &[])
}

/// Move a task to a column: `status:` is rewritten, and the reminder is completed when
/// the column is `done` (and re-opened when it leaves it). On a strict board a move to
/// `blocked` needs a reason (`blocked_by`, or one already on the task), and leaving
/// `blocked` drops it.
pub fn move_task(
    cfg: &TasksConfig,
    id: &str,
    column: &str,
    blocked_by: Option<&str>,
) -> Result<Task> {
    let column = column.to_lowercase();
    if !cfg.columns().contains(&column) {
        bail!(
            "unknown column '{column}' — this board has: {}",
            cfg.columns().join(", ")
        );
    }
    let t = get_owned(cfg, id)?;
    let mut notes = t.notes.clone();
    notes.set("status", &column);
    let reason = blocked_by.map(str::trim).filter(|r| !r.is_empty());
    if cfg.strict {
        if column == "blocked" {
            match reason {
                Some(r) => {
                    let w = strict_writes(cfg, &[("blocked-by".into(), r.into())])?;
                    notes.set(&w[0].0, &w[0].1);
                }
                None if notes.get("blocked-by").is_none() => {
                    bail!("moving to blocked needs the reason: pass --by '<what it waits on>'")
                }
                None => {}
            }
        } else {
            notes.remove("blocked-by");
        }
        notes.reorder(rank_fn(cfg));
    } else if let (Some(r), "blocked") = (reason, column.as_str()) {
        notes.set("blocked-by", r);
    }
    write_notes(id, &notes)?;
    let to_done = column == "done";
    if to_done != t.completed {
        run(vec![
            s(if to_done { "done" } else { "undone" }),
            id.to_string(),
        ])?;
    }
    invalidate(cfg);
    get_owned(cfg, id)
}

/// Changes to the fields Reminders owns. `due`: `Some(None)` clears it.
#[derive(Debug, Default, Clone)]
pub struct NativeEdit {
    pub title: Option<String>,
    pub priority: Option<String>,
    pub due: Option<Option<String>>,
}

impl NativeEdit {
    pub fn is_empty(&self) -> bool {
        self.title.is_none() && self.priority.is_none() && self.due.is_none()
    }
}

/// `[OPS-007]`-style id at the front of a title, if any.
fn title_id(t: &str) -> Option<&str> {
    let rest = t.strip_prefix('[')?;
    let end = rest.find(']')?;
    Some(&t[..end + 2])
}

fn is_iso_date(s: &str) -> bool {
    let b = s.as_bytes();
    let digits = |r: std::ops::Range<usize>| {
        b.get(r)
            .is_some_and(|x| !x.is_empty() && x.iter().all(u8::is_ascii_digit))
    };
    s.len() >= 10
        && digits(0..4)
        && b[4] == b'-'
        && digits(5..7)
        && b[7] == b'-'
        && digits(8..10)
        && (s.len() == 10
            || (b[10] == b'T'
                && s.len() >= 16
                && digits(11..13)
                && b[13] == b':'
                && digits(14..16)))
}

/// The Reminders CLI arguments for `edit`, or why the edit is refused. Pure, so the
/// rules are testable: priority is one of none/low/medium/high, a due date is ISO, and
/// a title may not lose its `[PREFIX-nnn]` id (ids are cited elsewhere).
fn native_args(old_title: &str, e: &NativeEdit) -> Result<Vec<String>> {
    let mut a = Vec::new();
    if let Some(t) = &e.title {
        let t = t.trim();
        if t.is_empty() {
            bail!("title can't be empty");
        }
        if let Some(id) = title_id(old_title) {
            if title_id(t) != Some(id) {
                bail!("keep the id: the title must still start with {id}");
            }
        }
        a.extend([s("--title"), t.to_string()]);
    }
    if let Some(p) = &e.priority {
        let p = p.trim().to_lowercase();
        if !["none", "low", "medium", "high"].contains(&p.as_str()) {
            bail!("priority '{p}' — use none, low, medium or high");
        }
        a.extend([s("--priority"), p]);
    }
    match &e.due {
        Some(Some(d)) => {
            if !is_iso_date(d.trim()) {
                bail!("due '{d}' — use YYYY-MM-DD or YYYY-MM-DDTHH:MM");
            }
            a.extend([s("--due"), d.trim().to_string()]);
        }
        Some(None) => a.extend([s("--due"), s("clear")]),
        None => {}
    }
    Ok(a)
}

/// Edit title / priority / due on a task of this board.
pub fn edit_native(cfg: &TasksConfig, id: &str, e: &NativeEdit) -> Result<Task> {
    let t = get_owned(cfg, id)?;
    let extra = native_args(&t.title, e)?;
    if extra.is_empty() {
        return Ok(t);
    }
    let mut args = vec![s("edit"), id.to_string()];
    args.extend(extra);
    run(args)?;
    invalidate(cfg);
    get_owned(cfg, id)
}

/// A change to a task's body (the free markdown under the header).
#[derive(Debug, Clone, PartialEq)]
pub enum BodyEdit {
    /// Rewrite it. Empty clears it.
    Replace(String),
    /// Add to the end of what is there.
    Append(String),
}

/// Edit the body of a task of this board. The header is left exactly as it was — no
/// reordering on a strict board either, since nothing in it changed. A write that would
/// change nothing is skipped: Reminders' `modified` date is the board's staleness clock
/// and any write resets it.
pub fn edit_body(cfg: &TasksConfig, id: &str, e: &BodyEdit) -> Result<Task> {
    let t = get_owned(cfg, id)?;
    let mut notes = t.notes.clone();
    match e {
        BodyEdit::Replace(b) => notes.set_body(b),
        BodyEdit::Append(a) => notes.append_body(a),
    }
    if notes == t.notes {
        return Ok(t);
    }
    write_notes(id, &notes)?;
    invalidate(cfg);
    get_owned(cfg, id)
}

pub struct NewTask {
    pub title: String,
    pub column: Option<String>,
    pub fields: Vec<(String, String)>,
    pub body: Option<String>,
    pub due: Option<String>,
    pub priority: Option<String>,
}

/// Every task on the board, completed ones included (for numbering ids).
fn fetch_everything(cfg: &TasksConfig) -> Result<Vec<Task>> {
    let all = run(vec![
        s("list"),
        s("--list"),
        cfg.list.clone(),
        s("--all"),
        s("--limit"),
        s("1000"),
    ])?;
    Ok(reminders_in(&all)
        .into_iter()
        .filter_map(|r| Task::from_reminder(r, cfg))
        .filter(|t| t.matches(cfg))
        .collect())
}

/// The number after the highest `[PREFIX-n]` among `titles`.
fn next_number<'a>(titles: impl Iterator<Item = &'a str>, prefix: &str) -> u32 {
    let head = format!("[{prefix}-");
    titles
        .filter_map(|t| {
            let rest = t.strip_prefix(&head)?;
            rest.split(']').next()?.parse::<u32>().ok()
        })
        .max()
        .unwrap_or(0)
        + 1
}

/// Add a task, returning its final title. The project's `filter` pairs are stamped into
/// the header so the new task shows up on this board and not a sibling's. On a strict
/// board the fields are validated, required ones must be present, the header is put in
/// canonical order, and an `id_prefix` area gets its `[PREFIX-n]` title id.
pub fn add_task(cfg: &TasksConfig, new: NewTask) -> Result<String> {
    check_source(cfg)?;
    let columns = cfg.columns();
    let column = new
        .column
        .map(|c| c.to_lowercase())
        .unwrap_or_else(|| columns[0].clone());
    if !columns.contains(&column) {
        bail!(
            "unknown column '{column}' — this board has: {}",
            columns.join(", ")
        );
    }
    let mut pairs: Vec<(String, String)> = vec![("status".into(), column)];
    pairs.extend(cfg.filter.iter().map(|(k, v)| (k.clone(), v.clone())));
    pairs.extend(new.fields.iter().cloned());
    let mut title = new.title.trim().to_string();
    if cfg.strict {
        pairs = strict_writes(cfg, &pairs)?;
        let missing = task_schema::missing_required(cfg, &pairs);
        if !missing.is_empty() {
            bail!(
                "refused by this board's schema: missing required field(s): {} (--field key=value)",
                missing.join(", ")
            );
        }
        let idf = cfg.id_field.as_deref().unwrap_or("area").to_lowercase();
        let prefix = pairs
            .iter()
            .find(|(k, _)| *k == idf)
            .and_then(|(_, v)| cfg.id_prefix.get(v));
        if let (Some(prefix), false) = (prefix, title.starts_with('[')) {
            let n = next_number(
                fetch_everything(cfg)?.iter().map(|t| t.title.as_str()),
                prefix,
            );
            title = format!("[{prefix}-{n:03}] {title}");
        }
        task_schema::canonical_order(cfg, &mut pairs);
    }
    let mut notes = Notes::default();
    for (k, v) in &pairs {
        notes.set(k, v);
    }
    if let Some(b) = new.body.filter(|b| !b.trim().is_empty()) {
        notes.rest = Some(format!("---\n{}", b.trim()));
    }
    let mut args = vec![
        s("add"),
        title.clone(),
        s("--list"),
        cfg.list.clone(),
        s("--notes"),
        notes.render(),
    ];
    if let Some(d) = new.due {
        args.extend([s("--due"), d]);
    }
    if let Some(p) = new.priority {
        args.extend([s("--priority"), p]);
    }
    run(args)?;
    invalidate(cfg);
    Ok(title)
}

// ----------------------------------------------------------------------- cache

type CacheMap = HashMap<String, (Instant, Vec<Task>)>;
static CACHE: Mutex<Option<CacheMap>> = Mutex::new(None);

fn invalidate(cfg: &TasksConfig) {
    if let Ok(mut g) = CACHE.lock() {
        if let Some(m) = g.as_mut() {
            m.remove(&cache_key(cfg));
        }
    }
}

fn cache_key(cfg: &TasksConfig) -> String {
    format!("{}|{:?}", cfg.list, cfg.filter)
}

/// `fetch`, memoised for a few seconds: the board polls every couple of seconds, and
/// Reminders is two process spawns per fetch.
pub fn fetch_cached(cfg: &TasksConfig) -> Result<Vec<Task>> {
    let key = cache_key(cfg);
    if let Ok(g) = CACHE.lock() {
        if let Some((at, tasks)) = g.as_ref().and_then(|m| m.get(&key)) {
            if at.elapsed() < CACHE_TTL {
                return Ok(tasks.clone());
            }
        }
    }
    let tasks = fetch(cfg)?;
    if let Ok(mut g) = CACHE.lock() {
        g.get_or_insert_with(HashMap::new)
            .insert(key, (Instant::now(), tasks.clone()));
    }
    Ok(tasks)
}

// -------------------------------------------------------------------- projects

/// The project key behind a worktree key (`project/branch` → `project`).
pub fn project_of(key: &str) -> &str {
    key.split('/').next().unwrap_or(key)
}

/// Which project a directory belongs to — a worktree resolves up to its project, so a
/// conversation in `avateen/some-branch` sees `avateen`'s board.
pub fn project_for_cwd(cwd: &str, projects: &ProjectRegistry) -> Option<String> {
    let worktrees = crate::common::worktree::WorktreeState::load();
    crate::common::registry::resolve_parent(cwd, &worktrees, projects)
        .map(|k| project_of(&k).to_string())
}

/// Projects that have a task board, sorted by key.
pub fn boards(projects: &ProjectRegistry) -> Vec<(String, TasksConfig)> {
    let mut v: Vec<_> = projects
        .projects
        .iter()
        .filter_map(|(k, p)| p.tasks.clone().map(|t| (k.clone(), t)))
        .collect();
    v.sort_by(|a, b| a.0.cmp(&b.0));
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    const THESIS: &str = "status: todo\narea: tooling\nepic: notacion\nblocked-by: none\nartefact: docs/research/structurizr.md\nest: 1h\nlog: yes";
    const AWS: &str = "project: symposium\ntype: sync\nstatus: todo\nsize: S\nlinks: https://hedgedoc.symposium.dev/Q7q\n---\n## Goal\nHave enough context.\n\n- one\n- two";

    fn cfg() -> TasksConfig {
        TasksConfig {
            source: "reminders".into(),
            list: "T".into(),
            ..Default::default()
        }
    }

    #[test]
    fn both_dialects_round_trip_untouched() {
        for s in [
            THESIS,
            AWS,
            "",
            "just prose, no header",
            "a: 1\n",
            "a: 1\n---\n",
        ] {
            assert_eq!(Notes::parse(s).render(), s, "{s:?}");
        }
    }

    #[test]
    fn a_url_or_prose_colon_is_not_a_field() {
        let n = Notes::parse("https://x.dev/a\nNote: this is prose with a colon: yes");
        assert!(n.fields().is_empty() || n.fields()[0].0 != "https");
        assert_eq!(n.get("https"), None);
    }

    #[test]
    fn set_rewrites_only_the_one_line() {
        let mut n = Notes::parse(AWS);
        n.set("status", "doing");
        assert_eq!(n.render(), AWS.replace("status: todo", "status: doing"));
        assert_eq!(n.get("STATUS"), Some("doing"));
    }

    #[test]
    fn set_appends_a_missing_key_to_the_header_not_the_body() {
        let mut n = Notes::parse(AWS);
        n.set("conversation", "abc");
        let out = n.render();
        assert!(out.contains(
            "size: S\nlinks: https://hedgedoc.symposium.dev/Q7q\nconversation: abc\n---\n## Goal"
        ));
        assert_eq!(Notes::parse(&out).body(), Notes::parse(AWS).body());
    }

    #[test]
    fn a_header_over_plain_text_fences_the_text_as_a_body() {
        let mut n = Notes::parse("remember the milk");
        n.set("status", "todo");
        assert_eq!(n.render(), "status: todo\n---\nremember the milk");
        assert_eq!(n.body(), "remember the milk");
    }

    #[test]
    fn body_drops_the_fence() {
        assert_eq!(
            Notes::parse(AWS).body(),
            "## Goal\nHave enough context.\n\n- one\n- two"
        );
        assert_eq!(Notes::parse(THESIS).body(), "");
    }

    #[test]
    fn set_body_rewrites_the_body_and_nothing_above_it() {
        let mut n = Notes::parse(AWS);
        n.set_body("## Review\nNiko's answers.");
        assert_eq!(
            n.render(),
            "project: symposium\ntype: sync\nstatus: todo\nsize: S\nlinks: https://hedgedoc.symposium.dev/Q7q\n---\n## Review\nNiko's answers."
        );
        assert_eq!(n.fields(), Notes::parse(AWS).fields());
    }

    #[test]
    fn set_body_fences_a_body_under_a_bare_header_or_none() {
        let mut n = Notes::parse(THESIS);
        n.set_body("hello");
        assert_eq!(n.render(), format!("{THESIS}\n---\nhello"));
        // No header at all: the fence is what stops a first line like this reading as a field.
        let mut bare = Notes::parse("");
        bare.set_body("Summary: it works");
        let again = Notes::parse(&bare.render());
        assert!(again.fields().is_empty(), "{:?}", again.fields());
        assert_eq!(again.body(), "Summary: it works");
    }

    #[test]
    fn set_body_keeps_the_fence_as_it_was_written() {
        let mut n = Notes::parse("a: 1\n\n---\nold");
        n.set_body("new");
        assert_eq!(n.render(), "a: 1\n\n---\nnew");
    }

    #[test]
    fn an_empty_body_clears_it_fence_included() {
        let mut n = Notes::parse(AWS);
        n.set_body("  \n");
        assert_eq!(n.body(), "");
        assert_eq!(
            n.render(),
            "project: symposium\ntype: sync\nstatus: todo\nsize: S\nlinks: https://hedgedoc.symposium.dev/Q7q"
        );
    }

    #[test]
    fn append_body_adds_after_a_blank_line_and_keeps_what_was_there() {
        let mut n = Notes::parse(AWS);
        n.append_body("## Review\nNiko's answers.\n");
        assert_eq!(
            n.render(),
            format!("{AWS}\n\n## Review\nNiko's answers."),
            "existing body byte-for-byte, one blank line, then the addition"
        );
        n.append_body("- more");
        assert!(n.render().ends_with("Niko's answers.\n\n- more"));
    }

    #[test]
    fn append_body_to_an_empty_body_starts_it() {
        let mut a = Notes::parse(THESIS);
        a.append_body("first");
        assert_eq!(a.render(), format!("{THESIS}\n---\nfirst"));
        let mut b = Notes::parse("a: 1\n---\n");
        b.append_body("first");
        assert_eq!(b.render(), "a: 1\n---\nfirst");
    }

    #[test]
    fn append_of_nothing_changes_nothing() {
        let mut n = Notes::parse(AWS);
        n.append_body(" \n ");
        assert_eq!(n, Notes::parse(AWS));
    }

    #[test]
    fn a_replaced_body_survives_a_parse_round_trip() {
        for body in ["plain", "## H\n\n- a\n- b", "---\nnot a fence", "k: v"] {
            let mut n = Notes::parse(AWS);
            n.set_body(body);
            let back = Notes::parse(&n.render());
            assert_eq!(back.body(), body);
            assert_eq!(back.fields(), Notes::parse(AWS).fields());
        }
    }

    fn reminder(notes: &str, completed: bool) -> Value {
        serde_json::json!({"id":"1","title":"t","notes":notes,"completed":completed,"priority":"high"})
    }

    #[test]
    fn column_follows_status_unknown_falls_to_first_completed_to_done() {
        let c = cfg();
        let t = |n: &str, done| Task::from_reminder(&reminder(n, done), &c).unwrap();
        assert_eq!(t("status: doing", false).column, "doing");
        assert_eq!(t("status: Blocked", false).column, "blocked");
        assert_eq!(t("status: tracking", false).column, "tracking");
        assert_eq!(
            t("status: review", false).column,
            "todo",
            "unknown falls to the first column"
        );
        assert_eq!(t("", false).column, "todo");
        assert_eq!(t("status: todo", true).column, "done");
    }

    #[test]
    fn every_board_has_the_generic_columns_even_when_it_names_fewer() {
        let c = TasksConfig {
            columns: vec!["todo".into(), "blocked".into()],
            ..cfg()
        };
        assert_eq!(
            c.columns(),
            ["todo", "doing", "blocked", "tracking", "done"]
        );
        let t = Task::from_reminder(&reminder("status: tracking", false), &c).unwrap();
        assert_eq!(t.column, "tracking");
        let done = Task::from_reminder(&reminder("status: blocked", true), &c).unwrap();
        assert_eq!(done.column, "done", "completed always lands in done");
    }

    #[test]
    fn filter_splits_one_list_between_projects() {
        let mut c = cfg();
        c.filter.insert("project".into(), "symposium".into());
        let mine = Task::from_reminder(&reminder(AWS, false), &c).unwrap();
        let other =
            Task::from_reminder(&reminder("project: ops\nstatus: todo", false), &c).unwrap();
        let bare = Task::from_reminder(&reminder("status: todo", false), &c).unwrap();
        assert!(mine.matches(&c));
        assert!(!other.matches(&c));
        assert!(!bare.matches(&c));
    }

    #[test]
    fn worktree_key_resolves_to_its_project() {
        assert_eq!(project_of("avateen/test-harness"), "avateen");
        assert_eq!(project_of("hive"), "hive");
    }

    #[test]
    fn next_id_follows_the_highest_existing_even_with_gaps() {
        let titles = [
            "[SYMP-001] a",
            "[SYMP-016] b",
            "[OPS-004] c",
            "plain",
            "[SYMP-x] d",
        ];
        assert_eq!(next_number(titles.iter().copied(), "SYMP"), 17);
        assert_eq!(next_number(titles.iter().copied(), "OPS"), 5);
        assert_eq!(next_number(titles.iter().copied(), "ROAD"), 1);
    }

    #[test]
    fn remove_drops_one_line_and_keeps_the_rest() {
        let mut n = Notes::parse(AWS);
        assert!(n.remove("TYPE"));
        assert!(!n.remove("type"));
        assert_eq!(n.render(), AWS.replace("type: sync\n", ""));
    }

    #[test]
    fn reorder_changes_only_the_order() {
        let mut n = Notes::parse("size: S\nstatus: todo\n---\nbody");
        n.reorder(|k| if k == "status" { 0 } else { 1 });
        assert_eq!(n.render(), "status: todo\nsize: S\n---\nbody");
    }

    #[test]
    fn a_task_carries_its_schema_issues() {
        let t = Task::from_reminder(&reminder("status: todo\nepc: x", false), &cfg()).unwrap();
        assert!(t
            .issues
            .iter()
            .any(|i| i.msg.contains("unknown field `epc`")));
    }

    #[test]
    fn staleness_counts_whole_days_since_the_last_edit_of_open_tasks_only() {
        let c = cfg();
        let ago = |d: i64| (chrono::Utc::now() - chrono::Duration::days(d)).to_rfc3339();
        let mk = |d: i64, done: bool| {
            let mut v = reminder("status: doing", done);
            v["modified"] = serde_json::json!(ago(d));
            Task::from_reminder(&v, &c).unwrap()
        };
        assert_eq!(mk(11, false).days_since_modified(), Some(11));
        assert!(mk(11, false).is_stale(&c));
        assert!(!mk(3, false).is_stale(&c));
        assert!(!mk(30, true).is_stale(&c), "a finished task is never stale");
        let none = Task::from_reminder(&reminder("status: doing", false), &c).unwrap();
        assert_eq!(none.days_since_modified(), None);
        assert!(!none.is_stale(&c));
        let mut tight = c.clone();
        tight.stale_days = Some(2);
        assert!(mk(3, false).is_stale(&tight));
    }

    #[test]
    fn native_edits_validate_priority_due_and_keep_ids() {
        let e = |t: Option<&str>, p: Option<&str>, d: Option<Option<&str>>| NativeEdit {
            title: t.map(String::from),
            priority: p.map(String::from),
            due: d.map(|x| x.map(String::from)),
        };
        let old = "[OPS-007] Do the thing";
        assert_eq!(
            native_args(old, &e(None, Some("HIGH"), None)).unwrap(),
            ["--priority", "high"]
        );
        assert!(native_args(old, &e(None, Some("urgent"), None)).is_err());
        assert_eq!(
            native_args(old, &e(None, None, Some(Some("2026-12-01")))).unwrap(),
            ["--due", "2026-12-01"]
        );
        assert!(native_args(old, &e(None, None, Some(Some("2026-12-01T09:30")))).is_ok());
        assert!(native_args(old, &e(None, None, Some(Some("next week")))).is_err());
        assert!(native_args(old, &e(None, None, Some(Some("2026-1-1")))).is_err());
        assert_eq!(
            native_args(old, &e(None, None, Some(None))).unwrap(),
            ["--due", "clear"]
        );
        assert!(
            native_args(old, &e(Some("Renamed"), None, None)).is_err(),
            "id dropped"
        );
        assert!(
            native_args(old, &e(Some("[OPS-008] Renamed"), None, None)).is_err(),
            "id changed"
        );
        assert!(native_args(old, &e(Some("[OPS-007] Renamed"), None, None)).is_ok());
        assert!(native_args("plain title", &e(Some("Other"), None, None)).is_ok());
        assert!(native_args(old, &e(Some("  "), None, None)).is_err());
    }

    #[test]
    fn conversation_link_is_read_from_the_header() {
        let t = Task::from_reminder(
            &reminder("status: doing\nconversation: abc-123", false),
            &cfg(),
        )
        .unwrap();
        assert_eq!(t.conversation.as_deref(), Some("abc-123"));
    }
}
