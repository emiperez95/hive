//! Self-update command — downloads the latest prebuilt macOS binary
//! from GitHub Releases and replaces the currently-running executable.
//!
//! macOS only: v0.1.0 ships mac tarballs exclusively. Users on other
//! platforms can build from source via `cargo install --git ...`.

use anyhow::{anyhow, bail, Context, Result};
use std::fs;
use std::process::{Command, Stdio};

const REPO: &str = "emiperez95/hive";

/// Download the latest mac release tarball, replace the current binary,
/// and re-run `hive setup` so hook paths stay correct.
pub fn run_update() -> Result<()> {
    let target = detect_target()?;
    let asset = format!("hive-{}.tar.gz", target);
    let url = format!(
        "https://github.com/{}/releases/latest/download/{}",
        REPO, asset
    );

    let current_exe = std::env::current_exe()
        .context("Could not determine current hive binary path")?;

    println!("Current version: {}", env!("CARGO_PKG_VERSION"));
    println!("Downloading: {}", url);
    println!();

    require_tool("curl")?;
    require_tool("tar")?;

    let tmp_dir = std::env::temp_dir().join(format!("hive-update-{}", std::process::id()));
    fs::create_dir_all(&tmp_dir).context("Could not create temp dir for update")?;

    // Scope guard — best-effort cleanup on all exit paths.
    let cleanup = CleanupDir(tmp_dir.clone());

    let archive = tmp_dir.join(&asset);
    let status = Command::new("curl")
        .args([
            "-fsSL",
            &url,
            "-o",
            &archive.to_string_lossy(),
        ])
        .status()
        .context("Failed to invoke curl")?;
    if !status.success() {
        bail!(
            "Download failed. Check that a release exists at https://github.com/{}/releases/latest",
            REPO
        );
    }

    let status = Command::new("tar")
        .args([
            "xzf",
            &archive.to_string_lossy(),
            "-C",
            &tmp_dir.to_string_lossy(),
        ])
        .status()
        .context("Failed to invoke tar")?;
    if !status.success() {
        bail!("Failed to extract {}", archive.display());
    }

    let new_binary = tmp_dir.join("hive");
    if !new_binary.exists() {
        bail!(
            "Extracted tarball did not contain a 'hive' binary (looked at {})",
            new_binary.display()
        );
    }

    // Replace the currently-running binary. On macOS, the running
    // process keeps its inode so this is safe; the next invocation
    // picks up the new file.
    fs::rename(&new_binary, &current_exe).with_context(|| {
        format!(
            "Failed to replace {}. You may need to run with elevated permissions.",
            current_exe.display()
        )
    })?;

    // Strip the quarantine bit if macOS set one during extraction.
    // xattr prints "No such xattr" to stderr when there's nothing to
    // remove, which is the common case — silence it.
    let _ = Command::new("xattr")
        .args(["-d", "com.apple.quarantine", &current_exe.to_string_lossy()])
        .stderr(Stdio::null())
        .status();

    drop(cleanup);

    println!("Installed new binary at {}", current_exe.display());
    println!();
    println!("Running setup with new binary...");
    println!();

    let setup_status = Command::new(&current_exe)
        .arg("setup")
        .status()
        .context("Failed to invoke new hive binary for setup")?;
    if !setup_status.success() {
        bail!("hive setup failed");
    }

    Ok(())
}

fn detect_target() -> Result<&'static str> {
    if std::env::consts::OS != "macos" {
        bail!(
            "hive update only supports macOS. On {}, build from source: cargo install --git https://github.com/{}",
            std::env::consts::OS,
            REPO
        );
    }
    match std::env::consts::ARCH {
        "aarch64" => Ok("aarch64-apple-darwin"),
        "x86_64" => Ok("x86_64-apple-darwin"),
        other => Err(anyhow!(
            "Unsupported macOS arch: {}. Expected aarch64 or x86_64.",
            other
        )),
    }
}

fn require_tool(cmd: &str) -> Result<()> {
    let ok = Command::new("which")
        .arg(cmd)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !ok {
        bail!("Required tool '{}' not found on PATH", cmd);
    }
    Ok(())
}

struct CleanupDir(std::path::PathBuf);

impl Drop for CleanupDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
