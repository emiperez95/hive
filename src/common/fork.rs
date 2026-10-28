//! Split a conversation in two: `claude --resume <id> --fork-session` in another window.
//!
//! Claude's own `/branch` forks inside the running process and the current directory,
//! and `/fork` is a background agent, not a conversation you can switch to. Neither
//! puts the second half anywhere else. The CLI form does, and two measured facts make
//! it the whole mechanism:
//!
//! - **The lookup is by id across the profile, not by cwd.** `--resume <id>` run from a
//!   different directory finds the transcript, and the fork's own transcript is written
//!   under the NEW cwd's slug — so a fork into a worktree is filed under that worktree by
//!   `resolve_parent` with no extra work. Nothing needs copying within a profile.
//! - **It is profile-scoped.** The same command under another `CLAUDE_CONFIG_DIR` says
//!   "No conversation found". Crossing profiles therefore copies the transcript into the
//!   target profile first ([`ForkPlan::copy_to`]).
//!
//! A conversation can fork **itself, mid-turn**: the `hive fork` Bash call is still
//! running when the fork reads the transcript, so the fork sees that call with no
//! result. The opening message ([`orientation`]) says it succeeded, or the fork would
//! reasonably run it again.
//!
//! `--session-id <new>` alongside `--fork-session` is honoured, so the fork's id is
//! chosen here rather than discovered afterwards. That is what lets the lineage
//! (`forked_from`) be written before Claude has started, and what lets
//! `process::parse_resume_id` attribute the window to the fork instead of its source.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, Result};

use crate::common::projects::{ensure_tmux_session, ProjectRegistry};
use crate::common::registry::{resolve_parent, ConversationSidecar};
use crate::common::tmux::{exact, exact_active_pane, sh_quote};
use crate::common::worktree::WorktreeState;

/// The conversation being forked, as found on disk.
#[derive(Debug, Clone)]
pub struct ForkSource {
    pub id: String,
    /// Launch dir — the transcript's first `cwd`.
    pub cwd: String,
    pub transcript: PathBuf,
}

impl ForkSource {
    /// Find a conversation's transcript under any profile.
    pub fn locate(id: &str) -> Result<Self> {
        let transcript =
            crate::common::jsonl::find_jsonl_by_session_id_anywhere(id).ok_or_else(|| {
                anyhow!("no transcript for conversation {id} under ~/.claude*/projects")
            })?;
        let cwd = crate::common::jsonl::read_cwd_from_jsonl(&transcript)
            .ok_or_else(|| anyhow!("transcript {} records no cwd", transcript.display()))?;
        Ok(ForkSource {
            id: id.to_string(),
            cwd,
            transcript,
        })
    }

    /// The profile root (`~/.claude`, `~/.claude-work`, …) the transcript lives under:
    /// `<profile>/projects/<slug>/<id>.jsonl`.
    pub fn profile_dir(&self) -> Option<PathBuf> {
        profile_of(&self.transcript)
    }
}

fn profile_of(transcript: &Path) -> Option<PathBuf> {
    transcript
        .parent()?
        .parent()?
        .parent()
        .map(Path::to_path_buf)
}

/// Where the fork goes.
#[derive(Debug, Clone)]
pub struct ForkTarget {
    pub session: String,
    pub cwd: String,
    /// `CLAUDE_CONFIG_DIR` for the target, when it isn't the default profile.
    pub env: Vec<(String, String)>,
    /// The session was just created with a bare shell in it (`hive wt new --no-startup`):
    /// type into that shell rather than opening a second window beside it.
    pub fresh_session: bool,
    /// Project / worktree key, to un-archive — starting work there.
    pub parent: Option<String>,
}

impl ForkTarget {
    /// A new window next to the source, in the source's own profile.
    pub fn beside(src: &ForkSource, session: String) -> Self {
        let env = src
            .profile_dir()
            .filter(|p| Some(p.as_path()) != default_profile().as_deref())
            .map(|p| {
                vec![(
                    "CLAUDE_CONFIG_DIR".to_string(),
                    p.to_string_lossy().into_owned(),
                )]
            })
            .unwrap_or_default();
        let parent = resolve_parent(&src.cwd, &WorktreeState::load(), &ProjectRegistry::load());
        ForkTarget {
            session,
            cwd: src.cwd.clone(),
            env,
            fresh_session: false,
            parent,
        }
    }

    /// A registered project (`key`) or worktree (`project/branch`), under its
    /// project's auth profile.
    pub fn registered(key: &str) -> Result<Self> {
        let projects = ProjectRegistry::load();
        let (session, cwd, project) = if key.contains('/') {
            let wts = WorktreeState::load();
            let e = wts
                .worktrees
                .get(key)
                .ok_or_else(|| anyhow!("no registered worktree '{key}' (see `hive wt list`)"))?;
            (
                e.session_name.clone(),
                e.path.clone(),
                e.project_key.clone(),
            )
        } else {
            let config = projects.projects.get(key).ok_or_else(|| {
                anyhow!("no registered project '{key}' (see `hive project list`)")
            })?;
            let root = crate::common::projects::expand_tilde(&config.project_root);
            (
                ProjectRegistry::session_name(key, config),
                root.to_string_lossy().into_owned(),
                key.to_string(),
            )
        };
        let env = projects
            .projects
            .get(&project)
            .map(|c| c.tmux_env())
            .unwrap_or_default();
        Ok(ForkTarget {
            session,
            cwd,
            env,
            fresh_session: false,
            parent: Some(key.to_string()),
        })
    }

    /// The profile the fork will run under.
    fn profile_dir(&self) -> Option<PathBuf> {
        self.env
            .iter()
            .find(|(k, _)| k == "CLAUDE_CONFIG_DIR")
            .map(|(_, v)| PathBuf::from(v))
            .or_else(default_profile)
    }
}

fn default_profile() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".claude"))
}

/// Everything decided before anything runs — pure, so it is testable.
#[derive(Debug, Clone, PartialEq)]
pub struct ForkPlan {
    pub new_id: String,
    /// The command typed into the target pane.
    pub command: String,
    /// Copy the source transcript here first: the target runs under another profile,
    /// and `--resume` only searches its own.
    pub copy_to: Option<PathBuf>,
}

/// Build the plan. `mid_turn`: the source is the conversation running this (a slash
/// command), so its last tool call will look unfinished to the fork.
pub fn plan(
    src: &ForkSource,
    dst: &ForkTarget,
    new_id: &str,
    directive: Option<&str>,
    mid_turn: bool,
) -> ForkPlan {
    let prompt = orientation(&src.id, &src.cwd, &dst.cwd, directive, mid_turn);
    let command = format!(
        "claude --resume {} --fork-session --session-id {} {}",
        src.id,
        new_id,
        sh_quote(&prompt)
    );
    let copy_to = match (src.profile_dir(), dst.profile_dir()) {
        (Some(from), Some(to)) if from != to => {
            let slug = src.transcript.parent().and_then(|p| p.file_name());
            slug.map(|s| {
                to.join("projects")
                    .join(s)
                    .join(format!("{}.jsonl", src.id))
            })
        }
        _ => None,
    };
    ForkPlan {
        new_id: new_id.to_string(),
        command,
        copy_to,
    }
}

/// The fork's first message: what it is, where it now runs, what to do.
///
/// Without it the fork wakes up mid-history believing it is the original — and, after
/// a self-fork, looking at a `hive fork` call with no result that it may well retry.
pub fn orientation(
    src_id: &str,
    src_cwd: &str,
    dst_cwd: &str,
    directive: Option<&str>,
    mid_turn: bool,
) -> String {
    let short = &src_id[..src_id.len().min(8)];
    let mut s = format!(
        "[hive fork] You are a fork of conversation {short}: everything above is shared \
         history, and the original conversation carries on separately in its own window."
    );
    if mid_turn {
        s.push_str(
            " The `hive fork` call that looks unfinished above is what created you — it \
             succeeded; do not run it again.",
        );
    }
    if src_cwd != dst_cwd {
        s.push_str(&format!(
            " You now run in {dst_cwd}, not {src_cwd}. File paths in the history point at \
             the old checkout: read and edit only under {dst_cwd}. Uncommitted changes from \
             the old checkout are not here."
        ));
    }
    // One line, directive included: the command is typed into a shell by `send-keys`,
    // where every newline is an Enter — inside the quotes zsh just keeps reading
    // (`quote>`), but nothing should depend on that.
    let directive = directive
        .map(|d| d.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|d| !d.is_empty());
    match directive {
        Some(d) => s.push_str(&format!(" Your task in this fork: {d}")),
        None => s.push_str(
            " No task was given for this fork yet: acknowledge in one line and wait for \
             instructions.",
        ),
    }
    s
}

/// What a fork produced.
#[derive(Debug, Clone)]
pub struct Forked {
    pub new_id: String,
    pub session: String,
    /// The pane the fork runs in, when tmux reported it — the precise switch target.
    pub pane: Option<String>,
}

/// Fork `src` into `dst`. Does NOT switch — the CLI, the TUI and a slash command each
/// want something different there.
pub fn fork(
    src: &ForkSource,
    dst: &ForkTarget,
    directive: Option<&str>,
    mid_turn: bool,
) -> Result<Forked> {
    let new_id = new_uuid()?;
    let p = plan(src, dst, &new_id, directive, mid_turn);

    if let Some(to) = &p.copy_to {
        if !to.exists() {
            if let Some(dir) = to.parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::copy(&src.transcript, to)?;
        }
    }
    if let Some(parent) = &dst.parent {
        crate::common::projects::activate_project(parent);
    }
    // Recorded before launch: the id is ours, so there is nothing to wait for.
    let _ = ConversationSidecar::edit(&new_id, |o| o.forked_from = Some(src.id.clone()));

    let pane = launch(dst, &p.command)?;
    Ok(Forked {
        new_id,
        session: dst.session.clone(),
        pane,
    })
}

/// Type `command` into the target: the fresh session's own shell, a new window of a
/// live session, or a session (re)created for it.
fn launch(dst: &ForkTarget, command: &str) -> Result<Option<String>> {
    let target = exact(&dst.session);
    let alive = Command::new("tmux")
        .args(["has-session", "-t", &target])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);

    if !alive {
        if !ensure_tmux_session(&dst.session, &dst.cwd, Some(command), &dst.env) {
            return Err(anyhow!("failed to create session '{}'", dst.session));
        }
        return Ok(None);
    }

    let pane = if dst.fresh_session {
        exact_active_pane(&dst.session)
    } else {
        // `-d` + the reported pane id, not "the session's current window": the source
        // is usually sitting in this very session, and the window must not be taken
        // from under it (nor the keys typed into it).
        let mut cmd = Command::new("tmux");
        cmd.args(["new-window", "-d", "-P", "-F", "#{pane_id}", "-t", &target]);
        cmd.args(["-c", &dst.cwd]);
        for (k, v) in &dst.env {
            cmd.arg("-e").arg(format!("{k}={v}"));
        }
        let out = cmd.output()?;
        let id = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !out.status.success() || id.is_empty() {
            return Err(anyhow!("failed to open a new window in '{}'", dst.session));
        }
        id
    };
    let ok = Command::new("tmux")
        .args(["send-keys", "-t", &pane, command, "Enter"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !ok {
        return Err(anyhow!("couldn't type the fork command into {pane}"));
    }
    Ok(pane.starts_with('%').then_some(pane))
}

/// A random (v4) UUID from the OS — the one place hive mints a conversation id.
fn new_uuid() -> Result<String> {
    use std::io::Read;
    let mut b = [0u8; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut b)?;
    Ok(format_uuid_v4(b))
}

fn format_uuid_v4(mut b: [u8; 16]) -> String {
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRC: &str = "41ba8d5c-93d7-4545-9b49-784a401ec8d6";
    const NEW: &str = "38139292-442c-47ff-8169-51a81aae73fa";

    fn src(profile: &str) -> ForkSource {
        ForkSource {
            id: SRC.into(),
            cwd: "/p/hive".into(),
            transcript: PathBuf::from(format!("/h/{profile}/projects/-p-hive/{SRC}.jsonl")),
        }
    }

    fn dst(cwd: &str, config_dir: Option<&str>) -> ForkTarget {
        ForkTarget {
            session: "🐝 hive".into(),
            cwd: cwd.into(),
            env: config_dir
                .map(|d| vec![("CLAUDE_CONFIG_DIR".to_string(), d.to_string())])
                .unwrap_or_default(),
            fresh_session: false,
            parent: None,
        }
    }

    #[test]
    fn the_command_resumes_the_source_under_the_chosen_id() {
        let p = plan(
            &src(".claude-work"),
            &dst("/p/hive", Some("/h/.claude-work")),
            NEW,
            None,
            false,
        );
        assert!(p.command.starts_with(&format!(
            "claude --resume {SRC} --fork-session --session-id {NEW} '"
        )));
        // The id we chose is the id `parse_resume_id` will attribute the window to.
        assert_eq!(
            crate::common::process::parse_resume_id(&p.command).as_deref(),
            Some(NEW)
        );
    }

    #[test]
    fn same_profile_copies_nothing_even_across_directories() {
        let p = plan(
            &src(".claude-work"),
            &dst("/wt/hive/feature", Some("/h/.claude-work")),
            NEW,
            None,
            false,
        );
        assert_eq!(p.copy_to, None);
    }

    #[test]
    fn another_profile_gets_the_transcript_under_the_same_slug() {
        let p = plan(
            &src(".claude"),
            &dst("/p/hive", Some("/h/.claude-work")),
            NEW,
            None,
            false,
        );
        assert_eq!(
            p.copy_to,
            Some(PathBuf::from(format!(
                "/h/.claude-work/projects/-p-hive/{SRC}.jsonl"
            )))
        );
    }

    #[test]
    fn the_prompt_is_one_shell_word_whatever_the_directive_holds() {
        let p = plan(
            &src(".claude"),
            &dst("/p/hive", None),
            NEW,
            Some("don't run $(rm -rf ~) or `x`"),
            true,
        );
        let quoted = p.command.splitn(7, ' ').nth(6).unwrap();
        assert!(quoted.starts_with('\'') && quoted.ends_with('\''));
    }

    #[test]
    fn orientation_names_the_move_only_when_there_is_one() {
        let here = orientation(SRC, "/p/hive", "/p/hive", None, false);
        assert!(here.contains("fork of conversation 41ba8d5c"));
        assert!(!here.contains("You now run in"));

        let moved = orientation(SRC, "/p/hive", "/wt/x", None, false);
        assert!(moved.contains("You now run in /wt/x, not /p/hive"));
        assert!(moved.contains("Uncommitted changes"));
    }

    #[test]
    fn a_self_fork_is_told_its_unfinished_call_succeeded() {
        assert!(orientation(SRC, "/a", "/a", None, true).contains("do not run it again"));
        assert!(!orientation(SRC, "/a", "/a", None, false).contains("do not run it again"));
    }

    #[test]
    fn orientation_carries_the_directive_or_asks_to_wait() {
        assert!(
            orientation(SRC, "/a", "/a", Some("  write the tests "), false)
                .ends_with("Your task in this fork: write the tests")
        );
        assert!(orientation(SRC, "/a", "/a", Some("   "), false).contains("wait for instructions"));
    }

    #[test]
    fn the_prompt_has_no_newline_for_the_shell_to_read_as_enter() {
        let o = orientation(SRC, "/a", "/b", Some("line one\n\nline two"), true);
        assert!(!o.contains('\n'));
        assert!(o.ends_with("Your task in this fork: line one line two"));
    }

    #[test]
    fn minted_ids_are_v4_uuids() {
        let id = format_uuid_v4([0xff; 16]);
        assert_eq!(id, "ffffffff-ffff-4fff-bfff-ffffffffffff");
        assert_eq!(
            crate::common::process::parse_resume_id(&format!("claude --resume {id}")),
            Some(id)
        );
    }
}
