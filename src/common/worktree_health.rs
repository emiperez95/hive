//! Git state for one working tree — what a conversation has actually produced.
//!
//! The sidebar's stats pane answers "what did this cost"; this answers "what came
//! out of it". Together they are the two things you cannot see from a list of
//! session names: an idle conversation sitting on twelve uncommitted files is a
//! different object from an idle one with nothing to show, and the list renders
//! them identically.
//!
//! **Measured cost**: the full probe sequence is ~0.05–0.10s per working tree.
//! That is fine for one tree on the pane's 10s cadence, and far too slow to run
//! across every live conversation on the 1s gather — hence the cache below, which
//! is what makes a fleet-wide consumer (an attention-ordered cycle key) affordable
//! later.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::process::Command;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::Serialize;

/// Git state of one working tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WorktreeHealth {
    /// Absolute path of the working tree root.
    pub root: String,
    /// The branch actually checked out — **not** whatever a registry recorded.
    /// These diverge in practice: one registered `avateen/test-harness` here has
    /// `test/integration` checked out.
    pub branch: String,
    /// True when HEAD is detached, in which case `branch` is `"HEAD"`.
    pub detached: bool,
    /// The comparison branch, e.g. `origin/production`. `None` when nothing
    /// plausible exists, in which case ahead/behind/merged are meaningless and
    /// reported as zero/false.
    pub base: Option<String>,
    /// Tracked files with changes (staged or not).
    pub dirty: usize,
    /// Untracked files.
    pub untracked: usize,
    /// Commits on HEAD that `base` does not have.
    pub ahead: usize,
    /// Commits on `base` that HEAD does not have.
    pub behind: usize,
    /// HEAD is an ancestor of `base` — the work has landed.
    pub merged: bool,
    /// Unix seconds of the last commit, for an age display.
    pub last_commit_unix: Option<i64>,
}

impl WorktreeHealth {
    /// Whether this tree holds work that exists nowhere else yet.
    ///
    /// The whole point of the distinction the sidebar is being taught to draw:
    /// an idle conversation over unreviewed output is *waiting on you*, while an
    /// idle one over a clean, landed tree is simply done.
    pub fn has_unreviewed_work(&self) -> bool {
        self.dirty > 0 || self.untracked > 0 || self.ahead > 0
    }
}

// --- pure parsing helpers (the unit-testable core) --------------------------

/// `(dirty, untracked)` from `git status --porcelain`.
pub fn parse_status_counts(out: &str) -> (usize, usize) {
    let mut dirty = 0;
    let mut untracked = 0;
    for line in out.lines() {
        if line.is_empty() {
            continue;
        }
        if line.starts_with("??") {
            untracked += 1;
        } else {
            dirty += 1;
        }
    }
    (dirty, untracked)
}

/// `(behind, ahead)` from `git rev-list --left-right --count base...HEAD`.
///
/// Left is the base side (commits base has and HEAD does not) = behind; right is
/// HEAD's own = ahead. Getting this backwards silently inverts the display, so it
/// has its own test.
pub fn parse_ahead_behind(out: &str) -> (usize, usize) {
    let mut it = out.split_whitespace();
    let behind = it.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let ahead = it.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    (behind, ahead)
}

/// `origin/production` from `refs/remotes/origin/HEAD`.
pub fn base_from_symbolic_ref(out: &str) -> Option<String> {
    let t = out.trim();
    let rest = t.strip_prefix("refs/remotes/")?;
    (!rest.is_empty()).then(|| rest.to_string())
}

// --- the probe --------------------------------------------------------------

/// A `git` invocation that never takes a lock or writes.
///
/// `GIT_OPTIONAL_LOCKS=0` is load-bearing, not hygiene. `git status` refreshes stale
/// stat info by **rewriting the index**, and this cache keys on the index's mtime —
/// so the probe would invalidate its own entry on the next tick, in exactly the
/// worktrees where an agent is actively editing. Measured: after touching three files,
/// a plain `git status --porcelain` moved the index mtime (…096 → …099); with the flag
/// set it did not move at all.
///
/// It also keeps hive out of a fight over `index.lock` with the Claude running in that
/// same tree.
fn git_cmd(dir: &str, args: &[&str]) -> Command {
    let mut c = Command::new("git");
    c.env("GIT_OPTIONAL_LOCKS", "0")
        .arg("-C")
        .arg(dir)
        .args(args);
    c
}

fn git(dir: &str, args: &[&str]) -> Option<String> {
    let out = git_cmd(dir, args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn git_ok(dir: &str, args: &[&str]) -> bool {
    git_cmd(dir, args)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// The branch to compare against.
///
/// `origin/HEAD` first, because assuming `main` is wrong on real repos — avateen's
/// default is `origin/production`, so a hardcoded `origin/main` reports every
/// worktree there as zero-ahead, zero-behind and unmerged.
fn resolve_base(dir: &str) -> Option<String> {
    if let Some(b) = git(dir, &["symbolic-ref", "refs/remotes/origin/HEAD"])
        .as_deref()
        .and_then(base_from_symbolic_ref)
    {
        return Some(b);
    }
    ["origin/main", "origin/master", "main", "master"]
        .into_iter()
        .find(|r| git_ok(dir, &["rev-parse", "--verify", "--quiet", r]))
        .map(str::to_string)
}

fn probe(cwd: &str) -> Option<WorktreeHealth> {
    let root = git(cwd, &["rev-parse", "--show-toplevel"])?;
    if root.is_empty() {
        return None;
    }
    let branch = git(&root, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_default();
    let detached = branch == "HEAD";
    let (dirty, untracked) = git(&root, &["status", "--porcelain"])
        .map(|s| parse_status_counts(&s))
        .unwrap_or((0, 0));

    let base = resolve_base(&root);
    let (behind, ahead) = base
        .as_deref()
        .and_then(|b| {
            git(
                &root,
                &[
                    "rev-list",
                    "--left-right",
                    "--count",
                    &format!("{b}...HEAD"),
                ],
            )
        })
        .map(|s| parse_ahead_behind(&s))
        .unwrap_or((0, 0));
    let merged = base
        .as_deref()
        .map(|b| git_ok(&root, &["merge-base", "--is-ancestor", "HEAD", b]))
        .unwrap_or(false);

    let last_commit_unix =
        git(&root, &["log", "-1", "--format=%ct"]).and_then(|s| s.parse::<i64>().ok());

    Some(WorktreeHealth {
        root,
        branch,
        detached,
        base,
        dirty,
        untracked,
        ahead,
        behind,
        merged,
        last_commit_unix,
    })
}

// --- cache ------------------------------------------------------------------

/// Upper bound on staleness even when nothing local changed.
///
/// The mtime keys below catch local edits and commits, but ahead/behind also moves
/// when the *remote* does — a fetch rewrites refs under the git dir without
/// touching the index or HEAD. Rather than try to watch every ref file, the entry
/// simply expires.
const MAX_AGE: Duration = Duration::from_secs(60);

/// How far past [`MAX_AGE`] an entry's expiry can be pushed.
const TTL_SPREAD_SECS: u64 = 30;

/// Per-tree expiry, deterministically spread over `[MAX_AGE, MAX_AGE + TTL_SPREAD)`.
///
/// Every tree is probed for the first time on the same gather, so a flat ceiling
/// expires them all on the same later tick and replays the full cold cost (measured
/// 0.59s across 13 trees) as a recurring stall. Spreading by a hash of the path keeps
/// it deterministic — no RNG, no state — while making a synchronised expiry
/// impossible.
fn ttl_for(cwd: &str) -> Duration {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    cwd.hash(&mut h);
    MAX_AGE + Duration::from_secs(h.finish() % TTL_SPREAD_SECS)
}

struct Entry {
    /// Resolved once per working tree so the cache check itself costs no
    /// subprocess — only two stats.
    git_dir: Option<String>,
    index_mtime: Option<std::time::SystemTime>,
    head_mtime: Option<std::time::SystemTime>,
    at: Instant,
    health: Option<WorktreeHealth>,
}

fn cache() -> &'static Mutex<HashMap<String, Entry>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Entry>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn mtime(p: &Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()
}

/// `(index mtime, HEAD mtime)` for a git dir.
///
/// A linked worktree's `.git` is a *file*, and its index lives under
/// `<common>/worktrees/<name>/`, which is exactly what `--absolute-git-dir`
/// returns — so this works for a worktree and a main checkout alike.
fn git_dir_mtimes(git_dir: &str) -> (Option<std::time::SystemTime>, Option<std::time::SystemTime>) {
    let d = Path::new(git_dir);
    (mtime(&d.join("index")), mtime(&d.join("HEAD")))
}

/// Git state for the working tree containing `cwd`, or `None` when it is not in a
/// repository.
///
/// Cached in memory for the process's lifetime, keyed on the git dir's index and
/// HEAD mtimes with a [`MAX_AGE`] ceiling — the same shape as `machine.rs`'s
/// `pmset` memoization, and for the same reason: cheap enough once, too slow on a
/// loop.
pub fn health_for_cwd(cwd: &str) -> Option<WorktreeHealth> {
    let key = cwd.to_string();
    if let Ok(map) = cache().lock() {
        if let Some(e) = map.get(&key) {
            if e.at.elapsed() < ttl_for(cwd) {
                if let Some(gd) = &e.git_dir {
                    let (i, h) = git_dir_mtimes(gd);
                    if i == e.index_mtime && h == e.head_mtime {
                        return e.health.clone();
                    }
                } else {
                    // Known not to be a repo; nothing can change that cheaply.
                    return None;
                }
            }
        }
    }

    let health = probe(cwd);
    let git_dir = health
        .as_ref()
        .and_then(|h| git(&h.root, &["rev-parse", "--absolute-git-dir"]));
    let (index_mtime, head_mtime) = git_dir
        .as_deref()
        .map(git_dir_mtimes)
        .unwrap_or((None, None));

    if let Ok(mut map) = cache().lock() {
        map.insert(
            key,
            Entry {
                git_dir,
                index_mtime,
                head_mtime,
                at: Instant::now(),
                health: health.clone(),
            },
        );
    }
    health
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ttl_is_deterministic_and_spread() {
        let a = ttl_for("/a/one");
        assert_eq!(a, ttl_for("/a/one"), "same path must give the same expiry");
        assert!(a >= MAX_AGE && a < MAX_AGE + Duration::from_secs(TTL_SPREAD_SECS));

        // Not a guarantee for any specific pair, but across a realistic set the
        // expiries must not all land together — that is the whole point.
        let paths = [
            "/Users/x/Projects/hive",
            "/Users/x/Projects/worktrees/avateen/sos-avatar",
            "/Users/x/Projects/worktrees/avateen/live-avatar",
            "/Users/x/Projects/00-main",
            "/Users/x/Projects/02-promobile/media",
        ];
        let distinct: std::collections::HashSet<_> = paths.iter().map(|p| ttl_for(p)).collect();
        assert!(
            distinct.len() > 1,
            "a flat TTL would replay the full cold probe cost on one tick"
        );
    }

    #[test]
    fn status_counts_split_tracked_from_untracked() {
        let out = " M src/main.rs\nA  src/new.rs\n?? scratch.txt\n?? other.txt\nD  gone.rs\n";
        assert_eq!(parse_status_counts(out), (3, 2));
    }

    #[test]
    fn status_counts_of_a_clean_tree_are_zero() {
        assert_eq!(parse_status_counts(""), (0, 0));
        assert_eq!(parse_status_counts("\n"), (0, 0));
    }

    #[test]
    fn ahead_behind_reads_left_as_behind() {
        // `--left-right --count base...HEAD` prints "<base-only>\t<head-only>".
        assert_eq!(parse_ahead_behind("37\t25"), (37, 25));
        assert_eq!(parse_ahead_behind("0\t14"), (0, 14));
        assert_eq!(parse_ahead_behind(""), (0, 0));
    }

    #[test]
    fn base_comes_from_the_remote_head_not_an_assumption() {
        assert_eq!(
            base_from_symbolic_ref("refs/remotes/origin/production").as_deref(),
            Some("origin/production"),
            "a repo whose default is not main must not be read as main"
        );
        assert_eq!(
            base_from_symbolic_ref("refs/remotes/origin/main").as_deref(),
            Some("origin/main")
        );
        assert!(base_from_symbolic_ref("").is_none());
        assert!(base_from_symbolic_ref("refs/heads/main").is_none());
    }

    #[test]
    fn unreviewed_work_is_any_of_dirty_untracked_or_ahead() {
        let base = WorktreeHealth {
            root: "/x".into(),
            branch: "b".into(),
            detached: false,
            base: Some("origin/main".into()),
            dirty: 0,
            untracked: 0,
            ahead: 0,
            behind: 9,
            merged: true,
            last_commit_unix: None,
        };
        assert!(
            !base.has_unreviewed_work(),
            "clean and landed is done, not pending"
        );
        assert!(WorktreeHealth {
            dirty: 1,
            ..base.clone()
        }
        .has_unreviewed_work());
        assert!(WorktreeHealth {
            untracked: 1,
            ..base.clone()
        }
        .has_unreviewed_work());
        assert!(WorktreeHealth {
            ahead: 1,
            ..base.clone()
        }
        .has_unreviewed_work());
        assert!(
            !WorktreeHealth {
                behind: 100,
                ..base.clone()
            }
            .has_unreviewed_work(),
            "being behind is not work this conversation produced"
        );
    }
}
