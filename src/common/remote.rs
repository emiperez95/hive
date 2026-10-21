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

/// Run a `hive …` subcommand on the remote; errors carry its stderr.
pub fn remote_hive_run(r: &RemoteConfig, args: &[&str]) -> Result<String> {
    let cmd = std::iter::once(sh_quote(&remote_hive(r)))
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
