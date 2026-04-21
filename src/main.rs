//! hive: Interactive Claude Code session dashboard for tmux.

mod cli;
mod common;
mod daemon;
mod ipc;
mod serve;
mod tui;

use anyhow::{bail, Result};
use clap::Parser;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::cli::{Args, Command, ProjectCommand};
use crate::common::debug::init_debug;
use crate::common::tmux::resolve_tmux_path;
use crate::tui::event_loop::{handle_post_action, run_tui};

fn main() -> Result<()> {
    let mut args = Args::parse();
    init_debug(args.debug);

    match args.command {
        Some(Command::Hook { event }) => cli::hook::run_hook(&event),
        Some(Command::Setup) => cli::setup::run_setup(),
        Some(Command::Update) => cli::update::run_update(),
        Some(Command::Uninstall) => cli::setup::run_uninstall(),
        Some(Command::CycleNext) => cli::session::run_cycle(true),
        Some(Command::CyclePrev) => cli::session::run_cycle(false),
        Some(Command::Connect { key }) => cli::session::run_connect(&key),
        Some(Command::Project { command }) => match *command {
            cmd @ ProjectCommand::Add { .. } => cli::project::run_project_add(cmd),
            ProjectCommand::Remove { key } => cli::project::run_project_remove(&key),
            ProjectCommand::List => cli::project::run_project_list(),
            ProjectCommand::Import => cli::project::run_project_import(),
        },
        Some(Command::Todo { command }) => cli::todo::run_todo(command),
        Some(Command::Spread { count }) => cli::session::run_spread(count),
        Some(Command::Collapse) => cli::session::run_collapse(),
        Some(Command::Web { port, dev, tts_host }) => {
            crate::serve::web::run_web_server(port, dev, tts_host)
        }
        Some(Command::Start) => {
            if let Some(target) = cli::session::run_start()? {
                use std::os::unix::process::CommandExt;
                let tmux = resolve_tmux_path();
                let err = std::process::Command::new(&tmux)
                    .args(["attach-session", "-t", &target])
                    .exec();
                bail!("exec failed: {}", err);
            }
            // No available session — fall through to TUI picker
            args.picker = true;
            args.command = None;
            // fall through below
            let running = Arc::new(AtomicBool::new(true));
            let r = running.clone();
            ctrlc::set_handler(move || {
                r.store(false, Ordering::SeqCst);
            })
            .expect("Error setting Ctrl-C handler");

            let mut terminal = ratatui::init();
            let action = run_tui(&mut terminal, &args, running);
            ratatui::restore();
            handle_post_action(action?)
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
        Some(Command::Tui) | None => {
            // Set up signal handler for graceful shutdown
            let running = Arc::new(AtomicBool::new(true));
            let r = running.clone();
            ctrlc::set_handler(move || {
                r.store(false, Ordering::SeqCst);
            })
            .expect("Error setting Ctrl-C handler");

            let mut terminal = ratatui::init();
            let action = run_tui(&mut terminal, &args, running);
            ratatui::restore();
            // Run spread/collapse after terminal is restored (popup closed)
            handle_post_action(action?)
        }
    }
}
