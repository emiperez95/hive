//! Dispatching a todo into work — the sidebar's todo "start" dialog.
//!
//! Three ways a todo becomes a conversation: typed into the current one, started as
//! a new conversation in the same session, or started in a fresh worktree. The
//! first two are instant; the worktree one runs hooks (copies, installs) that can
//! take minutes, so it runs as a background [`Jobs`] entry the dialog polls.
//!
//! The worktree needs a branch name, which is the one part that wants judgement:
//! a ticket key in the text wins outright; otherwise a small model (Haiku, via
//! `claude -p`) names it in the style of the project's existing branches; and a
//! plain slug is the fallback when the model is slow, absent or says something
//! that isn't a valid ref. The dialog shows the suggestion in an editable field —
//! the model proposes, the person decides.

use std::collections::HashMap;
use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::common::projects::ProjectRegistry;
use crate::common::worktree::WorktreeState;

/// Longest a branch suggestion may take before falling back to the slug. Haiku
/// answers in ~6s here; this bounds a cold start or a hung auth refresh.
const MODEL_TIMEOUT: Duration = Duration::from_secs(25);

/// Branch names stay short enough to read in a session name at sidebar width.
const MAX_BRANCH: usize = 40;

// ── Branch naming ───────────────────────────────────────────────────────────

/// A ticket key (`CSD-2723`, `PROJ-12`) anywhere in the text. The worktree
/// convention here names the branch after the ticket, so when one is present
/// nothing needs guessing.
pub fn ticket_key(text: &str) -> Option<String> {
    let b = text.as_bytes();
    let mut i = 0;
    while i < b.len() {
        // A key starts at a word boundary with an uppercase letter.
        let boundary = i == 0 || !b[i - 1].is_ascii_alphanumeric();
        if boundary && b[i].is_ascii_uppercase() {
            let mut j = i + 1;
            while j < b.len() && (b[j].is_ascii_uppercase() || b[j].is_ascii_digit()) {
                j += 1;
            }
            let prefix_len = j - i;
            if (2..=10).contains(&prefix_len) && j < b.len() && b[j] == b'-' {
                let mut k = j + 1;
                while k < b.len() && b[k].is_ascii_digit() {
                    k += 1;
                }
                let end_ok = k == b.len() || !b[k].is_ascii_alphanumeric();
                if k > j + 1 && end_ok {
                    return Some(text[i..k].to_string());
                }
            }
            i = j.max(i + 1);
        } else {
            i += 1;
        }
    }
    None
}

/// Deterministic fallback: lowercase words joined by `-`, cut at a word boundary.
pub fn slugify(text: &str) -> String {
    let mut out = String::new();
    for word in text
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
    {
        let w = word.to_ascii_lowercase();
        let extra = if out.is_empty() { w.len() } else { w.len() + 1 };
        if out.len() + extra > MAX_BRANCH {
            break;
        }
        if !out.is_empty() {
            out.push('-');
        }
        out.push_str(&w);
    }
    if out.is_empty() {
        "todo".to_string()
    } else {
        out
    }
}

/// Reduce a model's reply to a branch name, or `None` if nothing usable is left.
/// Models wrap answers in backticks, quotes or a sentence; only the first line's
/// ref-safe characters are kept, and the result must pass `git check-ref-format`.
pub fn clean_branch(reply: &str) -> Option<String> {
    let line = reply.lines().map(str::trim).find(|l| !l.is_empty())?;
    let cleaned: String = line
        .trim_matches(|c: char| c == '`' || c == '"' || c == '\'')
        .chars()
        .map(|c| c.to_ascii_lowercase())
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '/' | '.' | '_'))
        .collect();
    let cleaned = cleaned.trim_matches(|c| c == '-' || c == '/' || c == '.');
    if cleaned.is_empty() || cleaned.len() > MAX_BRANCH + 12 || !valid_branch(cleaned) {
        return None;
    }
    Some(cleaned.to_string())
}

/// `git check-ref-format --branch` — git's own definition, not a re-implementation.
pub fn valid_branch(name: &str) -> bool {
    !name.is_empty()
        && Command::new("git")
            .args(["check-ref-format", "--branch", name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
}

/// Where the naming model runs. A fixed, empty directory: `--setting-sources
/// project` there loads no settings at all, so none of the user's hooks fire — hive's
/// own hook included, which would otherwise record this throwaway session and ping
/// when it stops.
fn namer_dir() -> Option<std::path::PathBuf> {
    let dir = dirs::home_dir()?
        .join(".hive")
        .join("cache")
        .join("branch-namer");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

/// Ask Haiku for a branch name. `examples` are the project's existing branches, so
/// the answer follows the local convention (`feat/…`, bare kebab, …) rather than
/// a generic one.
fn model_branch(text: &str, examples: &[String], env: &[(String, String)]) -> Option<String> {
    let style = if examples.is_empty() {
        "feat/add-login-form, fix/null-avatar-crash".to_string()
    } else {
        examples.join(", ")
    };
    let prompt = format!(
        "Output ONLY a git branch name for the task below — nothing else, no quotes. \
         Lowercase kebab-case, in English, at most {MAX_BRANCH} characters. Follow the \
         style of this repository's existing branches: {style}.\n\nTask: {text}"
    );
    let mut cmd = Command::new("claude");
    cmd.args([
        "-p",
        "--model",
        "haiku",
        "--setting-sources",
        "project",
        "--no-session-persistence",
        "--tools",
        "",
    ])
    .current_dir(namer_dir()?)
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::null());
    // The project's auth profile, so the call bills the same identity its work does.
    for (k, v) in env {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().ok()?;
    child.stdin.take()?.write_all(prompt.as_bytes()).ok()?;

    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if started.elapsed() < MODEL_TIMEOUT => {
                std::thread::sleep(Duration::from_millis(100))
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let out = child.wait_with_output().ok()?;
    clean_branch(&String::from_utf8_lossy(&out.stdout))
}

/// A suggested branch for `text` in `project`, and where it came from
/// (`ticket` / `model` / `slug`). Made unique against the project's registered
/// worktrees, since `hive wt new` on a taken name fails.
pub fn suggest_branch(project: &str, text: &str) -> (String, &'static str) {
    let wts = WorktreeState::load();
    let mut existing: Vec<(String, String)> = wts
        .worktrees
        .values()
        .filter(|e| e.project_key == project)
        .map(|e| (e.created_at.clone(), e.branch.clone()))
        .collect();
    existing.sort_by(|a, b| b.0.cmp(&a.0));
    let examples: Vec<String> = existing.iter().take(8).map(|(_, b)| b.clone()).collect();

    let env = ProjectRegistry::load()
        .projects
        .get(project)
        .map(|c| c.tmux_env())
        .unwrap_or_default();

    let (base, source) = if let Some(t) = ticket_key(text) {
        (t, "ticket")
    } else if let Some(b) = model_branch(text, &examples, &env) {
        (b, "model")
    } else {
        (slugify(text), "slug")
    };

    let taken = |b: &str| existing.iter().any(|(_, e)| e == b);
    let mut name = base.clone();
    let mut n = 2;
    while taken(&name) {
        name = format!("{base}-{n}");
        n += 1;
    }
    (name, source)
}

// ── Background jobs ─────────────────────────────────────────────────────────

#[derive(Clone, serde::Serialize)]
pub struct Job {
    /// `running` | `ok` | `error`
    pub state: &'static str,
    pub message: String,
    /// The session the work landed in, once known — the dialog offers to go there.
    pub session: Option<String>,
}

/// In-memory job table, for the web server's lifetime. Small by construction:
/// one entry per worktree dispatched from the sidebar.
#[derive(Default)]
pub struct Jobs {
    next: Mutex<u64>,
    jobs: Mutex<HashMap<u64, Job>>,
}

impl Jobs {
    fn start(&self) -> u64 {
        let mut n = self.next.lock().unwrap();
        *n += 1;
        self.set(
            *n,
            Job {
                state: "running",
                message: "Creating worktree…".into(),
                session: None,
            },
        );
        *n
    }

    fn set(&self, id: u64, job: Job) {
        self.jobs.lock().unwrap().insert(id, job);
    }

    pub fn get(&self, id: u64) -> Option<Job> {
        self.jobs.lock().unwrap().get(&id).cloned()
    }
}

/// Create `project/branch` as a worktree and start Claude in it on `prompt`, in a
/// background thread. Returns the job id to poll.
///
/// The prompt rides on the project's startup command when that command runs
/// Claude — that's what `hive wt new --prompt` does. A project whose startup is
/// something else (a dev server, an editor) gets its startup untouched and the
/// conversation in a window of its own, rather than a prompt appended to a
/// command that doesn't take one.
pub fn start_worktree_job(
    jobs: &std::sync::Arc<Jobs>,
    project: String,
    branch: String,
    prompt: String,
) -> u64 {
    let id = jobs.start();
    let jobs = std::sync::Arc::clone(jobs);
    std::thread::spawn(move || {
        let startup_runs_claude = ProjectRegistry::load()
            .projects
            .get(&project)
            .and_then(|c| c.startup_command.clone())
            .is_some_and(|c| c.contains("claude"));
        let result = crate::cli::worktree::run_wt_new(
            &project,
            &branch,
            None,
            false,
            "worktree",
            startup_runs_claude.then_some(prompt.as_str()),
            false,
            false,
        )
        .and_then(|session| {
            if !startup_runs_claude {
                crate::common::conversations::start_task_in_session(&session, &prompt)?;
            }
            Ok(session)
        });
        let job = match result {
            Ok(session) => Job {
                state: "ok",
                message: format!("Started in {session}"),
                session: Some(session),
            },
            Err(e) => Job {
                state: "error",
                message: format!("{e:#}"),
                session: None,
            },
        };
        jobs.set(id, job);
    });
    id
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ticket_key_found_anywhere_on_a_word_boundary() {
        assert_eq!(
            ticket_key("fix CSD-2723 login").as_deref(),
            Some("CSD-2723")
        );
        assert_eq!(ticket_key("PROJ-1: do it").as_deref(), Some("PROJ-1"));
        assert_eq!(ticket_key("(AB-12)").as_deref(), Some("AB-12"));
    }

    #[test]
    fn ticket_key_rejects_lookalikes() {
        assert_eq!(ticket_key("no ticket here"), None);
        assert_eq!(ticket_key("xCSD-12 glued to a word"), None);
        assert_eq!(ticket_key("CSD- no number"), None);
        assert_eq!(ticket_key("A-12 prefix too short"), None);
        assert_eq!(ticket_key("CSD-12abc trailing letters"), None);
        assert_eq!(
            ticket_key("UTF-8 is not a ticket either"),
            Some("UTF-8".into())
        );
    }

    #[test]
    fn slugify_cuts_on_a_word_boundary() {
        assert_eq!(
            slugify("Add the content manager flow!"),
            "add-the-content-manager-flow"
        );
        let long = slugify("one two three four five six seven eight nine ten eleven");
        assert!(long.len() <= MAX_BRANCH, "{long}");
        assert!(!long.ends_with('-'));
        assert_eq!(slugify("¿¡!!"), "todo");
    }

    #[test]
    fn clean_branch_strips_model_wrapping() {
        assert_eq!(
            clean_branch("`feat/content-flow`\n").as_deref(),
            Some("feat/content-flow")
        );
        assert_eq!(
            clean_branch("\n\"Fix/Claro-Reports\"").as_deref(),
            Some("fix/claro-reports")
        );
        assert_eq!(clean_branch(""), None);
        assert_eq!(clean_branch("feat/..bad"), None);
    }
}
