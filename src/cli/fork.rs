//! `hive fork` — split a conversation in two, usually from inside it (`/hive:fork`).

use anyhow::{anyhow, bail, Result};

use crate::common::fork::{fork, ForkSource, ForkTarget};
use crate::common::projects::ProjectRegistry;
use crate::common::registry::resolve_parent;
use crate::common::worktree::WorktreeState;

/// Where the fork should go — the three ways to split.
pub struct ForkArgs {
    pub id: Option<String>,
    pub wt: Option<String>,
    pub base: Option<String>,
    pub to: Option<String>,
    pub switch: bool,
    pub directive: Option<String>,
}

pub fn run_fork(args: ForkArgs) -> Result<()> {
    let running = running_conversation_id();
    let id = args.id.clone().or_else(|| running.clone()).ok_or_else(|| {
        anyhow!("not inside a Claude conversation — pass --id <uuid> (or use `F` in the hive TUI)")
    })?;
    // Forking the conversation that is running this command: its last tool call (this
    // one) will look unfinished to the fork.
    let mid_turn = running.as_deref() == Some(id.as_str());
    let src = ForkSource::locate(&id)?;

    let dst = if let Some(branch) = &args.wt {
        new_worktree_target(&src, branch, args.base.as_deref())?
    } else if let Some(key) = &args.to {
        ForkTarget::registered(key)?
    } else {
        ForkTarget::beside(&src, beside_session(&src)?)
    };

    let forked = fork(&src, &dst, args.directive.as_deref(), mid_turn)?;
    println!("Forked {} → {}", short(&src.id), short(&forked.new_id));
    println!(
        "  running in '{}'{}",
        forked.session,
        forked
            .pane
            .as_deref()
            .map(|p| format!(" (pane {p})"))
            .unwrap_or_default()
    );
    if dst.cwd != src.cwd {
        println!("  at {}", dst.cwd);
    }
    println!("  the original conversation is unchanged");

    // Only from inside tmux, for the reason `wt new` gives: switch-client with no current
    // client acts on whichever one tmux guesses.
    if args.switch && std::env::var_os("TMUX").is_some() {
        if let Some(pane) = &forked.pane {
            let _ = std::process::Command::new("tmux")
                .args(["select-window", "-t", pane])
                .output();
        }
        crate::common::tmux::switch_to_session(&forked.session);
    }
    Ok(())
}

/// The conversation whose Bash tool is running us. `CLAUDE_PID`'s own session record
/// first — it follows `/clear` — then the id the shell inherited at spawn.
fn running_conversation_id() -> Option<String> {
    let from_record = std::env::var("CLAUDE_PID")
        .ok()
        .and_then(|p| p.parse().ok())
        .and_then(crate::common::claude_sessions::for_pid)
        .and_then(|s| s.session_id);
    from_record.or_else(|| {
        std::env::var("CLAUDE_CODE_SESSION_ID")
            .ok()
            .filter(|s| !s.is_empty())
    })
}

/// The session the source is in: our own pane's when run from tmux, else the one its
/// project / worktree is registered under.
fn beside_session(src: &ForkSource) -> Result<String> {
    if let Some(name) = std::env::var("TMUX_PANE")
        .ok()
        .and_then(|p| crate::common::tmux::display_message_for_pane(&p, "#{session_name}"))
    {
        return Ok(name);
    }
    let parent = resolve_parent(&src.cwd, &WorktreeState::load(), &ProjectRegistry::load())
        .ok_or_else(|| {
            anyhow!(
                "not in tmux, and {} belongs to no registered project",
                src.cwd
            )
        })?;
    Ok(ForkTarget::registered(&parent)?.session)
}

/// `hive wt new` a branch of the source's project — off the branch the source has
/// checked out, so the fork starts from the same commit — and fork into its shell.
fn new_worktree_target(src: &ForkSource, branch: &str, base: Option<&str>) -> Result<ForkTarget> {
    let parent = resolve_parent(&src.cwd, &WorktreeState::load(), &ProjectRegistry::load())
        .ok_or_else(|| {
            anyhow!(
                "{} belongs to no registered project — `hive project add` it first",
                src.cwd
            )
        })?;
    let project = parent.split('/').next().unwrap_or(&parent).to_string();
    let base = match base {
        Some(b) => b.to_string(),
        None => current_branch(&src.cwd)?,
    };
    let session = crate::cli::worktree::run_wt_new(
        &project,
        branch,
        Some(&base),
        false,
        "worktree",
        None,
        false,
        true,
    )?;
    let mut dst = ForkTarget::registered(&WorktreeState::make_key(&project, branch))?;
    dst.session = session;
    dst.fresh_session = true;
    Ok(dst)
}

fn current_branch(cwd: &str) -> Result<String> {
    let out = std::process::Command::new("git")
        .args(["-C", cwd, "branch", "--show-current"])
        .output()?;
    let b = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if !out.status.success() || b.is_empty() {
        bail!("{cwd} has no branch checked out (detached HEAD?) — pass --base");
    }
    Ok(b)
}

fn short(id: &str) -> &str {
    &id[..id.len().min(8)]
}
