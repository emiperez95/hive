//! Worktree lifecycle commands.

use anyhow::{bail, Result};

use crate::common::persistence::{load_auto_approve_sessions, save_auto_approve_sessions};
use crate::common::projects::{activate_project, expand_tilde, ProjectRegistry};
use crate::common::tmux::{get_current_tmux_session_names, kill_tmux_session};
use crate::common::worktree::*;

/// Create a new worktree: full 12-step workflow with hooks.
///
/// Returns the final tmux session name — a `post-copy` hook can override it via the
/// metadata protocol, so the caller can't derive it — which is what lets callers
/// switch you into the worktree once it's ready.
#[allow(clippy::too_many_arguments)]
pub fn run_wt_new(
    project: &str,
    branch: &str,
    base: Option<&str>,
    existing: bool,
    wt_type: &str,
    prompt: Option<&str>,
    auto_approve: bool,
    no_startup: bool,
) -> Result<String> {
    use anyhow::Context;

    // 1. Load project config, resolve worktrees dir, build default session name
    let registry = ProjectRegistry::load();
    let config = registry
        .projects
        .get(project)
        .ok_or_else(|| anyhow::anyhow!("Project '{}' not found in registry", project))?;

    let worktrees_dir = registry
        .resolve_worktrees_dir(project, config)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "No worktrees directory configured for '{}'. Set 'worktrees_dir' on the project or 'worktrees_root' globally in projects.toml.",
                project
            )
        })?;

    let project_root = expand_tilde(&config.project_root);
    let base_branch = base
        .map(|s| s.to_string())
        .or_else(|| config.default_base_branch.clone())
        .unwrap_or_else(|| "main".to_string());

    let mut session_name = build_session_name(config, project, wt_type, branch);
    let hooks_dir = resolve_hooks_dir(config, project);
    let mut metadata = serde_json::json!({});

    // Check if already registered (single load, reused for insertion later)
    let mut state = WorktreeState::load();
    if state.get(project, branch).is_some() {
        bail!(
            "Worktree '{}/{}' already exists in registry. Delete it first.",
            project,
            branch
        );
    }

    println!("Creating worktree {}/{}...", project, branch);

    // 2. pre-create hook (uses estimated worktree path since it doesn't exist yet)
    let pre_env = build_hook_env(
        project,
        branch,
        &worktrees_dir.join(branch),
        &project_root,
        &session_name,
        wt_type,
        &hooks_dir,
    );
    metadata = run_hook(&hooks_dir, "pre-create", &pre_env, &metadata)?;

    // 3. git worktree add
    let worktree_path = create_git_worktree(
        &project_root,
        &worktrees_dir,
        branch,
        &base_branch,
        existing,
    )?;
    println!("  Created worktree at {}", worktree_path.display());

    // Steps 4-12: any failure triggers cleanup of everything created so far.
    // Track what was created so cleanup knows what to tear down.
    let mut tmux_session_created = false;

    let result = (|| -> Result<()> {
        // Build hook env once (reused for post-worktree, post-copy, post-setup)
        let mut env = build_hook_env(
            project,
            branch,
            &worktree_path,
            &project_root,
            &session_name,
            wt_type,
            &hooks_dir,
        );

        // 4. post-worktree hook
        metadata = run_hook(&hooks_dir, "post-worktree", &env, &metadata)?;

        // 5. Copy/symlink file patterns
        if !config.files.copy.is_empty() {
            copy_file_patterns(&project_root, &worktree_path, &config.files.copy)?;
            println!("  Copied {} file pattern(s)", config.files.copy.len());
        }
        if !config.files.symlink.is_empty() {
            symlink_file_patterns(&project_root, &worktree_path, &config.files.symlink)?;
            println!("  Symlinked {} file pattern(s)", config.files.symlink.len());
        }

        // 6. Seed Claude memory + pre-trust
        seed_memory(&project_root, &worktree_path)?;
        pretrust_claude_project(&worktree_path)?;
        println!("  Seeded Claude memory (trusted)");

        // 7. post-copy hook
        metadata = run_hook(&hooks_dir, "post-copy", &env, &metadata)?;

        // 8. Check metadata for session name override, create tmux session
        if let Some(name_override) = metadata.get("session_name").and_then(|v| v.as_str()) {
            session_name = name_override.to_string();
            env.insert("HIVE_SESSION_NAME".to_string(), session_name.clone());
        }

        // Build tmux new-session with env vars from project config (e.g. CLAUDE_CONFIG_DIR).
        // -e passes env to the initial shell, so the startup command (claude) inherits it.
        let env_vars = config.tmux_env();
        let env_strings: Vec<String> = env_vars
            .iter()
            .map(|(k, v)| format!("{}={}", k, v))
            .collect();
        let mut tmux_cmd = std::process::Command::new("tmux");
        tmux_cmd.args(["new-session", "-d"]);
        for es in &env_strings {
            tmux_cmd.arg("-e").arg(es);
        }
        tmux_cmd.args(["-s", &session_name, "-c", &worktree_path.to_string_lossy()]);
        let tmux_output = tmux_cmd
            .output()
            .context("Failed to run tmux new-session")?;

        if !tmux_output.status.success() {
            let stderr = String::from_utf8_lossy(&tmux_output.stderr);
            bail!(
                "Failed to create tmux session '{}': {}",
                session_name,
                stderr.trim()
            );
        }
        tmux_session_created = true;
        println!("  Created tmux session '{}'", session_name);

        // 9. Register in worktrees.json (reuse state loaded at step 1)
        state.add(WorktreeEntry {
            project_key: project.to_string(),
            branch: branch.to_string(),
            worktree_type: wt_type.to_string(),
            path: worktree_path.to_string_lossy().to_string(),
            session_name: session_name.clone(),
            metadata: metadata.clone(),
            created_at: chrono::Utc::now().to_rfc3339(),
        });
        state.save()?;

        // 10. Enable auto-approve if requested
        if auto_approve {
            let mut sessions = load_auto_approve_sessions();
            sessions.insert(session_name.clone());
            save_auto_approve_sessions(&sessions);
            println!("  Enabled auto-approve for '{}'", session_name);
        }

        // 11. post-setup hook
        run_hook(&hooks_dir, "post-setup", &env, &metadata)?;

        // 12. Run startup command (append prompt as CLI argument if provided)
        //     New worktrees have no conversation to continue, so strip `-c` from claude commands.
        //     Skip if --no-startup was passed (e.g., for fork-to-worktree).
        if no_startup {
            // Don't run startup command — caller will start their own process
        } else if let Some(ref cmd) = config.startup_command {
            let base_cmd = cmd.replace("claude -c", "claude");
            let full_cmd = match prompt {
                // Single-quoted, not `{:?}`: Rust's debug quoting is DOUBLE quotes,
                // inside which the shell still expands `$(…)` and backticks — so a
                // prompt from a todo could run a command on the way in.
                Some(p) => format!("{} {}", base_cmd, crate::common::tmux::sh_quote(p)),
                None => base_cmd,
            };
            let _ = std::process::Command::new("tmux")
                .args([
                    "send-keys",
                    "-t",
                    &crate::common::tmux::exact_active_pane(&session_name),
                    &full_cmd,
                    "Enter",
                ])
                .output();
        }

        Ok(())
    })();

    if let Err(e) = result {
        eprintln!("  Error during worktree setup, cleaning up...");
        if tmux_session_created {
            kill_tmux_session(&session_name);
        }
        let _ = delete_git_worktree(&project_root, &worktree_path, branch, true, true);
        return Err(e);
    }

    // Branching off a project is work starting there: put it back on the Browse
    // list if it had been archived (same rule as starting a conversation in it).
    activate_project(project);

    println!("Ready: session '{}'", session_name);

    Ok(session_name)
}

/// Delete a worktree: full 7-step workflow with hooks
pub fn run_wt_delete(project: &str, branch: &str, keep_branch: bool, force: bool) -> Result<()> {
    // 1. Look up entry in worktrees.json
    let state = WorktreeState::load();
    let entry = state
        .get(project, branch)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Worktree '{}/{}' not found in registry. Use 'hive wt list' to see registered worktrees.",
                project,
                branch
            )
        })?
        .clone();

    let registry = ProjectRegistry::load();
    let config = registry
        .projects
        .get(project)
        .ok_or_else(|| anyhow::anyhow!("Project '{}' not found in registry", project))?;

    let project_root = expand_tilde(&config.project_root);
    let worktree_path = std::path::PathBuf::from(&entry.path);

    // 2. Confirmation prompt
    if !force {
        println!(
            "Delete worktree '{}/{}' (session: '{}', path: {})?",
            project, branch, entry.session_name, entry.path
        );
        print!("[y/N] ");
        std::io::Write::flush(&mut std::io::stdout()).ok();

        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        let input = input.trim().to_lowercase();
        if input != "y" && input != "yes" {
            println!("Aborted.");
            return Ok(());
        }
    }

    let hooks_dir = resolve_hooks_dir(config, project);
    let env = build_hook_env(
        project,
        branch,
        &worktree_path,
        &project_root,
        &entry.session_name,
        &entry.worktree_type,
        &hooks_dir,
    );

    // 3. pre-delete hook (receives full metadata)
    let _ = run_hook(&hooks_dir, "pre-delete", &env, &entry.metadata)?;

    // 4. Kill tmux session
    if kill_tmux_session(&entry.session_name) {
        println!("  Killed tmux session '{}'", entry.session_name);
    }

    // 5. git worktree remove + optionally delete branch
    let git_removed =
        match delete_git_worktree(&project_root, &worktree_path, branch, keep_branch, force) {
            Ok(()) => {
                println!("  Removed git worktree");
                true
            }
            Err(e) => {
                eprintln!("  Warning: git worktree removal failed: {}", e);
                false
            }
        };

    // 6. Remove from worktrees.json only if git worktree was actually removed
    if git_removed {
        let mut state = WorktreeState::load();
        state.remove(project, branch);
        state.save()?;
        // Its session is gone for good; its todos would otherwise linger unclaimed.
        crate::cli::project::print_dropped_todos(&crate::common::todos::drop_session_todos(
            std::slice::from_ref(&entry.session_name),
        ));
    } else {
        eprintln!("  Keeping registry entry (git worktree still exists on disk)");
    }

    // 7. post-delete hook
    if git_removed {
        let _ = run_hook(&hooks_dir, "post-delete", &env, &entry.metadata)?;
        println!("Deleted worktree '{}/{}'", project, branch);
    } else {
        eprintln!(
            "Partial delete: hooks ran and tmux killed, but worktree remains at {}",
            entry.path
        );
    }
    Ok(())
}

/// List worktrees with tmux session status
pub fn run_wt_list(project: Option<&str>) -> Result<()> {
    let state = WorktreeState::load();
    let tmux_sessions = get_current_tmux_session_names();

    let mut entries: Vec<_> = if let Some(proj) = project {
        state
            .worktrees
            .values()
            .filter(|e| e.project_key == proj)
            .collect()
    } else {
        state.worktrees.values().collect()
    };

    if entries.is_empty() {
        if let Some(proj) = project {
            println!("No worktrees registered for project '{}'.", proj);
        } else {
            println!("No worktrees registered. Use 'hive wt new' to create one.");
        }
        return Ok(());
    }

    // Sort by project then branch
    entries.sort_by(|a, b| {
        a.project_key
            .cmp(&b.project_key)
            .then(a.branch.cmp(&b.branch))
    });

    // Calculate column widths
    let max_key = entries
        .iter()
        .map(|e| WorktreeState::make_key(&e.project_key, &e.branch).len())
        .max()
        .unwrap_or(0);
    let max_session = entries
        .iter()
        .map(|e| e.session_name.len())
        .max()
        .unwrap_or(0);

    println!(
        "{:<width_k$}  {:<width_s$}  STATUS  PATH",
        "WORKTREE",
        "SESSION",
        width_k = max_key,
        width_s = max_session
    );
    println!(
        "{:<width_k$}  {:<width_s$}  ------  ----",
        "-".repeat(max_key),
        "-".repeat(max_session),
        width_k = max_key,
        width_s = max_session
    );

    for entry in &entries {
        let wt_key = WorktreeState::make_key(&entry.project_key, &entry.branch);
        let status = if tmux_sessions.contains(&entry.session_name) {
            "active"
        } else {
            "dead"
        };
        println!(
            "{:<width_k$}  {:<width_s$}  {:<6}  {}",
            wt_key,
            entry.session_name,
            status,
            entry.path,
            width_k = max_key,
            width_s = max_session
        );
    }

    println!("\n{} worktree(s)", entries.len());
    Ok(())
}

/// Import existing git worktrees into worktrees.json
pub fn run_wt_import(project: &str) -> Result<()> {
    let registry = ProjectRegistry::load();
    let config = registry
        .projects
        .get(project)
        .ok_or_else(|| anyhow::anyhow!("Project '{}' not found in registry", project))?;

    let worktrees_dir = registry
        .resolve_worktrees_dir(project, config)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "No worktrees directory configured for '{}'. Set 'worktrees_dir' on the project or 'worktrees_root' globally.",
                project
            )
        })?;

    let tmux_sessions = get_current_tmux_session_names();

    println!("Scanning git worktrees for '{}'...", project);
    let imported = import_worktrees(project, config, &worktrees_dir, &tmux_sessions)?;

    if imported.is_empty() {
        println!("No new worktrees found to import.");
    } else {
        for entry in &imported {
            println!(
                "  + {}/{} → session '{}'",
                entry.project_key, entry.branch, entry.session_name
            );
        }
        println!("\nImported {} worktree(s)", imported.len());
    }

    Ok(())
}

/// One repo's share of a prune: what was found and what will happen to each.
struct PrunePlan {
    label: String,
    repo: String,
    worktrees: Vec<(
        crate::common::prune::GitWorktree,
        crate::common::prune::Verdict,
    )>,
    branches: Vec<(String, crate::common::prune::Verdict)>,
    /// Worktree records whose folder is already gone (`git worktree prune` drops them).
    stale: usize,
}

/// Remove what Claude's background Workflows leave behind — agent worktrees under
/// `.claude/worktrees/` and their `worktree-wf_*` branches — across every registered
/// project (or one). See `common::prune` for the rules; nothing that exists only
/// locally is ever removed.
pub fn run_wt_prune(
    project: Option<&str>,
    extra_patterns: &[String],
    idle_hours: u64,
    dry_run: bool,
    yes: bool,
) -> Result<()> {
    use crate::common::prune::*;
    use std::collections::HashSet;
    use std::time::{Duration, SystemTime};

    let registry = ProjectRegistry::load();
    let mut keys: Vec<&String> = match project {
        Some(p) => vec![
            registry
                .projects
                .get_key_value(p)
                .ok_or_else(|| anyhow::anyhow!("Project '{}' not found in registry", p))?
                .0,
        ],
        None => registry.projects.keys().collect(),
    };
    keys.sort();

    let patterns: Vec<String> = DEFAULT_BRANCH_PATTERNS
        .iter()
        .map(|s| s.to_string())
        .chain(extra_patterns.iter().cloned())
        .collect();
    let now = SystemTime::now();
    let min_idle = Duration::from_secs(idle_hours * 3600);

    // Several projects can share one repo (a project registered at a subdirectory),
    // so plans are per git common dir, not per project.
    let mut seen = HashSet::new();
    let mut plans = Vec::new();
    for key in keys {
        let config = &registry.projects[key];
        let repo = expand_tilde(&config.project_root)
            .to_string_lossy()
            .to_string();
        let Some(common) = common_dir(&repo) else {
            continue;
        };
        if !seen.insert(common) {
            continue;
        }

        let all = list_worktrees(&repo);
        let stale = all.iter().filter(|w| w.prunable).count();
        let worktrees: Vec<_> = all
            .iter()
            .filter(|w| !w.prunable && is_agent_worktree(&w.path))
            .map(|w| {
                (
                    w.clone(),
                    worktree_verdict(&worktree_facts(w, now), min_idle),
                )
            })
            .collect();
        // Branches still checked out once the planned removals are done.
        let checked_out: HashSet<&str> = all
            .iter()
            .filter(|w| !w.prunable)
            .filter(|w| {
                !worktrees
                    .iter()
                    .any(|(r, v)| r.path == w.path && *v == Verdict::Remove)
            })
            .filter_map(|w| w.branch.as_deref())
            .collect();
        let branches: Vec<_> = matching_branches(&repo, &patterns)
            .into_iter()
            .map(|b| {
                let v =
                    branch_verdict(checked_out.contains(b.as_str()), branch_unpushed(&repo, &b));
                (b, v)
            })
            .collect();

        if worktrees.is_empty() && branches.is_empty() && stale == 0 {
            continue;
        }
        plans.push(PrunePlan {
            label: format!("{} {}", config.emoji, key),
            repo,
            worktrees,
            branches,
            stale,
        });
    }

    let removable = |v: &Verdict| *v == Verdict::Remove;
    let (mut n_wt, mut n_br, mut n_stale) = (0, 0, 0);
    for p in &plans {
        println!("{}  ({})", p.label, p.repo);
        for (w, v) in &p.worktrees {
            let rel = w
                .path
                .strip_prefix(&format!("{}/", p.repo))
                .unwrap_or(&w.path);
            match v {
                Verdict::Remove => println!("  remove  {}", rel),
                Verdict::Keep(why) => println!("  keep    {}  — {}", rel, why),
            }
        }
        let doomed: Vec<_> = p.branches.iter().filter(|(_, v)| removable(v)).collect();
        if !p.branches.is_empty() {
            println!(
                "  {} branch(es) matching {} — {} to delete",
                p.branches.len(),
                patterns.join(", "),
                doomed.len()
            );
        }
        // Listing 170 deletions helps no one; the kept ones and their reasons do.
        if doomed.len() <= 10 {
            for (b, _) in &doomed {
                println!("  delete  {}", b);
            }
        }
        for (b, v) in &p.branches {
            if let Verdict::Keep(why) = v {
                println!("  keep    {}  — {}", b, why);
            }
        }
        if p.stale > 0 {
            println!(
                "  {} stale worktree record(s) (folder already gone)",
                p.stale
            );
        }
        println!();
        n_wt += p.worktrees.iter().filter(|(_, v)| removable(v)).count();
        n_br += doomed.len();
        n_stale += p.stale;
    }

    if n_wt + n_br + n_stale == 0 {
        println!("Nothing to prune.");
        return Ok(());
    }
    println!(
        "Will remove {} agent worktree(s), delete {} branch(es) and drop {} stale record(s).",
        n_wt, n_br, n_stale
    );
    if dry_run {
        println!("(dry run — nothing changed)");
        return Ok(());
    }
    if !yes {
        print!("Proceed? [y/N] ");
        std::io::Write::flush(&mut std::io::stdout()).ok();
        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        if !matches!(input.trim().to_lowercase().as_str(), "y" | "yes") {
            println!("Aborted.");
            return Ok(());
        }
    }

    let mut failures = 0;
    for p in &plans {
        let mut fail = |what: &str, e: anyhow::Error| {
            eprintln!("  {}: {} — {}", p.label, what, e);
            failures += 1;
        };
        if p.stale > 0 {
            if let Err(e) = git_run(&p.repo, &["worktree", "prune"]) {
                fail("worktree prune", e);
            }
        }
        for (w, v) in &p.worktrees {
            // No --force: if the tree changed since the plan, git refuses, and we keep it.
            if removable(v) {
                if let Err(e) = git_run(&p.repo, &["worktree", "remove", &w.path]) {
                    fail(&w.path, e);
                }
            }
        }
        for (b, v) in &p.branches {
            // -D, not -d: -d judges "merged" against the current HEAD, while the
            // check that matters — every commit is on a remote — was made above.
            if removable(v) {
                if let Err(e) = git_run(&p.repo, &["branch", "-D", b]) {
                    fail(b, e);
                }
            }
        }
    }
    if failures == 0 {
        println!("Done.");
    } else {
        println!("Done, with {} failure(s) left in place.", failures);
    }
    Ok(())
}
