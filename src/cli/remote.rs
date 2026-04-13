//! Remote machine management commands.

use anyhow::Result;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use super::RemoteCommand;
use crate::common::remotes::{RemoteConfig, RemoteRegistry};

/// Send keys to a remote tmux pane via SSH.
/// Uses `-o RemoteCommand=none` to bypass any RemoteCommand in SSH config.
/// Uses `bash -lc` to get a login shell with full PATH on the remote.
pub(crate) fn send_keys_to_remote(
    ssh_host: &str,
    session: &str,
    window: &str,
    pane: &str,
    keys: &[String],
) {
    let target = format!("{}:{}.{}", session, window, pane);
    for key in keys {
        let cmd = format!("tmux send-keys -t '{}' '{}'", target, key);
        let _ = std::process::Command::new("ssh")
            .args([
                "-T",
                "-o",
                "RemoteCommand=none",
                ssh_host,
                "bash",
                "-lc",
                &cmd,
            ])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output();
    }
}

/// Start `hive remote sync` in the background if remotes are configured and it's not already running.
pub(crate) fn ensure_remote_sync() {
    let registry = RemoteRegistry::load();
    if registry.remotes.is_empty() {
        return;
    }

    let pid_path = crate::common::persistence::cache_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp/.hive/cache"))
        .join("remote-sync.pid");

    // Check if already running
    if pid_path.exists() {
        if let Ok(content) = std::fs::read_to_string(&pid_path) {
            if let Ok(pid) = content.trim().parse::<i32>() {
                unsafe {
                    if libc::kill(pid, 0) == 0 {
                        return; // already running
                    }
                }
            }
        }
    }

    // Find hive binary
    let binary = dirs::home_dir()
        .map(|h| h.join(".local/bin/hive"))
        .filter(|p| p.exists())
        .or_else(|| std::env::current_exe().ok())
        .unwrap_or_else(|| std::path::PathBuf::from("hive"));

    // Spawn detached: stdin/stdout/stderr all null so it doesn't block
    let _ = std::process::Command::new(binary)
        .args(["remote", "sync"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

pub fn run_remote(command: RemoteCommand) -> Result<()> {
    match command {
        RemoteCommand::Add {
            name,
            host,
            label,
            emoji,
        } => {
            let mut registry = RemoteRegistry::load();
            let config = RemoteConfig {
                ssh_host: host,
                label: label.unwrap_or_else(|| name.clone()),
                emoji,
            };
            registry.remotes.insert(name.clone(), config);
            registry.save()?;
            println!("Added remote '{}'", name);
            Ok(())
        }
        RemoteCommand::Remove { name } => {
            let mut registry = RemoteRegistry::load();
            if registry.remotes.remove(&name).is_some() {
                registry.save()?;
                println!("Removed remote '{}'", name);
            } else {
                println!("Remote '{}' not found", name);
            }
            Ok(())
        }
        RemoteCommand::List => {
            let registry = RemoteRegistry::load();
            if registry.remotes.is_empty() {
                println!(
                    "No remotes configured. Add one with: hive remote add <name> --host <ssh-host>"
                );
            } else {
                for (name, config) in &registry.remotes {
                    println!(
                        "  {} {} ({}) → ssh {}",
                        config.emoji, name, config.label, config.ssh_host
                    );
                }
            }
            Ok(())
        }
        RemoteCommand::Sync => {
            use crate::common::remote_client::RemoteHandle;

            // PID file lock — prevent multiple sync instances
            let pid_path = crate::common::persistence::cache_dir()
                .unwrap_or_else(|| std::path::PathBuf::from("/tmp/.hive/cache"))
                .join("remote-sync.pid");

            // Check if another sync is already running
            if pid_path.exists() {
                if let Ok(content) = std::fs::read_to_string(&pid_path) {
                    if let Ok(pid) = content.trim().parse::<i32>() {
                        unsafe {
                            if libc::kill(pid, 0) == 0 {
                                println!("Remote sync already running (pid {})", pid);
                                return Ok(());
                            }
                        }
                    }
                }
            }

            // Write our PID
            if let Some(parent) = pid_path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(&pid_path, format!("{}", std::process::id()));

            let registry = RemoteRegistry::load();
            if registry.remotes.is_empty() {
                let _ = std::fs::remove_file(&pid_path);
                println!("No remotes configured.");
                return Ok(());
            }

            let handles: Vec<RemoteHandle> = registry
                .remotes
                .into_iter()
                .map(|(key, config)| {
                    println!("Connecting to {} (ssh {})...", key, config.ssh_host);
                    RemoteHandle::spawn(key, config)
                })
                .collect();

            println!("Syncing {} remote(s). Press Ctrl-C to stop.", handles.len());

            // Block until Ctrl-C
            let running = Arc::new(AtomicBool::new(true));
            let r = running.clone();
            ctrlc::set_handler(move || {
                r.store(false, Ordering::SeqCst);
            })
            .ok();

            while running.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_secs(1));
            }

            drop(handles); // triggers shutdown + join
            let _ = std::fs::remove_file(&pid_path);
            println!("\nStopped.");
            Ok(())
        }
    }
}
