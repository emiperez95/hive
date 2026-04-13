//! Session management commands: connect, cycle, spread, collapse, start.

use anyhow::Result;

use crate::common::persistence::{load_skipped_sessions, save_skipped_sessions};
use crate::common::projects::{connect_project, ProjectRegistry};
use crate::common::tmux::{
    get_current_tmux_session, get_current_tmux_session_names, get_other_client_sessions,
    switch_to_session,
};

/// Cycle to next/prev tmux session, skipping skipped sessions
pub fn run_cycle(forward: bool) -> Result<()> {
    let skipped = load_skipped_sessions();
    let other_clients = get_other_client_sessions();
    let all_sessions = get_current_tmux_session_names();

    let filtered: Vec<&String> = all_sessions
        .iter()
        .filter(|name| !skipped.contains(*name) && !other_clients.contains(*name))
        .collect();

    if filtered.is_empty() {
        return Ok(());
    }

    let current = get_current_tmux_session();

    let current_idx = current
        .as_ref()
        .and_then(|c| filtered.iter().position(|name| *name == c));

    let target = match current_idx {
        Some(idx) => {
            if filtered.len() <= 1 {
                return Ok(());
            }
            if forward {
                filtered[(idx + 1) % filtered.len()]
            } else {
                filtered[(idx + filtered.len() - 1) % filtered.len()]
            }
        }
        None => filtered[0],
    };

    switch_to_session(target);
    Ok(())
}

/// Spread tmux sessions into N vertical iTerm2 panes
pub fn run_spread(count: usize) -> Result<()> {
    if count <= 1 {
        return Ok(());
    }
    crate::common::tmux::set_all_sessions_layout("spread");
    crate::common::iterm::spread_panes(count - 1);
    Ok(())
}

/// Collapse iTerm2 panes back to a single pane
pub fn run_collapse() -> Result<()> {
    crate::common::iterm::collapse_panes();
    crate::common::tmux::set_all_sessions_layout("collapse");
    Ok(())
}

/// Connect to a registered project by key
pub fn run_connect(key: &str) -> Result<()> {
    let registry = ProjectRegistry::load();
    let config = registry
        .projects
        .get(key)
        .ok_or_else(|| anyhow::anyhow!("Project '{}' not found in registry", key))?;

    let session_name = ProjectRegistry::session_name(key, config);
    if !connect_project(&session_name) {
        anyhow::bail!("Failed to create/connect session for '{}'", key);
    }
    // Unskip if it was skipped — user explicitly chose to connect
    let mut skipped = load_skipped_sessions();
    if skipped.remove(&session_name) {
        save_skipped_sessions(&skipped);
    }
    switch_to_session(&session_name);
    Ok(())
}

/// Find the first tmux session not skipped and not attached to another client.
pub fn run_start() -> Result<Option<String>> {
    let skipped = load_skipped_sessions();
    let other_clients = get_other_client_sessions();
    let sessions: Vec<String> = get_current_tmux_session_names()
        .into_iter()
        .filter(|name| !skipped.contains(name))
        .collect();

    // Prefer a session not attached elsewhere, fall back to any non-skipped session
    let target = sessions
        .iter()
        .find(|name| !other_clients.contains(*name))
        .or_else(|| sessions.first());

    Ok(target.cloned())
}
