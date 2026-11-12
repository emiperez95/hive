//! Boxes — one conversation packed into a directory that can be moved between machines.
//!
//! A conversation is files; the Claude process is disposable (see `docs/remote-boxes.md`).
//! `pack` writes everything needed to continue it elsewhere without committing anything:
//!
//! ```text
//! ~/.hive/boxes/<uuid>/
//!   manifest.json      identity + where it lived
//!   commits.bundle     commits that exist on no remote (absent when there are none)
//!   staged.patch       `git diff --cached --binary`
//!   tree/              rsync of the working tree, minus `.git`
//!   transcript.jsonl   + memory/ of the transcript's slug
//!   overlay.json       the conversation's sidecar entry
//!   todos.json         the session's active + done todos
//! ```
//!
//! `unpack` is the inverse and is deliberately conservative: nothing that exists only on
//! the target is overwritten (a dirty main checkout, a non-empty worktree path, a longer
//! transcript all refuse).
//!
//! ## What the git half has to get right
//!
//! - **Bundle the branch ref, not `HEAD`.** `bundle create <range> HEAD` carries a ref
//!   named `HEAD`, so `fetch bundle feat:feat` then fails with "couldn't find remote ref".
//! - **"Unpushed" is `--not --remotes`**, the same question `prune.rs` asks — not
//!   `origin/main..feat`, which would re-ship commits the branch already pushed.
//! - **Index and working tree are restored separately.** The tree is an rsync (so unstaged
//!   edits, untracked and ignored files come across); the index is rebuilt from `head`
//!   plus `staged.patch`. A tracked file deleted in the working tree has to be removed
//!   from the fresh checkout by hand, since rsync is run without `--delete`: `--delete`
//!   would also wipe ignored files on a main checkout when the box was packed
//!   `--without-ignored`.

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::common::claude_sessions::{self, ClaudeSessionStatus};
use crate::common::persistence::hive_home;
use crate::common::projects::{expand_tilde, ProjectRegistry};
use crate::common::registry::{resolve_parent, ConversationSidecar};
use crate::common::worktree::{WorktreeEntry, WorktreeState};

pub const MANIFEST_VERSION: u32 = 1;

/// `manifest.json` — identity of the conversation and where it lived.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    pub version: u32,
    /// Conversation UUID (the transcript basename).
    pub id: String,
    pub project_key: String,
    /// `project/branch` registry key when the conversation lived in a registered worktree.
    pub worktree_key: Option<String>,
    /// Checked-out branch (not necessarily the registered one — see `worktree_health`).
    pub branch: String,
    /// Base the tree was compared against (informational).
    pub base: Option<String>,
    /// Commit `branch` pointed at when packed.
    pub head: String,
    /// Conversation launch dir (what `--resume` and the transcript slug are anchored to).
    pub cwd: String,
    /// Git toplevel of the working tree — the path the tree is restored onto.
    pub root: String,
    /// True when `root` is the project's own checkout rather than a linked worktree.
    pub main_checkout: bool,
    /// Auth profile dir name under `$HOME` (`.claude`, `.claude-work`, …).
    pub profile: String,
    /// Transcript directory name under `<profile>/projects/`.
    pub slug: String,
    pub session_name: String,
    pub window_name: String,
    pub window_index: String,
    pub ports: Vec<u16>,
    pub note: String,
    pub packed_at: String,
    pub packed_on: String,
    pub with_ignored: bool,
    /// Whether `commits.bundle` exists (false when nothing was unpushed).
    pub has_bundle: bool,
    /// The `allowWrite` set the conversation ran under, when it ran sandboxed. Present ⇒
    /// the box is complete by construction: nothing outside these paths could have changed,
    /// and the box carries the ones that hold conversation state. Absent ⇒ best effort.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<Vec<String>>,
}

// ─── The gate ────────────────────────────────────────────────────────────────

/// Why a conversation can't be packed losslessly. Empty ⇒ safe.
///
/// A stop loses background tasks, subagents, queued messages and open prompts, so the
/// conversation must be quiescent by Claude's own account (`status`), and the transcript
/// must show no background launch still in flight. `status` is `None` for a conversation
/// with no live Claude process — nothing to lose on that side.
pub fn gate_blockers(status: Option<ClaudeSessionStatus>, background: Option<&str>) -> Vec<String> {
    let mut out = Vec::new();
    match status {
        Some(ClaudeSessionStatus::Busy) => out.push("a turn is running".to_string()),
        Some(ClaudeSessionStatus::Shell) => {
            out.push("background shells are still running".to_string())
        }
        Some(ClaudeSessionStatus::Waiting) => {
            out.push("a prompt is open and waiting for a decision".to_string())
        }
        Some(ClaudeSessionStatus::Idle) | None => {}
    }
    if let Some(s) = background {
        out.push(format!(
            "background work was launched and not finished: {s}"
        ));
    }
    out
}

/// Claude's own status for a LIVE conversation. The session files have no heartbeat, so a
/// record only counts when its pid is alive (`kill(pid, 0)`), which also guards a recycled
/// pid being believed.
fn live_status(id: &str) -> Option<ClaudeSessionStatus> {
    claude_sessions::load_all()
        .into_iter()
        .filter(|s| s.session_id.as_deref() == Some(id))
        .filter(|s| unsafe { libc::kill(s.pid as i32, 0) } == 0)
        .find_map(|s| s.status)
}

// ─── Git / rsync plumbing ────────────────────────────────────────────────────

fn git_raw(dir: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .context("failed to run git")?;
    if !out.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(out.stdout)
}

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    Ok(String::from_utf8_lossy(&git_raw(dir, args)?)
        .trim()
        .to_string())
}

fn git_ok(dir: &Path, args: &[&str]) -> bool {
    git_raw(dir, args).is_ok()
}

fn rsync(args: &[&str]) -> Result<()> {
    let out = Command::new("rsync")
        .args(args)
        .output()
        .context("failed to run rsync")?;
    if !out.status.success() {
        bail!(
            "rsync failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Backslash-escape rsync pattern metacharacters so a filename is matched literally.
fn rsync_escape(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for c in path.chars() {
        if matches!(c, '*' | '?' | '[' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// rsync exclude lines (anchored) for every git-ignored path under `root`.
fn ignored_excludes(root: &Path) -> Result<String> {
    let raw = git_raw(
        root,
        &[
            "ls-files",
            "-z",
            "--others",
            "--ignored",
            "--exclude-standard",
            "--directory",
        ],
    )?;
    let mut out = String::new();
    for p in raw.split(|b| *b == 0).filter(|p| !p.is_empty()) {
        out.push('/');
        out.push_str(&rsync_escape(&String::from_utf8_lossy(p)));
        out.push('\n');
    }
    Ok(out)
}

/// Commits on `branch` that exist on no remote.
fn unpushed_count(root: &Path, branch: &str) -> Result<usize> {
    Ok(
        git(root, &["rev-list", "--count", branch, "--not", "--remotes"])?
            .parse()
            .unwrap_or(0),
    )
}

/// What `pack_worktree` found.
#[derive(Debug, PartialEq)]
pub struct PackedTree {
    pub branch: String,
    pub head: String,
    pub base: Option<String>,
    pub main_checkout: bool,
    pub has_bundle: bool,
}

/// Snapshot the git state of the working tree at `root` into `dest` (created).
pub fn pack_worktree(root: &Path, dest: &Path, with_ignored: bool) -> Result<PackedTree> {
    let branch = git(root, &["symbolic-ref", "--short", "-q", "HEAD"]).map_err(|_| {
        anyhow!(
            "{} is on a detached HEAD; check out a branch first",
            root.display()
        )
    })?;
    let head = git(root, &["rev-parse", "HEAD"])?;
    let base = crate::common::worktree_health::resolve_base(&root.to_string_lossy());
    let main_checkout = {
        let gd = git(root, &["rev-parse", "--absolute-git-dir"])?;
        let common = git(
            root,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )?;
        fs::canonicalize(gd)? == fs::canonicalize(common)?
    };

    fs::create_dir_all(dest.join("tree"))?;

    let has_bundle = unpushed_count(root, &branch)? > 0;
    if has_bundle {
        let bundle = dest.join("commits.bundle");
        git(
            root,
            &[
                "bundle",
                "create",
                &bundle.to_string_lossy(),
                &format!("refs/heads/{branch}"),
                "--not",
                "--remotes",
            ],
        )?;
    }

    fs::write(
        dest.join("staged.patch"),
        git_raw(root, &["diff", "--cached", "--binary"])?,
    )?;

    let src = format!("{}/", root.display());
    let dst = format!("{}/", dest.join("tree").display());
    let mut args = vec!["-a", "--exclude", "/.git"];
    let excl_file = dest.join("ignored.exclude");
    if !with_ignored {
        fs::write(&excl_file, ignored_excludes(root)?)?;
        args.push("--exclude-from");
        args.push(excl_file.to_str().ok_or_else(|| anyhow!("non-utf8 path"))?);
    }
    args.push(&src);
    args.push(&dst);
    rsync(&args)?;
    let _ = fs::remove_file(&excl_file);

    Ok(PackedTree {
        branch,
        head,
        base,
        main_checkout,
        has_bundle,
    })
}

/// Is `dir` the top of its OWN git checkout? `rev-parse --git-dir` alone also succeeds in an
/// empty directory nested inside someone else's repository.
pub fn is_own_checkout(dir: &Path) -> bool {
    let Ok(top) = git(dir, &["rev-parse", "--show-toplevel"]) else {
        return false;
    };
    match (fs::canonicalize(top), fs::canonicalize(dir)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

fn dir_has_entries(dir: &Path) -> bool {
    dir.exists()
        && fs::read_dir(dir)
            .map(|mut d| d.next().is_some())
            .unwrap_or(true)
}

/// Everything `unpack_worktree` refuses on, with nothing touched. Public so `box pull` can
/// ask BEFORE it releases the remote copy: a pull that fails here must leave the other
/// machine exactly as it was.
pub fn check_target(m: &Manifest, root: &Path) -> Result<()> {
    if m.main_checkout {
        if !is_own_checkout(root) {
            // Not a checkout yet: fine when there is nothing there (it gets initialised),
            // never when there are files we would be writing a repository over.
            if dir_has_entries(root) {
                bail!(
                    "{} exists, is not a git checkout and is not empty",
                    root.display()
                );
            }
            return Ok(());
        }
        let cur = git(root, &["symbolic-ref", "--short", "-q", "HEAD"])
            .map_err(|_| anyhow!("{} is on a detached HEAD", root.display()))?;
        // Another branch is fine when the box's branch is new to this machine: the unpack
        // creates it from here (it still refuses unless HEAD is an ancestor of the box's
        // tip). A branch of that name that exists but isn't checked out may hold its own
        // commits, so that stays a refusal.
        if cur != m.branch && local_branch_exists(root, &m.branch) {
            bail!(
                "{} has '{}' checked out, the box is for '{}' (which exists here too); \
                 check it out first",
                root.display(),
                cur,
                m.branch
            );
        }
        if !git(root, &["status", "--porcelain"])?.is_empty() {
            bail!(
                "{} has uncommitted changes that exist only here; commit or stash them first",
                root.display()
            );
        }
    } else if dir_has_entries(root) {
        bail!("{} already exists and is not empty", root.display());
    }
    Ok(())
}

fn local_branch_exists(repo: &Path, branch: &str) -> bool {
    git_ok(
        repo,
        &[
            "rev-parse",
            "--verify",
            "-q",
            &format!("refs/heads/{branch}"),
        ],
    )
}

/// Restore the git state from `src` (a box dir) onto `root`.
///
/// `repo` is the project's own checkout: where the bundle is fetched into and, for a
/// linked worktree, where `git worktree add` runs. For a main-checkout box `repo == root`.
///
/// `after_checkout` runs for a linked worktree right after `git worktree add` and BEFORE the
/// tree is overlaid: project hooks (ports, env files) get a fresh worktree to act on, and
/// anything they write is then overwritten by the box, which is the truth.
pub fn unpack_worktree(
    src: &Path,
    m: &Manifest,
    repo: &Path,
    root: &Path,
    after_checkout: &mut dyn FnMut() -> Result<()>,
) -> Result<()> {
    // Preconditions first — refuse before anything is touched.
    check_target(m, root)?;

    // A project that exists only on the machine the box came from: no remote to clone, so
    // the box's bundle is the whole history and the checkout starts here, empty.
    if m.main_checkout && !is_own_checkout(repo) {
        fs::create_dir_all(repo)?;
        git(repo, &["init", "-q", "-b", &m.branch])?;
    }
    if !git_ok(repo, &["rev-parse", "--git-dir"]) {
        bail!("{} is not a git checkout", repo.display());
    }

    // 1. Make the tip commit available as a ref. Fetch into a scratch ref so a branch that
    //    is checked out (the main-checkout case) isn't the fetch target.
    let scratch = format!("refs/hive/box/{}", m.id);
    if m.has_bundle {
        let bundle = src.join("commits.bundle");
        let b = bundle.to_string_lossy().to_string();
        if !git_ok(repo, &["bundle", "verify", &b]) {
            // Prerequisites are commits already on a remote; the target may just be behind.
            let _ = git(repo, &["fetch", "--quiet", "origin"]);
        }
        git(
            repo,
            &[
                "fetch",
                "--quiet",
                &b,
                &format!("refs/heads/{}:{scratch}", m.branch),
            ],
        )?;
    } else {
        if !git_ok(repo, &["cat-file", "-e", &format!("{}^{{commit}}", m.head)]) {
            let _ = git(repo, &["fetch", "--quiet", "origin"]);
        }
        git(repo, &["update-ref", &scratch, &m.head]).map_err(|_| {
            anyhow!(
                "commit {} is not in {} and was not bundled (it should be on a remote — fetch it)",
                m.head,
                repo.display()
            )
        })?;
    }
    let tip = git(repo, &["rev-parse", &scratch])?;
    if tip != m.head {
        let _ = git(repo, &["update-ref", "-d", &scratch]);
        bail!("box tip {tip} does not match its manifest head {}", m.head);
    }

    // 2. Put the branch at the tip (never rewinding or discarding commits that exist here).
    let result = (|| -> Result<()> {
        let have = git(
            repo,
            &[
                "rev-parse",
                "--verify",
                "-q",
                &format!("refs/heads/{}", m.branch),
            ],
        )
        .ok();
        if m.main_checkout {
            // An unborn branch (a checkout initialised above) has nothing to rewind.
            let born = git_ok(root, &["rev-parse", "--verify", "-q", "HEAD"]);
            if born && !git_ok(root, &["merge-base", "--is-ancestor", "HEAD", &m.head]) {
                bail!(
                    "{} has commits that are not ancestors of the box tip; refusing to move it",
                    root.display()
                );
            }
            // The conversation moved to a branch this machine never had (it was made
            // over there): start it here, at HEAD. The tree is clean (`check_target`),
            // and step 4 moves the new branch to the box's tip.
            let cur = git(root, &["symbolic-ref", "--short", "-q", "HEAD"]).unwrap_or_default();
            if born && cur != m.branch {
                git(root, &["checkout", "-q", "-b", &m.branch])?;
            }
        } else {
            match have {
                Some(cur) if cur == m.head => {}
                Some(cur) => {
                    if !git_ok(repo, &["merge-base", "--is-ancestor", &cur, &m.head]) {
                        bail!("branch '{}' here has diverged from the box", m.branch);
                    }
                    git(repo, &["branch", "-f", &m.branch, &m.head])?;
                }
                None => {
                    git(repo, &["branch", &m.branch, &m.head])?;
                }
            }
            git(
                repo,
                &["worktree", "add", &root.to_string_lossy(), &m.branch],
            )?;
            after_checkout()?;
        }

        // 3. Working tree: overlay the box, then drop tracked files the box doesn't have.
        let tree = src.join("tree");
        rsync(&[
            "-a",
            "--exclude",
            "/.git",
            &format!("{}/", tree.display()),
            &format!("{}/", root.display()),
        ])?;
        let tracked = git_raw(root, &["ls-files", "-z"])?;
        for p in tracked.split(|b| *b == 0).filter(|p| !p.is_empty()) {
            let rel = String::from_utf8_lossy(p).to_string();
            if tree.join(&rel).symlink_metadata().is_err() {
                let _ = fs::remove_file(root.join(&rel));
            }
        }

        // 4. Index: `head` + the staged patch. `reset --mixed` moves a main checkout's
        //    branch and leaves the working tree (just restored) alone.
        git(root, &["reset", "-q", "--mixed", &m.head])?;
        let patch = src.join("staged.patch");
        if fs::metadata(&patch).map(|md| md.len() > 0).unwrap_or(false) {
            git(root, &["apply", "--cached", &patch.to_string_lossy()])?;
        }
        Ok(())
    })();
    let _ = git(repo, &["update-ref", "-d", &scratch]);
    result
}

// ─── Conversation-side files ─────────────────────────────────────────────────

/// `<home>/<profile>/projects/<slug>/<id>.jsonl` → (`<profile>` dir name, slug).
fn profile_and_slug(transcript: &Path) -> Result<(String, String)> {
    let slug_dir = transcript
        .parent()
        .ok_or_else(|| anyhow!("transcript has no parent dir"))?;
    let slug = slug_dir
        .file_name()
        .map(|s| s.to_string_lossy().to_string());
    let profile = slug_dir
        .parent()
        .and_then(|p| p.parent())
        .and_then(|p| p.file_name())
        .map(|s| s.to_string_lossy().to_string());
    match (profile, slug) {
        (Some(p), Some(s)) => Ok((p, s)),
        _ => Err(anyhow!(
            "unexpected transcript location {}",
            transcript.display()
        )),
    }
}

pub fn hostname() -> String {
    Command::new("hostname")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct BoxTodos {
    active: Vec<String>,
    done: Vec<String>,
}

pub fn boxes_dir() -> Result<PathBuf> {
    Ok(hive_home()
        .ok_or_else(|| anyhow!("cannot determine home directory"))?
        .join("boxes"))
}

/// What the caller already knows about the conversation (from the registry).
pub struct PackSource<'a> {
    pub id: &'a str,
    pub cwd: &'a str,
    pub note: &'a str,
    /// Live tmux placement (session, window index, window name) when running.
    pub placement: Option<(&'a str, &'a str, &'a str)>,
}

pub struct PackOptions {
    pub force: bool,
    pub with_ignored: bool,
}

/// Everything `pack` can decide before it touches a window or writes a byte.
pub struct PackPlan {
    pub manifest: Manifest,
    pub transcript: PathBuf,
    /// Reasons the gate refused (only non-empty when `--force` overrode it).
    pub overridden: Vec<String>,
}

/// Resolve identity and run the gate, without side effects.
pub fn plan_pack(src: &PackSource, opts: &PackOptions) -> Result<PackPlan> {
    let transcript = crate::common::jsonl::resolve_jsonl_path(src.cwd, Some(src.id))
        .ok_or_else(|| anyhow!("no transcript on disk for {}", src.id))?;
    let (profile, slug) = profile_and_slug(&transcript)?;

    let live = src.placement.is_some();
    let status = if live { live_status(src.id) } else { None };
    let background = crate::common::jsonl::background_running_summary(src.cwd, Some(src.id));
    let blockers = gate_blockers(status, background.as_deref());
    if !blockers.is_empty() && !opts.force {
        bail!(
            "not safe to pack {}:\n  - {}\nwait for it to go idle, or pass --force to pack anyway \
             (these are lost on unpack)",
            src.id,
            blockers.join("\n  - ")
        );
    }

    let cwd_path = Path::new(src.cwd);
    let root = git(cwd_path, &["rev-parse", "--show-toplevel"])
        .map_err(|_| anyhow!("{} is not inside a git working tree", src.cwd))?;
    let root_path = Path::new(&root);

    let projects = ProjectRegistry::load();
    let worktrees = WorktreeState::load();
    let parent = resolve_parent(src.cwd, &worktrees, &projects)
        .ok_or_else(|| anyhow!("{} belongs to no registered project", src.cwd))?;
    let (project_key, worktree_key, entry) = match worktrees.worktrees.get(&parent) {
        Some(e) => (e.project_key.clone(), Some(parent.clone()), Some(e)),
        None => (parent.clone(), None, None),
    };

    let branch = git(root_path, &["symbolic-ref", "--short", "-q", "HEAD"])
        .map_err(|_| anyhow!("{root} is on a detached HEAD; check out a branch first"))?;
    let head = git(root_path, &["rev-parse", "HEAD"])?;
    let main_checkout = {
        let gd = git(root_path, &["rev-parse", "--absolute-git-dir"])?;
        let common = git(
            root_path,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )?;
        fs::canonicalize(gd)? == fs::canonicalize(common)?
    };

    let ports = entry
        .and_then(|e| e.metadata.get("ports"))
        .and_then(|p| p.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_u64())
                .map(|v| v as u16)
                .collect()
        })
        .unwrap_or_default();
    let owner_session = entry.map(|e| e.session_name.clone()).or_else(|| {
        projects
            .projects
            .get(&project_key)
            .map(|c| ProjectRegistry::session_name(&project_key, c))
    });
    let (win_session, win_index, win_name) = src
        .placement
        .map(|(s, i, n)| (s.to_string(), i.to_string(), n.to_string()))
        .unwrap_or_default();

    let manifest = Manifest {
        version: MANIFEST_VERSION,
        id: src.id.to_string(),
        project_key,
        worktree_key,
        branch,
        base: crate::common::worktree_health::resolve_base(&root),
        head,
        cwd: src.cwd.to_string(),
        root,
        main_checkout,
        profile,
        slug,
        session_name: owner_session.unwrap_or(win_session),
        window_name: win_name,
        window_index: win_index,
        ports,
        note: src.note.to_string(),
        packed_at: chrono::Utc::now().to_rfc3339(),
        packed_on: hostname(),
        with_ignored: opts.with_ignored,
        has_bundle: false, // settled by pack_worktree
        sandbox: crate::common::sandbox::recorded_write_set(src.id),
    };
    Ok(PackPlan {
        manifest,
        transcript,
        overridden: blockers,
    })
}

/// Write the box for `plan` (the caller has frozen the window by now) and return its dir.
pub fn write_box(plan: &PackPlan) -> Result<PathBuf> {
    let m = &plan.manifest;
    let dir = boxes_dir()?;
    fs::create_dir_all(&dir)?;
    let tmp = dir.join(format!("{}.tmp-{}", m.id, std::process::id()));
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp)?;

    let built = (|| -> Result<Manifest> {
        let packed = pack_worktree(Path::new(&m.root), &tmp, m.with_ignored)?;
        if packed.branch != m.branch || packed.head != m.head {
            bail!("the branch moved while packing; try again");
        }
        let mut manifest = m.clone();
        manifest.has_bundle = packed.has_bundle;

        fs::copy(&plan.transcript, tmp.join("transcript.jsonl"))?;
        let memory = plan.transcript.parent().map(|p| p.join("memory"));
        if let Some(mem) = memory.filter(|p| p.is_dir()) {
            fs::create_dir_all(tmp.join("memory"))?;
            rsync(&[
                "-a",
                &format!("{}/", mem.display()),
                &format!("{}/", tmp.join("memory").display()),
            ])?;
        }

        let overlay = ConversationSidecar::load()
            .conversations
            .get(&m.id)
            .cloned()
            .unwrap_or_default();
        fs::write(
            tmp.join("overlay.json"),
            serde_json::to_string_pretty(&overlay)?,
        )?;

        let todos = BoxTodos {
            active: crate::common::persistence::load_session_todos()
                .remove(&m.session_name)
                .unwrap_or_default(),
            done: crate::common::persistence::load_completed_todos()
                .remove(&m.session_name)
                .unwrap_or_default(),
        };
        fs::write(
            tmp.join("todos.json"),
            serde_json::to_string_pretty(&todos)?,
        )?;

        // Manifest last: its presence is what marks the box complete.
        fs::write(
            tmp.join("manifest.json"),
            serde_json::to_string_pretty(&manifest)?,
        )?;
        Ok(manifest)
    })();

    match built {
        Ok(_) => {
            let final_dir = dir.join(&m.id);
            let _ = fs::remove_dir_all(&final_dir); // a re-pack replaces its own earlier box
            fs::rename(&tmp, &final_dir)?;
            Ok(final_dir)
        }
        Err(e) => {
            let _ = fs::remove_dir_all(&tmp);
            Err(e)
        }
    }
}

/// A box id becomes a directory name and a URL segment: hex digits and dashes only.
pub fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// `.claude`, `.claude-work`, … — a manifest's `profile` is joined onto `$HOME`.
fn valid_profile(p: &str) -> bool {
    p.starts_with(".claude")
        && p.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
        && !p.contains("..")
}

/// A transcript slug is one directory name.
fn valid_slug(s: &str) -> bool {
    !s.is_empty() && s != "." && s != ".." && !s.contains('/') && !s.contains('\\')
}

/// Where a box may land. A manifest names an absolute path to write a whole working tree
/// onto, and a box can arrive from another machine, so the target must already belong to a
/// registered project: its own checkout, anywhere under its worktrees dir or checkout, or a
/// worktree already registered at exactly that path. Relative paths and `..` are refused
/// outright rather than normalised.
pub fn root_allowed(
    root: &Path,
    project_root: &Path,
    worktrees_dir: Option<&Path>,
    registered: &[PathBuf],
) -> bool {
    use std::path::Component;
    if !root.is_absolute() || root.components().any(|c| matches!(c, Component::ParentDir)) {
        return false;
    }
    root.starts_with(project_root)
        || worktrees_dir.is_some_and(|w| root.starts_with(w) && root != w)
        || registered.iter().any(|r| r == root)
}

/// `tar` a box directory into `out` (a temp file the caller owns).
///
/// `COPYFILE_DISABLE`: macOS `tar` otherwise stores extended attributes and resource forks
/// as extra `._name` entries (AppleDouble). GNU tar on the receiving Linux box extracts
/// those as real files, and every tracked file grows an untracked `._` twin in the worktree.
pub fn tar_box(dir: &Path, out: &Path) -> Result<()> {
    let o = Command::new("tar")
        .env("COPYFILE_DISABLE", "1")
        .arg("-C")
        .arg(dir)
        .arg("-cf")
        .arg(out)
        .arg(".")
        .output()
        .context("failed to run tar")?;
    if !o.status.success() {
        bail!("tar failed: {}", String::from_utf8_lossy(&o.stderr).trim());
    }
    Ok(())
}

/// Extract a tar stream into `dest` (created; must be empty or new). The stream may come
/// from another machine, so extraction is into a scratch directory the caller validates
/// before moving it into place — never straight into `~/.hive/boxes`.
pub fn untar_box(input: &mut dyn std::io::Read, dest: &Path) -> Result<()> {
    use std::process::Stdio;
    fs::create_dir_all(dest)?;
    let mut child = Command::new("tar")
        .arg("-C")
        .arg(dest)
        .arg("-xf")
        .arg("-")
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to run tar")?;
    let copied = std::io::copy(input, child.stdin.as_mut().unwrap());
    drop(child.stdin.take());
    let out = child.wait_with_output()?;
    copied?;
    if !out.status.success() {
        bail!(
            "tar failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Move a freshly extracted box into `~/.hive/boxes/<id>` once its manifest checks out
/// (right id, supported version). A re-send replaces the earlier copy.
pub fn install_received_box(scratch: &Path, id: &str) -> Result<PathBuf> {
    if !valid_id(id) {
        bail!("invalid box id");
    }
    let m = read_manifest(scratch)?;
    if m.id != id {
        bail!("box says it is {} but was sent as {id}", m.id);
    }
    let dir = boxes_dir()?;
    fs::create_dir_all(&dir)?;
    let final_dir = dir.join(id);
    let _ = fs::remove_dir_all(&final_dir);
    fs::rename(scratch, &final_dir)?;
    Ok(final_dir)
}

pub fn read_manifest(dir: &Path) -> Result<Manifest> {
    let raw = fs::read_to_string(dir.join("manifest.json"))
        .with_context(|| format!("{} has no manifest.json (incomplete box?)", dir.display()))?;
    let m: Manifest = serde_json::from_str(&raw)?;
    if m.version != MANIFEST_VERSION {
        bail!(
            "box version {} is not supported (this hive reads {})",
            m.version,
            MANIFEST_VERSION
        );
    }
    Ok(m)
}

/// Every complete box under `~/.hive/boxes`.
pub fn list_boxes() -> Vec<Manifest> {
    let Ok(dir) = boxes_dir() else {
        return Vec::new();
    };
    let mut out: Vec<Manifest> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .filter_map(|e| read_manifest(&e.path()).ok())
        .collect();
    out.sort_by(|a, b| b.packed_at.cmp(&a.packed_at));
    out
}

/// Record where a conversation now lives. `None` ⇒ here again.
pub fn set_location(id: &str, location: Option<String>) -> Result<()> {
    ConversationSidecar::edit(id, |o| o.location = location)
}

/// Remove this machine's working copy of a packed conversation — only a LINKED worktree
/// (never a project's own checkout), and only the directory: the branch, transcript and
/// box stay. Callers do this once the box is safely on another disk.
pub fn remove_worktree_copy(m: &Manifest) -> Result<()> {
    if m.main_checkout {
        // A project's own checkout is never removed, but what the box just carried away is
        // still lying in it — and a main checkout must be clean to take the box back, so
        // left alone it refuses its own conversation's return. Set it aside in a stash:
        // nothing is discarded, and the tree is clean for the round trip.
        let root = Path::new(&m.root);
        if is_own_checkout(root) && !git(root, &["status", "--porcelain"])?.is_empty() {
            git(
                root,
                &[
                    "stash",
                    "push",
                    "-u",
                    "-q",
                    "-m",
                    &format!("hive box {}: shipped to another machine", m.id),
                ],
            )?;
        }
        return Ok(());
    }
    let projects = ProjectRegistry::load();
    let config = projects
        .projects
        .get(&m.project_key)
        .ok_or_else(|| anyhow!("project '{}' is not registered here", m.project_key))?;
    let repo = expand_tilde(&config.project_root);
    if Path::new(&m.root).exists() {
        git(&repo, &["worktree", "remove", "--force", &m.root])?;
    }
    if let Some(key) = &m.worktree_key {
        let mut state = WorktreeState::load();
        if state.worktrees.remove(key).is_some() {
            state.save()?;
        }
    }
    Ok(())
}

/// The project's own checkout on THIS machine — where a box's bundle is fetched into.
pub fn repo_for(m: &Manifest) -> Result<PathBuf> {
    let projects = ProjectRegistry::load();
    let config = projects.projects.get(&m.project_key).ok_or_else(|| {
        anyhow!(
            "project '{}' is not registered here (register it at the same path first)",
            m.project_key
        )
    })?;
    Ok(expand_tilde(&config.project_root))
}

pub struct UnpackOptions {
    pub force: bool,
}

/// Restore a box onto this machine. Returns the frozen-entry key (the conversation id),
/// ready for `frozen::thaw_window`.
pub fn unpack(src: &Path, opts: &UnpackOptions) -> Result<String> {
    let m = read_manifest(src)?;
    let home = dirs::home_dir().ok_or_else(|| anyhow!("cannot determine home directory"))?;

    // Same absolute path or refuse: the transcript slug and every cwd in it are anchored there.
    let projects = ProjectRegistry::load();
    let config = projects.projects.get(&m.project_key).ok_or_else(|| {
        anyhow!(
            "project '{}' is not registered here (register it at the same path first)",
            m.project_key
        )
    })?;
    let repo = expand_tilde(&config.project_root);
    let root = PathBuf::from(&m.root);
    if !valid_id(&m.id) || !valid_profile(&m.profile) || !valid_slug(&m.slug) {
        bail!("box {} has an unsafe id, profile or slug; refusing", m.id);
    }
    let worktrees = WorktreeState::load();
    let registered: Vec<PathBuf> = worktrees
        .worktrees
        .values()
        .filter(|e| e.project_key == m.project_key)
        .map(|e| expand_tilde(&e.path))
        .collect();
    if !root_allowed(
        &root,
        &repo,
        projects
            .resolve_worktrees_dir(&m.project_key, config)
            .as_deref(),
        &registered,
    ) {
        bail!(
            "{} is outside project '{}' (its checkout {} and worktrees dir); refusing to write there",
            root.display(),
            m.project_key,
            repo.display()
        );
    }
    if m.main_checkout && repo != root {
        bail!(
            "project '{}' lives at {} here but the box was packed from {}; \
             the paths must match",
            m.project_key,
            repo.display(),
            root.display()
        );
    }

    let profile_dir = home.join(&m.profile);
    if !profile_dir.is_dir() {
        bail!(
            "{} does not exist here — log in to that Claude profile on this machine first",
            profile_dir.display()
        );
    }
    let slug_dir = profile_dir.join("projects").join(&m.slug);
    let target_transcript = slug_dir.join(format!("{}.jsonl", m.id));
    let box_transcript = src.join("transcript.jsonl");
    if !opts.force {
        let have = fs::metadata(&target_transcript)
            .map(|md| md.len())
            .unwrap_or(0);
        let incoming = fs::metadata(&box_transcript)?.len();
        if have > incoming {
            bail!(
                "this machine already has a longer transcript for {} ({have} > {incoming} bytes); \
                 it has turns the box doesn't — pass --force to overwrite",
                m.id
            );
        }
    }

    // Project hooks, as a fresh `hive wt new` would run them. What they report (allocated
    // ports, a session-name override) becomes the registered worktree's metadata.
    let mut metadata = serde_json::json!({ "ports": m.ports });
    let hooks_dir = crate::common::worktree::resolve_hooks_dir(config, &m.project_key);
    let registered_branch = m
        .worktree_key
        .as_deref()
        .and_then(|k| k.strip_prefix(&format!("{}/", m.project_key)))
        .unwrap_or(&m.branch)
        .to_string();
    let mut session_name = m.session_name.clone();
    // Asked before the unpack creates it: a checkout we initialise is as new to Claude as a
    // worktree is.
    let fresh_main = m.main_checkout && !is_own_checkout(&repo);
    unpack_worktree(src, &m, &repo, &root, &mut || {
        if m.worktree_key.is_none() {
            return Ok(());
        }
        let mut env = crate::common::worktree::build_hook_env(
            &m.project_key,
            &registered_branch,
            &root,
            &repo,
            &session_name,
            "worktree",
            &hooks_dir,
        );
        for hook in ["post-worktree", "post-copy"] {
            metadata = crate::common::worktree::run_hook(&hooks_dir, hook, &env, &metadata)?;
        }
        if let Some(name) = metadata.get("session_name").and_then(|v| v.as_str()) {
            session_name = name.to_string();
            env.insert("HIVE_SESSION_NAME".to_string(), session_name.clone());
        }
        Ok(())
    })?;

    // `wt new` pre-trusts the worktrees it makes; a box lands one without it, and a resumed
    // conversation would sit at "Do you trust this folder?" until someone attached. Linked
    // worktrees, and a main checkout this unpack created: a project's own checkout that was
    // already here is one the user set up. Best effort — a failure costs a prompt, not the
    // unpack.
    if !m.main_checkout || fresh_main {
        let cfg = if m.profile == ".claude" {
            home.join(".claude.json")
        } else {
            home.join(&m.profile).join(".claude.json")
        };
        for dir in [Path::new(&m.root), Path::new(&m.cwd)] {
            if dir.starts_with(&m.root) {
                if let Err(e) = crate::common::worktree::pretrust_in(&cfg, dir) {
                    eprintln!("warning: could not pre-trust {}: {e}", dir.display());
                }
            }
        }
    }

    fs::create_dir_all(&slug_dir)?;
    fs::copy(&box_transcript, &target_transcript)?;
    if src.join("memory").is_dir() {
        fs::create_dir_all(slug_dir.join("memory"))?;
        rsync(&[
            "-a",
            &format!("{}/", src.join("memory").display()),
            &format!("{}/", slug_dir.join("memory").display()),
        ])?;
    }

    // Register the worktree so the conversation resolves to its parent here too.
    if let Some(key) = &m.worktree_key {
        let mut state = WorktreeState::load();
        if !state.worktrees.contains_key(key) {
            let branch = key
                .strip_prefix(&format!("{}/", m.project_key))
                .unwrap_or(&m.branch)
                .to_string();
            state.add(WorktreeEntry {
                project_key: m.project_key.clone(),
                branch,
                worktree_type: "worktree".to_string(),
                path: m.root.clone(),
                session_name: session_name.clone(),
                metadata: metadata.clone(),
                created_at: chrono::Utc::now().to_rfc3339(),
            });
            state.save()?;
        }
    }

    let overlay: crate::common::registry::ConversationOverlay =
        serde_json::from_str(&fs::read_to_string(src.join("overlay.json"))?)?;
    ConversationSidecar::edit(&m.id, |o| *o = overlay)?;

    if let Ok(raw) = fs::read_to_string(src.join("todos.json")) {
        if let Ok(t) = serde_json::from_str::<BoxTodos>(&raw) {
            merge_todos(&m.session_name, t);
        }
    }

    Ok(m.id)
}

fn merge_todos(session: &str, incoming: BoxTodos) {
    use crate::common::persistence::*;
    let mut active = load_session_todos();
    let list = active.entry(session.to_string()).or_default();
    for t in incoming.active {
        if !list.contains(&t) {
            list.push(t);
        }
    }
    save_session_todos(&active);
    let mut done = load_completed_todos();
    let list = done.entry(session.to_string()).or_default();
    for t in incoming.done {
        if !list.contains(&t) {
            list.push(t);
        }
    }
    save_completed_todos(&done);
}

/// The frozen entry that `thaw_window` resumes from, built from a box's manifest.
pub fn frozen_entry_for(m: &Manifest) -> crate::common::frozen::FrozenEntry {
    let claude_config_dir = (m.profile != ".claude")
        .then(|| dirs::home_dir().map(|h| h.join(&m.profile).to_string_lossy().to_string()))
        .flatten();
    crate::common::frozen::FrozenEntry {
        session_name: m.session_name.clone(),
        window_name: m.window_name.clone(),
        window_index: m.window_index.clone(),
        cwd: m.cwd.clone(),
        claude_session_id: Some(m.id.clone()),
        claude_config_dir,
        note: m.note.clone(),
        frozen_at: chrono::Utc::now().to_rfc3339(),
        sandbox: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(dir: &Path, args: &[&str]) -> String {
        git(dir, args).unwrap()
    }

    fn init_repo(base: &Path) -> (PathBuf, PathBuf) {
        let origin = base.join("origin.git");
        let main = base.join("main");
        Command::new("git")
            .args(["init", "-q", "--bare", &origin.to_string_lossy()])
            .output()
            .unwrap();
        Command::new("git")
            .args([
                "clone",
                "-q",
                &origin.to_string_lossy(),
                &main.to_string_lossy(),
            ])
            .output()
            .unwrap();
        for (k, v) in [("user.email", "a@b"), ("user.name", "t")] {
            sh(&main, &["config", k, v]);
        }
        fs::write(main.join("a.txt"), "base\n").unwrap();
        fs::write(main.join(".gitignore"), "*.log\ntarget/\n").unwrap();
        sh(&main, &["add", "."]);
        sh(&main, &["commit", "-qm", "base"]);
        sh(&main, &["branch", "-M", "main"]);
        sh(&main, &["push", "-q", "origin", "main"]);
        (origin, main)
    }

    fn manifest_for(root: &Path, p: &PackedTree, id: &str) -> Manifest {
        Manifest {
            version: MANIFEST_VERSION,
            id: id.into(),
            project_key: "p".into(),
            worktree_key: None,
            branch: p.branch.clone(),
            base: p.base.clone(),
            head: p.head.clone(),
            cwd: root.to_string_lossy().into(),
            root: root.to_string_lossy().into(),
            main_checkout: p.main_checkout,
            profile: ".claude".into(),
            slug: "s".into(),
            session_name: "p".into(),
            window_name: String::new(),
            window_index: String::new(),
            ports: vec![],
            note: String::new(),
            packed_at: String::new(),
            packed_on: String::new(),
            with_ignored: true,
            has_bundle: p.has_bundle,
            sandbox: None,
        }
    }

    #[test]
    fn gate_passes_idle_and_closed() {
        assert!(gate_blockers(Some(ClaudeSessionStatus::Idle), None).is_empty());
        assert!(gate_blockers(None, None).is_empty());
    }

    #[test]
    fn gate_refuses_busy_shell_waiting_and_orphaned_background() {
        for s in [
            ClaudeSessionStatus::Busy,
            ClaudeSessionStatus::Shell,
            ClaudeSessionStatus::Waiting,
        ] {
            assert_eq!(gate_blockers(Some(s), None).len(), 1, "{s:?}");
        }
        let b = gate_blockers(Some(ClaudeSessionStatus::Idle), Some("1 workflow"));
        assert_eq!(b.len(), 1);
        assert!(b[0].contains("1 workflow"));
        // A closed conversation with an in-flight launch is still lossy.
        assert_eq!(gate_blockers(None, Some("dev server")).len(), 1);
    }

    #[test]
    fn manifest_roundtrips() {
        let m = manifest_for(
            Path::new("/x"),
            &PackedTree {
                branch: "feat".into(),
                head: "abc".into(),
                base: Some("origin/main".into()),
                main_checkout: false,
                has_bundle: true,
            },
            "id1",
        );
        let back: Manifest = serde_json::from_str(&serde_json::to_string(&m).unwrap()).unwrap();
        assert_eq!(m, back);
    }

    #[test]
    fn rsync_escape_neutralises_globs() {
        assert_eq!(rsync_escape("a*b[c]?\\"), "a\\*b\\[c]\\?\\\\");
        assert_eq!(rsync_escape("plain name"), "plain name");
    }

    #[test]
    fn bundle_carries_only_unpushed_commits_under_the_branch_name() {
        let tmp = tempdir("bundle");
        let (_o, main) = init_repo(&tmp);
        let wt = tmp.join("wt");
        sh(
            &main,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "feat",
                &wt.to_string_lossy(),
                "origin/main",
            ],
        );
        for n in ["c1", "c2"] {
            fs::write(wt.join(format!("{n}.txt")), n).unwrap();
            sh(&wt, &["add", "."]);
            sh(&wt, &["commit", "-qm", n]);
        }
        let dest = tmp.join("box");
        let p = pack_worktree(&wt, &dest, true).unwrap();
        assert!(p.has_bundle && !p.main_checkout && p.branch == "feat");
        // Exactly the two unpushed commits, reachable as refs/heads/feat (not HEAD).
        let heads = sh(
            &main,
            &[
                "bundle",
                "list-heads",
                &dest.join("commits.bundle").to_string_lossy(),
            ],
        );
        assert!(heads.contains("refs/heads/feat"), "{heads}");
        assert!(!heads.contains(" HEAD"), "{heads}");
        let objs = sh(&wt, &["rev-list", "--count", "feat", "--not", "--remotes"]);
        assert_eq!(objs, "2");
    }

    #[test]
    fn no_bundle_when_everything_is_pushed() {
        let tmp = tempdir("nobundle");
        let (_o, main) = init_repo(&tmp);
        let p = pack_worktree(&main, &tmp.join("box"), true).unwrap();
        assert!(!p.has_bundle && p.main_checkout);
        assert!(!tmp.join("box/commits.bundle").exists());
    }

    #[test]
    fn staged_patch_holds_only_the_index() {
        let tmp = tempdir("staged");
        let (_o, main) = init_repo(&tmp);
        fs::write(main.join("s.txt"), "staged").unwrap();
        sh(&main, &["add", "s.txt"]);
        fs::write(main.join("a.txt"), "unstaged edit\n").unwrap();
        pack_worktree(&main, &tmp.join("box"), true).unwrap();
        let patch = fs::read_to_string(tmp.join("box/staged.patch")).unwrap();
        assert!(
            patch.contains("s.txt") && !patch.contains("a.txt"),
            "{patch}"
        );
    }

    #[test]
    fn without_ignored_leaves_ignored_files_out() {
        let tmp = tempdir("ignored");
        let (_o, main) = init_repo(&tmp);
        fs::write(main.join("x.log"), "junk").unwrap();
        fs::create_dir_all(main.join("target")).unwrap();
        fs::write(main.join("target/bin"), "b").unwrap();
        fs::write(main.join("keep.txt"), "k").unwrap();
        pack_worktree(&main, &tmp.join("with"), true).unwrap();
        pack_worktree(&main, &tmp.join("without"), false).unwrap();
        assert!(tmp.join("with/tree/x.log").exists() && tmp.join("with/tree/target/bin").exists());
        assert!(!tmp.join("without/tree/x.log").exists());
        assert!(!tmp.join("without/tree/target").exists());
        assert!(tmp.join("without/tree/keep.txt").exists());
        assert!(!tmp.join("without/tree/.git").exists());
    }

    #[test]
    fn worktree_roundtrip_is_identical() {
        let tmp = tempdir("roundtrip");
        let (_o, main) = init_repo(&tmp);
        let wt = tmp.join("wt");
        sh(
            &main,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "feat",
                &wt.to_string_lossy(),
                "origin/main",
            ],
        );
        for n in ["c1", "c2"] {
            fs::write(wt.join(format!("{n}.txt")), n).unwrap();
            sh(&wt, &["add", "."]);
            sh(&wt, &["commit", "-qm", n]);
        }
        fs::write(wt.join("c1.txt"), "c1 edited").unwrap(); // unstaged
        fs::write(wt.join("s.txt"), "staged").unwrap();
        sh(&wt, &["add", "s.txt"]);
        fs::write(wt.join("untracked.txt"), "un").unwrap();
        fs::write(wt.join("x.log"), "ignored").unwrap();
        fs::remove_file(wt.join("a.txt")).unwrap(); // tracked file deleted, unstaged
        let status = sh(&wt, &["status", "--short"]);
        let log = sh(&wt, &["log", "--oneline"]);

        let box_dir = tmp.join("box");
        let p = pack_worktree(&wt, &box_dir, true).unwrap();
        let m = manifest_for(&wt, &p, "id1");

        // Destroy the worktree and the branch.
        sh(
            &main,
            &["worktree", "remove", "--force", &wt.to_string_lossy()],
        );
        sh(&main, &["branch", "-D", "feat"]);

        unpack_worktree(&box_dir, &m, &main, &wt, &mut || Ok(())).unwrap();
        assert_eq!(sh(&wt, &["status", "--short"]), status);
        assert_eq!(sh(&wt, &["log", "--oneline"]), log);
        assert_eq!(sh(&wt, &["rev-parse", "HEAD"]), m.head);
        assert_eq!(fs::read_to_string(wt.join("c1.txt")).unwrap(), "c1 edited");
        assert_eq!(fs::read_to_string(wt.join("x.log")).unwrap(), "ignored");
        assert!(
            !wt.join("a.txt").exists(),
            "deleted tracked file stayed deleted"
        );
        // The scratch ref doesn't leak.
        assert!(sh(&main, &["for-each-ref", "refs/hive"]).is_empty());
    }

    #[test]
    fn main_checkout_unpack_refuses_dirty_tree() {
        let tmp = tempdir("dirty");
        let (_o, main) = init_repo(&tmp);
        let box_dir = tmp.join("box");
        let p = pack_worktree(&main, &box_dir, true).unwrap();
        let m = manifest_for(&main, &p, "id1");
        fs::write(main.join("a.txt"), "local only").unwrap();
        let err = unpack_worktree(&box_dir, &m, &main, &main, &mut || Ok(())).unwrap_err();
        assert!(err.to_string().contains("uncommitted"), "{err}");
        assert_eq!(
            fs::read_to_string(main.join("a.txt")).unwrap(),
            "local only"
        );
    }

    #[test]
    fn main_checkout_unpacks_onto_a_machine_that_has_no_repo_at_all() {
        // A project with no remote: the bundle is the whole history, the target path is empty.
        let tmp = tempdir("norepo");
        let src = tmp.join("src");
        fs::create_dir_all(&src).unwrap();
        sh(&src, &["init", "-q", "-b", "main"]);
        sh(&src, &["config", "user.email", "t@t"]);
        sh(&src, &["config", "user.name", "t"]);
        fs::write(src.join("a.txt"), "one").unwrap();
        sh(&src, &["add", "a.txt"]);
        sh(&src, &["commit", "-q", "-m", "first"]);
        fs::write(src.join("b.txt"), "untracked").unwrap();
        let box_dir = tmp.join("box");
        let p = pack_worktree(&src, &box_dir, true).unwrap();
        assert!(p.has_bundle && p.main_checkout);
        let m = manifest_for(&src, &p, "id1");

        let dst = tmp.join("elsewhere").join("proj");
        assert!(!is_own_checkout(&dst));
        unpack_worktree(&box_dir, &m, &dst, &dst, &mut || Ok(())).unwrap();
        assert!(is_own_checkout(&dst));
        assert_eq!(sh(&dst, &["rev-parse", "HEAD"]), p.head);
        assert_eq!(sh(&dst, &["symbolic-ref", "--short", "HEAD"]), "main");
        assert_eq!(fs::read_to_string(dst.join("a.txt")).unwrap(), "one");
        assert_eq!(sh(&dst, &["status", "--porcelain"]), "?? b.txt");
    }

    #[test]
    fn main_checkout_unpack_follows_the_box_onto_a_branch_made_elsewhere() {
        // Shipped on `main`, the conversation branched and committed on the other machine.
        // Coming back, this checkout is still on `main` and has never seen that branch.
        let tmp = tempdir("newbranch");
        let src = tmp.join("src");
        fs::create_dir_all(&src).unwrap();
        sh(&src, &["init", "-q", "-b", "main"]);
        sh(&src, &["config", "user.email", "t@t"]);
        sh(&src, &["config", "user.name", "t"]);
        fs::write(src.join("a.txt"), "one").unwrap();
        sh(&src, &["add", "a.txt"]);
        sh(&src, &["commit", "-q", "-m", "first"]);
        let dst = tmp.join("dst");
        sh(&tmp, &["clone", "-q", "src", "dst"]);

        sh(&src, &["checkout", "-q", "-b", "step-1"]);
        fs::write(src.join("a.txt"), "second version").unwrap();
        sh(&src, &["commit", "-q", "-am", "second"]);
        let box_dir = tmp.join("box");
        let p = pack_worktree(&src, &box_dir, true).unwrap();
        let m = manifest_for(&src, &p, "id1");

        check_target(&m, &dst).unwrap();
        unpack_worktree(&box_dir, &m, &dst, &dst, &mut || Ok(())).unwrap();
        assert_eq!(sh(&dst, &["symbolic-ref", "--short", "HEAD"]), "step-1");
        assert_eq!(sh(&dst, &["rev-parse", "HEAD"]), p.head);
        assert_eq!(
            fs::read_to_string(dst.join("a.txt")).unwrap(),
            "second version"
        );
        assert_eq!(sh(&dst, &["status", "--porcelain"]), "");
        // `main` stays where it was: nothing here was rewound or moved.
        assert_ne!(sh(&dst, &["rev-parse", "main"]), p.head);

        // A branch of that name that already exists here, not checked out, is refused.
        let other = tmp.join("other");
        sh(&tmp, &["clone", "-q", "src", "other"]);
        sh(&other, &["checkout", "-q", "-b", "main2"]);
        let err = check_target(&m, &other).unwrap_err();
        assert!(err.to_string().contains("exists here too"), "{err}");
    }

    #[test]
    fn main_checkout_unpack_refuses_a_non_empty_directory_that_is_not_a_repo() {
        let tmp = tempdir("notrepo");
        let (_o, main) = init_repo(&tmp);
        let box_dir = tmp.join("box");
        let p = pack_worktree(&main, &box_dir, true).unwrap();
        let m = manifest_for(&main, &p, "id1");
        let dst = tmp.join("plain");
        fs::create_dir_all(&dst).unwrap();
        fs::write(dst.join("keep.txt"), "mine").unwrap();
        let err = check_target(&m, &dst).unwrap_err();
        assert!(err.to_string().contains("not a git checkout"), "{err}");
        assert!(unpack_worktree(&box_dir, &m, &dst, &dst, &mut || Ok(())).is_err());
        assert!(!dst.join(".git").exists());
    }

    #[test]
    fn releasing_a_main_checkout_stashes_what_the_box_carried_and_keeps_the_checkout() {
        let tmp = tempdir("release-main");
        let (_o, main) = init_repo(&tmp);
        fs::write(main.join("a.txt"), "edited").unwrap();
        fs::write(main.join("new.txt"), "untracked").unwrap();
        let box_dir = tmp.join("box");
        let p = pack_worktree(&main, &box_dir, true).unwrap();
        let m = manifest_for(&main, &p, "id1");
        remove_worktree_copy(&m).unwrap();
        assert!(main.join(".git").exists());
        assert_eq!(sh(&main, &["status", "--porcelain"]), "");
        assert!(sh(&main, &["stash", "list"]).contains("hive box id1"));
        // Clean now, so the same box comes back, dirty state and all.
        unpack_worktree(&box_dir, &m, &main, &main, &mut || Ok(())).unwrap();
        assert_eq!(fs::read_to_string(main.join("a.txt")).unwrap(), "edited");
        assert!(main.join("new.txt").exists());
    }

    #[test]
    fn worktree_unpack_refuses_non_empty_path() {
        let tmp = tempdir("nonempty");
        let (_o, main) = init_repo(&tmp);
        let wt = tmp.join("wt");
        sh(
            &main,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "feat",
                &wt.to_string_lossy(),
                "origin/main",
            ],
        );
        let box_dir = tmp.join("box");
        let p = pack_worktree(&wt, &box_dir, true).unwrap();
        let m = manifest_for(&wt, &p, "id1");
        let err = unpack_worktree(&box_dir, &m, &main, &wt, &mut || Ok(())).unwrap_err();
        assert!(err.to_string().contains("not empty"), "{err}");
    }

    fn tempdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("hive-boxes-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        // Resolve /var → /private/var so git's reported paths match ours.
        fs::canonicalize(d).unwrap()
    }

    #[test]
    fn boxes_may_only_land_inside_a_registered_project() {
        let proj = Path::new("/w/proj");
        let wts = Path::new("/w/worktrees/proj");
        let reg = [PathBuf::from("/elsewhere/known")];
        let ok = |r: &str| root_allowed(Path::new(r), proj, Some(wts), &reg);
        assert!(ok("/w/proj"));
        assert!(ok("/w/worktrees/proj/feat"));
        assert!(ok("/elsewhere/known"), "an already-registered worktree");
        assert!(!ok("/w/worktrees/proj"), "the worktrees dir itself");
        assert!(!ok("/etc"));
        assert!(!ok("/Users/someone/.ssh"));
        assert!(!ok("/w/worktrees/proj/../../../etc"), "no traversal");
        assert!(!ok("relative/path"));
        assert!(!root_allowed(
            Path::new("/w/worktrees/proj/x"),
            proj,
            None,
            &[]
        ));
    }

    #[test]
    fn manifest_names_that_become_paths_are_checked() {
        assert!(valid_id("aaaaaaaa-2222-3333-4444-555555555555"));
        assert!(!valid_id("../x") && !valid_id("") && !valid_id("a/b"));
        assert!(valid_profile(".claude") && valid_profile(".claude-work"));
        assert!(!valid_profile(".ssh") && !valid_profile(".claude/../.ssh"));
        assert!(valid_slug("-Users-me-proj") && !valid_slug("../x") && !valid_slug("a/b"));
    }

    /// Entry names of an (uncompressed) tar file, read straight from the headers —
    /// macOS `tar -t` silently merges AppleDouble entries and would hide exactly what
    /// this checks for.
    fn raw_tar_names(path: &Path) -> Vec<String> {
        let b = fs::read(path).unwrap();
        let (mut off, mut names) = (0usize, Vec::new());
        while off + 512 <= b.len() && b[off] != 0 {
            let name = String::from_utf8_lossy(&b[off..off + 100])
                .trim_end_matches('\0')
                .to_string();
            let size = usize::from_str_radix(
                String::from_utf8_lossy(&b[off + 124..off + 135])
                    .trim_matches(|c| c == '\0' || c == ' '),
                8,
            )
            .unwrap_or(0);
            names.push(name);
            off += 512 + size.div_ceil(512) * 512;
        }
        names
    }

    /// A file with an extended attribute must not travel as a `._` twin: the receiving
    /// Linux tar would write it into the working tree. (Only macOS can set the attribute.)
    #[test]
    fn tar_box_carries_no_appledouble_entries() {
        let tmp = tempdir("tarxattr");
        let src = tmp.join("box");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("a.txt"), "x").unwrap();
        let set = Command::new("xattr")
            .args(["-w", "com.apple.test", "1"])
            .arg(src.join("a.txt"))
            .output();
        if !set.map(|o| o.status.success()).unwrap_or(false) {
            return; // no xattr tool here: nothing to prove
        }
        let out = tmp.join("box.tar");
        tar_box(&src, &out).unwrap();
        let names = raw_tar_names(&out);
        assert!(names.iter().any(|n| n.ends_with("a.txt")), "{names:?}");
        assert!(!names.iter().any(|n| n.contains("._")), "{names:?}");
    }
}
