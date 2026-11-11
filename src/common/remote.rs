//! Talking to another machine running hive: ssh for commands, rsync for boxes.
//!
//! `HIVE_SSH` overrides the ssh program (default `ssh`). It is used for both direct ssh
//! calls and `rsync -e`, which is how the tests stand a second `$HOME` in for a second
//! machine without needing a passwordless sshd.

use anyhow::{anyhow, bail, Result};
use std::process::{Command, Output};

use crate::common::config::{HiveConfig, RemoteConfig};
use crate::common::tmux::sh_quote;

pub fn ssh_program() -> String {
    std::env::var("HIVE_SSH").unwrap_or_else(|_| "ssh".to_string())
}

pub fn find_remote(name: &str) -> Result<RemoteConfig> {
    HiveConfig::load()
        .remote(name)
        .cloned()
        .ok_or_else(|| anyhow!("no remote named '{name}' (see `hive remote list`)"))
}

/// `hive` as the remote's non-interactive shell should call it.
pub fn remote_hive(r: &RemoteConfig) -> String {
    r.hive_path.clone().unwrap_or_else(|| "hive".to_string())
}

/// Run `cmd` (a shell string) on the remote. Never prompts: a password prompt would hang
/// the TUI, so a remote that needs one fails fast instead.
pub fn ssh_output(r: &RemoteConfig, cmd: &str) -> Result<Output> {
    Command::new(ssh_program())
        .args(["-o", "BatchMode=yes", &r.ssh, cmd])
        .output()
        .map_err(|e| anyhow!("failed to run ssh: {e}"))
}

/// Where a POSIX remote keeps the tools hive needs. A command sent over ssh runs in a
/// non-login, non-interactive shell, whose PATH on macOS lacks Homebrew (`tmux`, `node`) and
/// `~/.local/bin` (`hive`, `claude`): `ssh -t host tmux attach` answers "command not found"
/// on a machine where `tmux` works fine at its own prompt. Prefixed to every command hive
/// runs there; `$PATH` and `$HOME` are expanded by the REMOTE shell, so the string must reach
/// it unexpanded. (Windows' cmd.exe has neither syntax — not a supported remote.)
pub const REMOTE_PATH: &str =
    "PATH=/opt/homebrew/bin:/usr/local/bin:$HOME/.local/bin:$HOME/.cargo/bin:$PATH";

/// Run a `hive …` subcommand on the remote; errors carry its stderr.
pub fn remote_hive_run(r: &RemoteConfig, args: &[&str]) -> Result<String> {
    let cmd = std::iter::once(REMOTE_PATH.to_string())
        .chain(std::iter::once(sh_quote(&remote_hive(r))))
        .chain(args.iter().map(|a| sh_quote(a)))
        .collect::<Vec<_>>()
        .join(" ");
    let out = ssh_output(r, &cmd)?;
    if !out.status.success() {
        bail!(
            "`hive {}` failed on {}: {}",
            args.join(" "),
            r.name,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

fn rsync_rsh() -> String {
    format!("{} -o BatchMode=yes", ssh_program())
}

/// Copy the local directory `src` to `~/<remote_rel>` on the remote (mirror: `--delete`).
pub fn rsync_dir_to(r: &RemoteConfig, src: &std::path::Path, remote_rel: &str) -> Result<()> {
    run_rsync(&[
        "-a",
        "--delete",
        "-e",
        &rsync_rsh(),
        &format!("{}/", src.display()),
        &format!("{}:{}/", r.ssh, remote_rel),
    ])
}

/// Copy `~/<remote_rel>` on the remote to the local directory `dst` (mirror).
pub fn rsync_dir_from(r: &RemoteConfig, remote_rel: &str, dst: &std::path::Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    run_rsync(&[
        "-a",
        "--delete",
        "-e",
        &rsync_rsh(),
        &format!("{}:{}/", r.ssh, remote_rel),
        &format!("{}/", dst.display()),
    ])
}

fn run_rsync(args: &[&str]) -> Result<()> {
    let out = Command::new("rsync").args(args).output()?;
    if !out.status.success() {
        bail!(
            "rsync failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// What this machine calls itself in `location`.
pub fn local_name() -> String {
    crate::common::boxes::hostname()
}

// ─── Attaching to a remote conversation ──────────────────────────────────────

/// The local window's name for a remote conversation's window: `🛰 Argus @mini:2` — the
/// remote session as every listing names it, plus the window's index there (unique and
/// stable, which its name is not).
pub fn attach_window_name(host: &str, remote_session: &str, window_index: &str) -> String {
    format!("{remote_session} @{host}:{window_index}")
}

/// What the REMOTE shell runs to attach: tmux, found via [`REMOTE_PATH`], on the exact window
/// (`=` so a longer session name can't be prefix-matched).
fn attach_shell(session: &str, window_index: &str) -> String {
    format!(
        "{REMOTE_PATH} tmux attach-session -t {}",
        sh_quote(&format!("={session}:{window_index}"))
    )
}

/// The shell command a local window runs to land in the remote conversation's window:
/// `ssh -t [-L p:localhost:p …] <target> tmux attach -t '=<session>:<window>'`.
///
/// The `-L` forwards are the conversation's listening ports, so the detail view's port
/// list, Chrome matching and Enter-on-port keep working against `localhost` as before.
pub fn attach_command(
    r: &RemoteConfig,
    session: &str,
    window_index: &str,
    ports: &[u16],
) -> String {
    let mut parts = vec![sh_quote(&ssh_program()), "-t".to_string()];
    for p in ports {
        parts.push("-L".to_string());
        parts.push(sh_quote(&format!("{p}:localhost:{p}")));
    }
    parts.push(sh_quote(&r.ssh));
    parts.push(sh_quote(&attach_shell(session, window_index)));
    parts.join(" ")
}

/// Find or create the local window attached to a remote conversation, in the session
/// that owns the conversation's project here. Returns `(local session, window index)`
/// for the caller to switch to; a second call finds the same window.
pub fn attach_remote(c: &crate::common::registry::Conversation) -> Result<(String, String)> {
    let host = c
        .host
        .as_deref()
        .ok_or_else(|| anyhow!("not a remote conversation"))?;
    let p = c
        .placement
        .as_ref()
        .ok_or_else(|| anyhow!("remote conversation has no window to attach to"))?;
    attach_window(host, &p.session_name, &p.window_index, &c.ports)
}

/// [`attach_remote`] for a caller that holds only names (the sidebar, which sees the
/// dashboard's rows and not the registry): open — or find — the local window attached to
/// `remote_session:window_index` on `host`, inside `local_session`.
pub fn attach_window(
    host: &str,
    remote_session: &str,
    window_index: &str,
    ports: &[u16],
) -> Result<(String, String)> {
    use crate::common::tmux::exact;
    let r = find_remote(host)?;
    // Always the hidden session: the conversation lives on `host`, so nothing named after
    // its project may appear here as if it ran on this machine too.
    let local_session = crate::common::config::REMOTE_SESSION.to_string();
    let name = attach_window_name(host, remote_session, window_index);
    let cmd = attach_command(&r, remote_session, window_index, ports);

    let tmux = |args: &[&str]| Command::new("tmux").args(args).output();
    let session_alive = tmux(&["has-session", "-t", &exact(&local_session)])
        .map(|o| o.status.success())
        .unwrap_or(false);

    if session_alive {
        // An existing attach window for this conversation: just switch to it.
        let listing = tmux(&[
            "list-windows",
            "-t",
            &exact(&local_session),
            "-F",
            "#{window_index}\t#{window_name}",
        ])?;
        for line in String::from_utf8_lossy(&listing.stdout).lines() {
            if let Some((idx, n)) = line.split_once('\t') {
                if n == name {
                    return Ok((local_session, idx.to_string()));
                }
            }
        }
        let out = tmux(&[
            "new-window",
            "-P",
            "-F",
            "#{window_index}",
            "-t",
            &format!("{}:", exact(&local_session)),
            "-n",
            &name,
            &cmd,
        ])?;
        if !out.status.success() {
            bail!(
                "tmux new-window failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        let idx = String::from_utf8_lossy(&out.stdout).trim().to_string();
        Ok((local_session, idx))
    } else {
        let out = tmux(&[
            "new-session",
            "-d",
            "-P",
            "-F",
            "#{window_index}",
            "-s",
            &local_session,
            "-n",
            &name,
            &cmd,
        ])?;
        if !out.status.success() {
            bail!(
                "tmux new-session failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok((
            local_session,
            String::from_utf8_lossy(&out.stdout).trim().to_string(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn remote() -> RemoteConfig {
        RemoteConfig {
            name: "mini".into(),
            ssh: "me@mini.local".into(),
            api: None,
            hive_path: None,
        }
    }

    #[test]
    fn attach_command_forwards_ports_and_targets_the_exact_window() {
        let c = attach_command(&remote(), "📊 Avateen", "3", &[3000, 5432]);
        assert!(c.contains("-L '3000:localhost:3000'") && c.contains("-L '5432:localhost:5432'"));
        assert!(c.contains("'me@mini.local'"));
        // `=` keeps tmux from prefix-matching a longer session name on the remote.
        assert!(c.contains("=📊 Avateen:3"), "{c}");
        assert!(c.contains("-t"), "{c}");
    }

    #[test]
    fn attach_command_survives_a_hostile_session_name() {
        let c = attach_command(&remote(), "it's $(x)", "1", &[]);
        // Runs through a real shell: the whole thing must come back as inert words.
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("set -- {}; echo $#", c.replace("'ssh'", "'echo'")))
            .output()
            .unwrap();
        assert!(out.status.success());
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "4");
    }

    #[test]
    fn attach_window_names_are_recognisable_plumbing() {
        assert_eq!(
            attach_window_name("mini", "🛰 Argus", "2"),
            "🛰 Argus @mini:2"
        );
    }

    /// The reason for `REMOTE_PATH`: on a macOS remote `tmux` lives in Homebrew, which a
    /// non-interactive ssh shell does not have on PATH. Run the prefix the way the remote
    /// shell receives it, from a bare PATH, and ask where `tmux` resolves.
    #[test]
    fn the_remote_path_prefix_reaches_tools_a_bare_path_misses() {
        let home = std::env::temp_dir().join(format!("hive-remote-path-{}", std::process::id()));
        let bin = home.join(".local/bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("only-in-local-bin"), "#!/bin/sh\n").unwrap();
        std::process::Command::new("chmod")
            .arg("+x")
            .arg(bin.join("only-in-local-bin"))
            .output()
            .unwrap();
        let resolve = |prefix: &str| {
            let out = std::process::Command::new("sh")
                .arg("-c")
                .arg(format!("{prefix} command -v only-in-local-bin"))
                .env("HOME", &home)
                .env("PATH", "/usr/bin:/bin")
                .output()
                .unwrap();
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        assert_eq!(resolve(""), "", "a bare ssh PATH cannot see ~/.local/bin");
        assert_eq!(
            resolve(&format!("{REMOTE_PATH};")),
            bin.join("only-in-local-bin").to_string_lossy()
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn the_attach_command_the_remote_receives_is_path_prefixed_and_exact() {
        let c = attach_shell("🛰 Argus", "2");
        assert!(c.starts_with("PATH="), "{c}");
        assert!(c.ends_with("tmux attach-session -t '=🛰 Argus:2'"), "{c}");
    }
}
