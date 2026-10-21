//! Project registry commands.

use anyhow::Result;

use super::ProjectCommand;
use crate::common::projects::{
    DatabaseConfig, FilePatterns, PortConfig, ProjectConfig, ProjectRegistry,
};

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
        auth_profile,
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

    // An auth profile is only useful if ~/.claude-{name} exists: CLAUDE_CONFIG_DIR
    // pointing at a missing directory makes Claude start as a fresh, unauthenticated
    // identity rather than erroring. Warn instead of failing — the profile may be set
    // up after registering.
    if let Some(profile) = &auth_profile {
        if let Some(dir) = dirs::home_dir().map(|h| h.join(format!(".claude-{profile}"))) {
            if !dir.exists() {
                eprintln!(
                    "Warning: auth profile directory {} does not exist yet",
                    dir.display()
                );
            }
        }
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
        auth_profile,
        archived: false,
        tasks: None,
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
    // The session names must be read BEFORE the project leaves the registry.
    let mut sessions: Vec<String> = registry
        .projects
        .get(key)
        .map(|c| ProjectRegistry::session_name(key, c))
        .into_iter()
        .collect();
    sessions.extend(
        crate::common::worktree::WorktreeState::load()
            .worktrees
            .values()
            .filter(|e| e.project_key == key)
            .map(|e| e.session_name.clone()),
    );
    if !registry.remove_project(key) {
        anyhow::bail!("Project '{}' not found in registry", key);
    }
    registry.save()?;
    println!("Removed project '{}'", key);
    print_dropped_todos(&crate::common::todos::drop_session_todos(&sessions));
    Ok(())
}

/// Echo todos a removal dropped, so they survive in the scrollback at least.
pub fn print_dropped_todos(dropped: &[(String, String)]) {
    if dropped.is_empty() {
        return;
    }
    println!("  Dropped {} todo(s):", dropped.len());
    for (session, text) in dropped {
        println!("    [{session}] {text}");
    }
}

/// Archive or unarchive a project in the registry
pub fn run_project_set_archived(key: &str, archived: bool) -> Result<()> {
    let mut registry = ProjectRegistry::load();
    if !registry.set_archived(key, archived) {
        anyhow::bail!("Project '{}' not found in registry", key);
    }
    registry.save()?;
    let verb = if archived { "Archived" } else { "Unarchived" };
    println!("{} project '{}'", verb, key);
    Ok(())
}

/// List all configured projects
pub fn run_project_list(all: bool) -> Result<()> {
    let registry = ProjectRegistry::load();

    if registry.projects.is_empty() {
        println!("No projects configured. Use 'hive project add' or 'hive project import'.");
        return Ok(());
    }

    let archived_total = registry.projects.values().filter(|c| c.archived).count();

    // Sort by key for consistent output; hide archived unless --all
    let mut entries: Vec<_> = registry
        .projects
        .iter()
        .filter(|(_, c)| all || !c.archived)
        .collect();
    entries.sort_by_key(|(k, _)| k.as_str());

    if entries.is_empty() {
        println!("No active projects. Use 'hive project list --all' to show archived.");
        return Ok(());
    }

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
        let marker = if config.archived { "  [archived]" } else { "" };
        println!(
            "{:<width_k$}  {:<width_s$}  {}{}",
            key,
            session,
            config.project_root,
            marker,
            width_k = max_key,
            width_s = max_session
        );
    }

    if all {
        println!(
            "\n{} project(s) ({} archived)",
            entries.len(),
            archived_total
        );
    } else if archived_total > 0 {
        println!(
            "\n{} project(s) ({} archived hidden — use --all)",
            entries.len(),
            archived_total
        );
    } else {
        println!("\n{} project(s)", entries.len());
    }
    Ok(())
}

/// Print the registry (or one project) as TOML, ready for `hive project import` on another
/// machine. Paths are written as registered: the other machine must have them at the same
/// absolute location, which is what a box needs anyway.
pub fn run_project_export(key: Option<&str>) -> Result<()> {
    let mut reg = ProjectRegistry::load();
    if let Some(k) = key {
        reg.projects.retain(|name, _| name == k);
        if reg.projects.is_empty() {
            anyhow::bail!("project '{k}' not found");
        }
    }
    reg.worktrees_root = None;
    print!("{}", toml::to_string_pretty(&reg)?);
    Ok(())
}

/// Merge projects from a TOML file (`-` for stdin). Existing keys are kept unless `force`:
/// re-adding would clobber local auth profile / ports / worktrees dir.
pub fn run_project_import(file: &str, force: bool) -> Result<()> {
    use std::io::Read;
    let mut text = String::new();
    if file == "-" {
        std::io::stdin().read_to_string(&mut text)?;
    } else {
        text = std::fs::read_to_string(file)?;
    }
    let incoming: ProjectRegistry = toml::from_str(&text)?;
    let mut reg = ProjectRegistry::load();
    let (mut added, mut skipped) = (0, 0);
    for (key, config) in incoming.projects {
        if reg.projects.contains_key(&key) && !force {
            skipped += 1;
            continue;
        }
        reg.add_project(key, config);
        added += 1;
    }
    reg.save()?;
    println!("imported {added} project(s), kept {skipped} existing");
    Ok(())
}
