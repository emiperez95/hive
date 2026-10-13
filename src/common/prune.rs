//! Leftovers that Claude's background Workflows leave in a repo, and the rules for
//! deciding which are safe to remove.
//!
//! A Workflow gives each agent its own git worktree under
//! `<checkout>/.claude/worktrees/wf_*`, on a branch named `worktree-wf_*`. The harness
//! removes the folder when an agent finishes cleanly, but never the branch — and an
//! agent that ran `git checkout -b` inside its worktree leaves that branch too.
//! Measured on avateen after two months: 177 `worktree-wf_*` branches (173 merged)
//! and 3 worktree folders holding 2.7 GB.
//!
//! None of this is hive's: hive's own `wt delete` removes its worktree and branch.
//! But hive knows every project root, so it is the one place that can sweep them all.
//!
//! The rule is **nothing that exists only here is removed**. A worktree goes only
//! when it is clean, every commit it holds is on a remote, and git hasn't touched it
//! for a while (a running workflow's tree is clean and pushed too — idleness is what
//! tells the two apart). A branch goes only when no worktree has it checked out and
//! every commit on it is on a remote.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

/// `worktree-wf_*` is what the Workflow harness names its agent worktrees' branches;
/// `wf/*` is the namespace workflow agents are told to use when they truly need a
/// local branch of their own (global CLAUDE.md), so those are swept by default too.
pub const DEFAULT_BRANCH_PATTERNS: &[&str] = &["worktree-wf_*", "wf/*"];

/// One entry of `git worktree list --porcelain`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitWorktree {
    pub path: String,
    /// Short branch name; `None` when detached.
    pub branch: Option<String>,
    pub locked: bool,
    /// git already knows the folder is gone; `git worktree prune` drops the record.
    pub prunable: bool,
}

/// Parse `git worktree list --porcelain`: blank-line separated records of
/// `key value` lines, with `locked` / `prunable` / `detached` as bare or valued flags.
pub fn parse_worktree_porcelain(out: &str) -> Vec<GitWorktree> {
    let mut all = Vec::new();
    let mut cur: Option<GitWorktree> = None;
    for line in out.lines() {
        let (key, val) = line.split_once(' ').unwrap_or((line, ""));
        match key {
            "worktree" => {
                all.extend(cur.take());
                cur = Some(GitWorktree {
                    path: val.to_string(),
                    branch: None,
                    locked: false,
                    prunable: false,
                });
            }
            "branch" => {
                if let Some(w) = cur.as_mut() {
                    w.branch = Some(val.strip_prefix("refs/heads/").unwrap_or(val).to_string());
                }
            }
            "locked" => cur.iter_mut().for_each(|w| w.locked = true),
            "prunable" => cur.iter_mut().for_each(|w| w.prunable = true),
            _ => {}
        }
    }
    all.extend(cur);
    all
}

/// A worktree the Workflow harness created — the only kind prune ever removes.
/// Hive's own worktrees are left to `hive wt delete`, which runs their hooks.
pub fn is_agent_worktree(path: &str) -> bool {
    path.contains("/.claude/worktrees/")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Remove,
    Keep(String),
}

/// What the worktree holds, gathered from git.
#[derive(Debug, Clone, Default)]
pub struct WorktreeFacts {
    pub locked: bool,
    /// Tracked changes or untracked files (ignored files don't count).
    pub dirty: bool,
    /// Commits on HEAD that no remote has; `None` when git couldn't say.
    pub unpushed: Option<usize>,
    /// Time since git last touched this worktree; `None` when it can't be read,
    /// which is treated as recent.
    pub idle: Option<Duration>,
}

pub fn worktree_verdict(f: &WorktreeFacts, min_idle: Duration) -> Verdict {
    if f.locked {
        return Verdict::Keep("locked".into());
    }
    if f.dirty {
        return Verdict::Keep("uncommitted changes".into());
    }
    if let Some(reason) = unpushed_reason(f.unpushed) {
        return Verdict::Keep(reason);
    }
    match f.idle {
        Some(idle) if idle >= min_idle => Verdict::Remove,
        Some(idle) => Verdict::Keep(format!("used {} ago — may be running", human(idle))),
        None => Verdict::Keep("can't tell when it was last used".into()),
    }
}

/// A branch is removable when nothing has it checked out and no commit on it is
/// missing from every remote — then deleting it loses nothing that can't be fetched.
pub fn branch_verdict(checked_out: bool, unpushed: Option<usize>) -> Verdict {
    if checked_out {
        return Verdict::Keep("checked out in a worktree".into());
    }
    match unpushed_reason(unpushed) {
        Some(reason) => Verdict::Keep(reason),
        None => Verdict::Remove,
    }
}

/// Unreadable counts as having unpushed work — the safe direction.
fn unpushed_reason(unpushed: Option<usize>) -> Option<String> {
    match unpushed {
        Some(0) => None,
        Some(n) => Some(format!("{} commit(s) not on any remote", n)),
        None => Some("can't tell whether its commits are pushed".into()),
    }
}

pub fn human(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86_400),
    }
}

// ── git ─────────────────────────────────────────────────────────────────────

/// `GIT_OPTIONAL_LOCKS=0` so inspecting a tree doesn't refresh its index — that
/// would reset the very idle clock this module reads, and contend with an agent
/// working there. Same reason as `worktree_health::git_cmd`.
fn git(dir: &str, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim_end().to_string())
}

/// Run a mutating git command; the error carries git's own message.
pub fn git_run(dir: &str, args: &[&str]) -> anyhow::Result<()> {
    let out = Command::new("git").arg("-C").arg(dir).args(args).output()?;
    if out.status.success() {
        Ok(())
    } else {
        anyhow::bail!("{}", String::from_utf8_lossy(&out.stderr).trim())
    }
}

/// The repo's shared git dir — two project roots in one repo resolve to the same one.
pub fn common_dir(dir: &str) -> Option<PathBuf> {
    let p = git(
        dir,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    std::fs::canonicalize(p).ok()
}

pub fn list_worktrees(repo: &str) -> Vec<GitWorktree> {
    git(repo, &["worktree", "list", "--porcelain"])
        .map(|o| parse_worktree_porcelain(&o))
        .unwrap_or_default()
}

/// Commits reachable from `rev` that no remote-tracking ref contains.
fn unpushed(dir: &str, rev: &str) -> Option<usize> {
    git(dir, &["rev-list", "--count", rev, "--not", "--remotes"])?
        .trim()
        .parse()
        .ok()
}

/// Newest mtime among the files git rewrites on every operation in this worktree.
/// Its admin dir (`.git/worktrees/<name>`), not the folder, so editing files
/// without committing doesn't look idle — that shows up as `dirty` instead.
fn last_git_activity(path: &str) -> Option<SystemTime> {
    let admin = git(path, &["rev-parse", "--absolute-git-dir"])?;
    ["HEAD", "index", "logs/HEAD"]
        .iter()
        .filter_map(|f| {
            std::fs::metadata(Path::new(&admin).join(f))
                .ok()?
                .modified()
                .ok()
        })
        .max()
}

pub fn worktree_facts(w: &GitWorktree, now: SystemTime) -> WorktreeFacts {
    let dirty = git(&w.path, &["status", "--porcelain"]).is_none_or(|s| !s.trim().is_empty());
    WorktreeFacts {
        locked: w.locked,
        dirty,
        unpushed: unpushed(&w.path, "HEAD"),
        idle: last_git_activity(&w.path).and_then(|t| now.duration_since(t).ok()),
    }
}

/// Local branches matching any of `patterns` (git's own ref globbing).
pub fn matching_branches(repo: &str, patterns: &[String]) -> Vec<String> {
    let refs: Vec<String> = patterns
        .iter()
        .map(|p| format!("refs/heads/{}", p))
        .collect();
    let mut args = vec!["for-each-ref", "--format=%(refname:short)"];
    args.extend(refs.iter().map(String::as_str));
    git(repo, &args)
        .map(|o| {
            o.lines()
                .map(str::to_string)
                .filter(|l| !l.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

pub fn branch_unpushed(repo: &str, branch: &str) -> Option<usize> {
    unpushed(repo, &format!("refs/heads/{}", branch))
}

#[cfg(test)]
mod tests {
    use super::*;

    const PORCELAIN: &str = "worktree /u/avateen
HEAD 417c656f
branch refs/heads/production

worktree /u/avateen/.claude/worktrees/wf_3b950d55-4e8-2
HEAD 495bfca5
branch refs/heads/worktree-wf_3b950d55-4e8-2

worktree /u/avateen/.claude/worktrees/wf_d17756a6-2c5-1
HEAD 7cb2baf2
detached
locked agent running

worktree /u/gone
HEAD 1234
branch refs/heads/feat/x
prunable gitdir file points to non-existent location
";

    #[test]
    fn porcelain_records_carry_branch_and_flags() {
        let w = parse_worktree_porcelain(PORCELAIN);
        assert_eq!(w.len(), 4);
        assert_eq!(w[0].branch.as_deref(), Some("production"));
        assert_eq!(w[1].branch.as_deref(), Some("worktree-wf_3b950d55-4e8-2"));
        assert_eq!(w[2].branch, None, "detached has no branch");
        assert!(w[2].locked, "a valued `locked <reason>` still locks");
        assert!(w[3].prunable && !w[3].locked);
        assert_eq!(w[3].branch.as_deref(), Some("feat/x"), "slashes survive");
    }

    #[test]
    fn only_harness_worktrees_are_agent_worktrees() {
        assert!(is_agent_worktree("/u/avateen/.claude/worktrees/wf_1-2"));
        // A hive worktree — removed by `hive wt delete`, which runs its hooks.
        assert!(!is_agent_worktree("/u/worktrees/avateen/live-avatar"));
        assert!(!is_agent_worktree("/u/avateen"));
    }

    fn idle_clean(hours: u64) -> WorktreeFacts {
        WorktreeFacts {
            unpushed: Some(0),
            idle: Some(Duration::from_secs(hours * 3600)),
            ..Default::default()
        }
    }

    #[test]
    fn a_clean_pushed_idle_worktree_is_removed() {
        let day = Duration::from_secs(86_400);
        assert_eq!(worktree_verdict(&idle_clean(48), day), Verdict::Remove);
    }

    #[test]
    fn anything_that_exists_only_here_keeps_the_worktree() {
        let day = Duration::from_secs(86_400);
        // The avateen case: three month-old agent trees with staged, uncommitted work.
        let dirty = WorktreeFacts {
            dirty: true,
            ..idle_clean(700)
        };
        assert!(
            matches!(worktree_verdict(&dirty, day), Verdict::Keep(r) if r.contains("uncommitted"))
        );
        let ahead = WorktreeFacts {
            unpushed: Some(2),
            ..idle_clean(700)
        };
        assert!(
            matches!(worktree_verdict(&ahead, day), Verdict::Keep(r) if r.contains("2 commit"))
        );
        let unread = WorktreeFacts {
            unpushed: None,
            ..idle_clean(700)
        };
        assert!(matches!(worktree_verdict(&unread, day), Verdict::Keep(_)));
        let locked = WorktreeFacts {
            locked: true,
            ..idle_clean(700)
        };
        assert_eq!(
            worktree_verdict(&locked, day),
            Verdict::Keep("locked".into())
        );
    }

    #[test]
    fn a_recent_or_unreadable_worktree_may_be_running() {
        let day = Duration::from_secs(86_400);
        // A running reviewer's tree is clean and pushed too; only idleness separates them.
        assert!(
            matches!(worktree_verdict(&idle_clean(2), day), Verdict::Keep(r) if r.contains("2h ago"))
        );
        let unknown = WorktreeFacts {
            idle: None,
            ..idle_clean(0)
        };
        assert!(matches!(worktree_verdict(&unknown, day), Verdict::Keep(_)));
    }

    #[test]
    fn a_branch_goes_only_when_free_and_fully_on_a_remote() {
        assert_eq!(branch_verdict(false, Some(0)), Verdict::Remove);
        assert!(
            matches!(branch_verdict(true, Some(0)), Verdict::Keep(r) if r.contains("checked out"))
        );
        assert!(
            matches!(branch_verdict(false, Some(3)), Verdict::Keep(r) if r.contains("3 commit"))
        );
        assert!(
            matches!(branch_verdict(false, None), Verdict::Keep(_)),
            "unreadable is kept"
        );
    }

    #[test]
    fn human_durations() {
        assert_eq!(human(Duration::from_secs(300)), "5m");
        assert_eq!(human(Duration::from_secs(7200)), "2h");
        assert_eq!(human(Duration::from_secs(3 * 86_400)), "3d");
    }
}
