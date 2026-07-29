//! hive: Interactive Claude Code session dashboard for tmux.

mod cli;
mod common;
mod daemon;
mod ipc;
mod serve;

use anyhow::{bail, Result};
use clap::Parser;

use crate::cli::conversations::ConvOptions;
use crate::cli::{Args, Command, ProjectCommand};
use crate::common::debug::init_debug;
use crate::common::tmux::resolve_tmux_path;

/// Build conversation-TUI options from the shared global CLI flags.
fn conv_opts(args: &Args, list: bool) -> ConvOptions {
    ConvOptions {
        list,
        detail: args.detail,
        project_detail: args.project_detail,
        picker: args.picker,
        filter: args.filter.clone(),
    }
}

fn main() -> Result<()> {
    let args = Args::parse();
    init_debug(args.debug);

    match args.command {
        Some(Command::Hook { event }) => cli::hook::run_hook(&event),
        Some(Command::Setup { yes }) => cli::setup::run_setup(yes),
        Some(Command::Update) => cli::update::run_update(),
        Some(Command::Uninstall { yes }) => cli::setup::run_uninstall(yes),
        Some(Command::CycleNext { pane }) => cli::session::run_cycle(true, pane.as_deref()),
        Some(Command::CyclePrev { pane }) => cli::session::run_cycle(false, pane.as_deref()),
        Some(Command::CycleFree { pane }) => cli::session::run_cycle_free(pane.as_deref()),
        Some(Command::WindowNext { pane }) => cli::session::run_window_cycle(true, pane.as_deref()),
        Some(Command::WindowPrev { pane }) => {
            cli::session::run_window_cycle(false, pane.as_deref())
        }
        Some(Command::Connect { key }) => cli::session::run_connect(&key),
        Some(Command::Project { command }) => match *command {
            cmd @ ProjectCommand::Add { .. } => cli::project::run_project_add(cmd),
            ProjectCommand::Remove { key } => cli::project::run_project_remove(&key),
            ProjectCommand::Archive { key } => cli::project::run_project_set_archived(&key, true),
            ProjectCommand::Unarchive { key } => {
                cli::project::run_project_set_archived(&key, false)
            }
            ProjectCommand::List { all } => cli::project::run_project_list(all),
        },
        Some(Command::Todo { command }) => cli::todo::run_todo(command),
        Some(Command::Spread { count }) => cli::session::run_spread(count),
        Some(Command::Collapse) => cli::session::run_collapse(),
        Some(Command::Stats { days }) => cli::stats::run_stats(days),
        Some(Command::Conversations { list }) => {
            cli::conversations::run_conversations(conv_opts(&args, list))
        }
        Some(Command::Event {
            kind,
            session,
            window,
        }) => cli::session::run_event(&kind, session.as_deref(), window.as_deref()),
        Some(Command::Web {
            port,
            dev,
            tts_host,
        }) => crate::serve::web::run_web_server(port, dev, tts_host),
        Some(Command::Start) => {
            if let Some(target) = cli::session::run_start()? {
                use std::os::unix::process::CommandExt;
                let tmux = resolve_tmux_path();
                let err = std::process::Command::new(&tmux)
                    .args(["attach-session", "-t", &target])
                    .exec();
                bail!("exec failed: {}", err);
            }
            // No available session — open the conversations picker (Browse + search).
            // A `hive start` invocation is outside tmux, so selecting/creating a
            // session there exec's `tmux attach` (see conversations::attach_or_switch).
            cli::conversations::run_conversations(ConvOptions {
                picker: true,
                ..Default::default()
            })
        }
        Some(Command::Wt { command }) => {
            use crate::cli::WtCommand;
            match command {
                WtCommand::New {
                    project,
                    branch,
                    base,
                    existing,
                    wt_type,
                    prompt,
                    no_startup,
                    auto_approve,
                } => cli::worktree::run_wt_new(
                    &project,
                    &branch,
                    base.as_deref(),
                    existing,
                    &wt_type,
                    prompt.as_deref(),
                    auto_approve,
                    no_startup,
                ),
                WtCommand::Delete {
                    project,
                    branch,
                    keep_branch,
                    force,
                } => cli::worktree::run_wt_delete(&project, &branch, keep_branch, force),
                WtCommand::List { project } => cli::worktree::run_wt_list(project.as_deref()),
                WtCommand::Import { project } => cli::worktree::run_wt_import(&project),
            }
        }
        // Default view is now the conversation-first TUI. The classic session-first
        // TUI stays available via `hive classic` during the migration.
        Some(Command::Tui) | None => cli::conversations::run_conversations(conv_opts(&args, false)),
    }
}
