//! Todo management commands.

use anyhow::{bail, Result};

use super::TodoCommand;
use crate::common::persistence::{
    load_completed_todos, load_session_todos, save_completed_todos, save_session_todos,
};
use crate::common::tmux::get_current_tmux_session;

/// Resolve session name: explicit flag or auto-detect from tmux
fn resolve_session(explicit: Option<String>) -> Result<String> {
    match explicit {
        Some(name) => Ok(name),
        None => get_current_tmux_session()
            .ok_or_else(|| anyhow::anyhow!("Could not detect tmux session. Use --session <name>.")),
    }
}

/// Dispatch todo subcommands
pub fn run_todo(command: TodoCommand) -> Result<()> {
    match command {
        TodoCommand::List { session, done } => run_todo_list(session, done),
        TodoCommand::Next { session } => run_todo_next(session),
        TodoCommand::Add { text, session } => run_todo_add(text, session),
        TodoCommand::Done { index, session } => run_todo_done(index, session),
        TodoCommand::Clear { session } => run_todo_clear(session),
    }
}

/// List todos: active or completed, 1-based INDEX\tTEXT per line
fn run_todo_list(session: Option<String>, done: bool) -> Result<()> {
    let session = resolve_session(session)?;
    let todos = if done {
        load_completed_todos()
    } else {
        load_session_todos()
    };
    if let Some(items) = todos.get(&session) {
        for (i, item) in items.iter().enumerate() {
            println!("{}\t{}", i + 1, item);
        }
    }
    Ok(())
}

/// Print first active todo as raw text, exit 1 if none
fn run_todo_next(session: Option<String>) -> Result<()> {
    let session = resolve_session(session)?;
    let todos = load_session_todos();
    if let Some(items) = todos.get(&session) {
        if let Some(first) = items.first() {
            println!("{}", first);
            return Ok(());
        }
    }
    std::process::exit(1);
}

/// Add a todo to the active list
fn run_todo_add(text: String, session: Option<String>) -> Result<()> {
    let session = resolve_session(session)?;
    let mut todos = load_session_todos();
    todos.entry(session).or_default().push(text);
    save_session_todos(&todos);
    Ok(())
}

/// Mark a todo as done: remove from active, append to completed
fn run_todo_done(index: Option<usize>, session: Option<String>) -> Result<()> {
    let session = resolve_session(session)?;
    let mut todos = load_session_todos();
    let items = todos.entry(session.clone()).or_default();
    let idx = index.unwrap_or(1);
    if idx == 0 || idx > items.len() {
        bail!("Invalid todo index: {} (have {} todo(s))", idx, items.len());
    }
    let removed = items.remove(idx - 1);
    if items.is_empty() {
        todos.remove(&session);
    }
    save_session_todos(&todos);

    let mut completed = load_completed_todos();
    completed.entry(session).or_default().push(removed.clone());
    save_completed_todos(&completed);

    println!("{}", removed);
    Ok(())
}

/// Clear completed todos for a session
fn run_todo_clear(session: Option<String>) -> Result<()> {
    let session = resolve_session(session)?;
    let mut completed = load_completed_todos();
    completed.remove(&session);
    save_completed_todos(&completed);
    Ok(())
}
