//! Self-update command.

use anyhow::{bail, Result};

/// Update hive to the latest version from GitHub, then re-run setup
pub fn run_update() -> Result<()> {
    const REPO_URL: &str = "https://github.com/emiperez95/hive";

    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("Cannot find home directory"))?;
    let install_root = home.join(".local");

    println!("Current version: {}", env!("CARGO_PKG_VERSION"));
    println!("Installing latest from {}...", REPO_URL);
    println!();

    let status = std::process::Command::new("cargo")
        .args([
            "install",
            "--git",
            REPO_URL,
            "--root",
            &install_root.to_string_lossy(),
        ])
        .status()?;

    if !status.success() {
        bail!("cargo install failed");
    }

    println!();
    println!("Running setup with new binary...");
    println!();

    let new_binary = install_root.join("bin").join("hive");
    let setup_status = std::process::Command::new(&new_binary)
        .arg("setup")
        .status()?;

    if !setup_status.success() {
        bail!("hive setup failed");
    }

    Ok(())
}
