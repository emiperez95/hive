//! Session management commands: connect, cycle, spread, collapse, start.

use anyhow::Result;

use crate::common::persistence::{load_skipped_sessions, save_skipped_sessions};
use crate::common::projects::{connect_project, ProjectRegistry};
use crate::common::tmux::{
    display_message_for_pane, get_current_tmux_session, get_current_tmux_session_names,
    get_current_tmux_window, get_other_client_sessions, select_window, switch_to_session,
};

/// Resolve the session that owns `pane` — the pane that triggered the keybind,
/// passed as `#{pane_id}`. Falls back to the attached client's current session
/// when no pane is given (e.g. `hive cycle-next` run by hand). See
/// [`display_message_for_pane`] for why the pane must be passed explicitly.
fn current_session(pane: Option<&str>) -> Option<String> {
    match pane {
        Some(p) => display_message_for_pane(p, "#{session_name}"),
        None => get_current_tmux_session(),
    }
}

/// Resolve the window index of `pane`, falling back to the client's current window.
fn current_window(pane: Option<&str>) -> Option<String> {
    match pane {
        Some(p) => display_message_for_pane(p, "#{window_index}"),
        None => get_current_tmux_window(),
    }
}

/// Cycle to next/prev tmux session, skipping skipped sessions.
/// `pane` is the triggering pane (`#{pane_id}`) used to locate the current session.
pub fn run_cycle(forward: bool, pane: Option<&str>) -> Result<()> {
    let skipped = load_skipped_sessions();
    let other_clients = get_other_client_sessions();
    let current = current_session(pane);
    cycle_among(forward, current, |name| {
        !skipped.contains(name) && !other_clients.contains(name)
    })
}

/// Jump to the next non-busy Claude *window*. The unit is the window, not the
/// session: a session can host several Claude windows, so this lands precisely
/// on a free one — inside the current session first, then out to other sessions.
///
/// "Free" = a Claude window whose status isn't Working or a running background
/// workflow, in a session that isn't skipped or attached to another client.
/// Always moves forward — it's a single "jump to a free window" action.
/// `pane` is the triggering pane (`#{pane_id}`) used to locate the current window.
pub fn run_cycle_free(pane: Option<&str>) -> Result<()> {
    use crate::ipc::messages::{HookState, SessionStatus};
    use sysinfo::System;

    let skipped = load_skipped_sessions();
    let other_clients = get_other_client_sessions();

    // Reuse the dashboard's per-window status resolution so "busy" stays
    // consistent across the app. Heavier than a plain cycle (full process scan),
    // but it's a one-shot keypress.
    let mut sys = System::new_all();
    sys.refresh_all();
    let hook_state = HookState::load();
    let session_data = crate::serve::server::gather_session_data(&sys, &hook_state);

    // Flatten to one entry per Claude window in tmux order. A session's windows
    // are contiguous (sorted by window index), so a forward scan from the current
    // window naturally exhausts the current session before moving outside it.
    let mut windows: Vec<(String, String, bool)> = Vec::new(); // (session, window_index, free)
    for sv in &session_data {
        let session_blocked = skipped.contains(&sv.name) || other_clients.contains(&sv.name);
        for w in &sv.windows {
            let Some((s, win, _pane)) = w.pane.clone() else {
                continue;
            };
            let busy = matches!(
                w.status,
                Some(SessionStatus::Working) | Some(SessionStatus::RunningWorkflow { .. })
            );
            windows.push((s, win, !busy && !session_blocked));
        }
    }

    if !windows.iter().any(|(_, _, free)| *free) {
        return Ok(());
    }

    // Locate the current window, then scan forward (wrapping) for the next free
    // one. Starting at current+1 means a free current window is only revisited
    // last — so the keypress always moves if anywhere else is free.
    let cur_session = current_session(pane);
    let cur_window = current_window(pane);
    let cur_idx = match (&cur_session, &cur_window) {
        (Some(s), Some(w)) => windows.iter().position(|(ws, ww, _)| ws == s && ww == w),
        _ => None,
    };

    let n = windows.len();
    let start = cur_idx.map(|i| i + 1).unwrap_or(0);
    let target = (0..n)
        .map(|step| &windows[(start + step) % n])
        .find(|(_, _, free)| *free);

    if let Some((s, w, _)) = target {
        // Avoid a no-op switch when the only free window is the current one.
        if Some(s) != cur_session.as_ref() || Some(w) != cur_window.as_ref() {
            switch_to_session(s);
            select_window(s, w);
        }
    }
    Ok(())
}

/// Shared cycle core: switch to the next/prev session among those for which
/// `eligible` returns true, preserving tmux's session order. When the current
/// session isn't eligible (e.g. it's busy/skipped), fall back to the nearest
/// eligible neighbor in the requested direction. `current` is the resolved
/// current session (from the triggering pane).
fn cycle_among(
    forward: bool,
    current: Option<String>,
    eligible: impl Fn(&String) -> bool,
) -> Result<()> {
    let all_sessions = get_current_tmux_session_names();

    let filtered: Vec<&String> = all_sessions.iter().filter(|name| eligible(name)).collect();

    if filtered.is_empty() {
        return Ok(());
    }

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
        None => {
            // Current session is not in `filtered` (e.g. it was just skipped).
            // Find its position in the full session list and pick the nearest
            // non-skipped neighbor in the requested direction.
            let full_idx = current
                .as_ref()
                .and_then(|c| all_sessions.iter().position(|name| name == c));
            match full_idx {
                Some(idx) => {
                    let n = all_sessions.len();
                    let mut target = None;
                    for step in 1..=n {
                        let probe = if forward {
                            (idx + step) % n
                        } else {
                            (idx + n - step) % n
                        };
                        let name = &all_sessions[probe];
                        if filtered.contains(&name) {
                            target = Some(name);
                            break;
                        }
                    }
                    match target {
                        Some(t) => t,
                        None => return Ok(()),
                    }
                }
                None => filtered[0],
            }
        }
    };

    switch_to_session(target);
    Ok(())
}

/// Cycle to next/prev tmux window within the current session. `pane` is the
/// triggering pane (`#{pane_id}`); its session is targeted explicitly so the
/// right session advances even with multiple attached clients.
pub fn run_window_cycle(forward: bool, pane: Option<&str>) -> Result<()> {
    use crate::common::tmux::resolve_tmux_path;
    let tmux = resolve_tmux_path();
    let cmd = if forward {
        "next-window"
    } else {
        "previous-window"
    };
    let mut command = std::process::Command::new(tmux);
    command.arg(cmd);
    if let Some(session) = current_session(pane) {
        command.args(["-t", &session]);
    }
    command.status().ok();
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
