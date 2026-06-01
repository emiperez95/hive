//! `hive config` — view and edit global hive defaults (`~/.hive/config.toml`).

use crate::cli::ConfigCommand;
use crate::common::config::HiveConfig;
use anyhow::Result;

pub fn run_config(command: ConfigCommand) -> Result<()> {
    match command {
        ConfigCommand::List => run_config_list(),
        ConfigCommand::Get { key } => run_config_get(&key),
        ConfigCommand::Set { key, value } => run_config_set(&key, &value),
        ConfigCommand::Path => run_config_path(),
    }
}

fn run_config_list() -> Result<()> {
    let config = HiveConfig::load();
    for (key, value) in config.entries() {
        println!("{} = {}", key, value);
    }
    Ok(())
}

fn run_config_get(key: &str) -> Result<()> {
    let config = HiveConfig::load();
    match config.get(key) {
        Some(value) => {
            println!("{}", value);
            Ok(())
        }
        None => anyhow::bail!(
            "unknown config key '{}' (run `hive config list` to see all keys)",
            key
        ),
    }
}

fn run_config_set(key: &str, value: &str) -> Result<()> {
    let mut config = HiveConfig::load();
    config.set(key, value)?;
    config.save()?;
    println!("{} = {}", key, value);
    Ok(())
}

fn run_config_path() -> Result<()> {
    println!("{}", HiveConfig::config_path().display());
    Ok(())
}
