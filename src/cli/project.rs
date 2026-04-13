//! Project registry commands.

use anyhow::Result;

use super::ProjectCommand;
use crate::common::projects::{DatabaseConfig, FilePatterns, PortConfig, ProjectConfig, ProjectRegistry};

/// Add a project to the registry
pub fn run_project_add(cmd: ProjectCommand) -> Result<()> {
    let ProjectCommand::Add {
        key,
        emoji,
        path,
        display_name,
        startup,
        worktrees_dir,
        base_branch,
        package_manager,
        ports_enabled,
        base_port,
        port_increment,
        db_enabled,
        db_prefix,
        copy_files,
        symlink_files,
        hooks_dir,
    } = cmd
    else {
        unreachable!()
    };

    let mut registry = ProjectRegistry::load();

    if registry.projects.contains_key(&key) {
        anyhow::bail!(
            "Project '{}' already exists. Remove it first to re-add.",
            key
        );
    }

    let project_root = path.unwrap_or_else(|| {
        std::env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| ".".to_string())
    });

    let config = ProjectConfig {
        emoji,
        project_root,
        display_name,
        startup_command: startup,
        worktrees_dir,
        default_base_branch: base_branch,
        worktree_types: Vec::new(),
        package_manager,
        ports: PortConfig {
            enabled: ports_enabled,
            base_port: base_port.unwrap_or(0),
            increment: port_increment.unwrap_or(1),
        },
        database: DatabaseConfig {
            enabled: db_enabled,
            prefix: db_prefix,
        },
        files: FilePatterns {
            copy: copy_files,
            symlink: symlink_files,
        },
        hooks_dir,
        auth_profile: None,
    };

    let session_name = ProjectRegistry::session_name(&key, &config);
    registry.add_project(key, config);
    registry.save()?;
    println!("Added project '{}'", session_name);
    Ok(())
}

/// Remove a project from the registry
pub fn run_project_remove(key: &str) -> Result<()> {
    let mut registry = ProjectRegistry::load();
    if !registry.remove_project(key) {
        anyhow::bail!("Project '{}' not found in registry", key);
    }
    registry.save()?;
    println!("Removed project '{}'", key);
    Ok(())
}

/// List all configured projects
pub fn run_project_list() -> Result<()> {
    let registry = ProjectRegistry::load();

    if registry.projects.is_empty() {
        println!("No projects configured. Use 'hive project add' or 'hive project import'.");
        return Ok(());
    }

    // Sort by key for consistent output
    let mut entries: Vec<_> = registry.projects.iter().collect();
    entries.sort_by_key(|(k, _)| k.as_str());

    // Calculate column widths
    let max_key = entries.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
    let max_session = entries
        .iter()
        .map(|(k, c)| ProjectRegistry::session_name(k, c).len())
        .max()
        .unwrap_or(0);

    println!(
        "{:<width_k$}  {:<width_s$}  PATH",
        "KEY",
        "SESSION",
        width_k = max_key,
        width_s = max_session
    );
    println!(
        "{:<width_k$}  {:<width_s$}  ----",
        "-".repeat(max_key),
        "-".repeat(max_session),
        width_k = max_key,
        width_s = max_session
    );

    for (key, config) in &entries {
        let session = ProjectRegistry::session_name(key, config);
        println!(
            "{:<width_k$}  {:<width_s$}  {}",
            key,
            session,
            config.project_root,
            width_k = max_key,
            width_s = max_session
        );
    }

    println!("\n{} project(s)", entries.len());
    Ok(())
}

/// Import projects from sesh.toml
pub fn run_project_import() -> Result<()> {
    use crate::common::projects::parse_sesh_toml;

    let home =
        dirs::home_dir().ok_or_else(|| anyhow::anyhow!("Cannot determine home directory"))?;
    let dot_config_path = home.join(".config").join("sesh").join("sesh.toml");
    let xdg_path = dirs::config_dir().map(|p| p.join("sesh").join("sesh.toml"));

    let sesh_path = if dot_config_path.exists() {
        dot_config_path
    } else if let Some(ref xdg) = xdg_path {
        if xdg.exists() {
            xdg.clone()
        } else {
            anyhow::bail!("sesh.toml not found at {:?} or {:?}", dot_config_path, xdg);
        }
    } else {
        anyhow::bail!("sesh.toml not found at {:?}", dot_config_path);
    };

    let entries = parse_sesh_toml(&sesh_path)?;
    let mut registry = ProjectRegistry::load();
    let mut added = 0;
    let mut skipped = 0;

    for (key, config) in entries {
        if registry.projects.contains_key(&key) {
            skipped += 1;
        } else {
            let name = ProjectRegistry::session_name(&key, &config);
            println!("  + {}", name);
            registry.add_project(key, config);
            added += 1;
        }
    }

    registry.save()?;
    println!(
        "\nImported {} project(s), skipped {} existing",
        added, skipped
    );
    Ok(())
}
