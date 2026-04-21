//! Setup and uninstall commands.

use anyhow::{bail, Result};

/// Bundled janus-wt-portal agent definition, embedded at compile time.
const JANUS_AGENT_CONTENT: &str = include_str!("../../.claude/agents/janus-wt-portal.md");

/// Bundled create-project command, embedded at compile time.
const CREATE_PROJECT_CMD_CONTENT: &str =
    include_str!("../../.claude/commands/hive/create-project.md");

/// Check if a hook command belongs to hive
pub(crate) fn is_hive_hook_command(cmd: &str) -> bool {
    let is_hive_event = cmd.ends_with(" hook Stop")
        || cmd.ends_with(" hook PreToolUse")
        || cmd.ends_with(" hook PostToolUse")
        || cmd.ends_with(" hook UserPromptSubmit")
        || cmd.ends_with(" hook PermissionRequest")
        || cmd.ends_with(" hook Notification");
    is_hive_event && (cmd.contains("/hive hook ") || cmd.starts_with("hive hook "))
}

/// Check whether `cmd` is available on PATH.
fn has_command(cmd: &str) -> bool {
    std::process::Command::new("which")
        .arg(cmd)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Warn about missing external dependencies. Setup is safe to run
/// without them (it's idempotent; the user can install and re-run),
/// but we want them to see the problem now rather than discover it
/// later when tmux keybindings silently no-op or no hooks ever fire.
fn warn_missing_deps() {
    let tmux_ok = has_command("tmux");
    let claude_ok = has_command("claude");
    if tmux_ok && claude_ok {
        return;
    }
    println!("Warning: some required tools are missing from PATH.");
    if !tmux_ok {
        println!("  tmux not found — install with `brew install tmux` (hive uses tmux for sessions).");
    }
    if !claude_ok {
        println!(
            "  claude CLI not found — see https://docs.claude.com/en/docs/claude-code/overview"
        );
    }
    println!("Setup will continue, but these need to be installed before hive works end-to-end.");
    println!();
}

/// Read a Y/n answer from stdin, or auto-accept when `yes` is set.
/// The caller is expected to have already printed the question + `[Y/n] `.
fn read_yn(yes: bool) -> Result<bool> {
    if yes {
        println!("[auto-yes]");
        return Ok(true);
    }
    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    let input = input.trim().to_lowercase();
    Ok(input.is_empty() || input == "y" || input == "yes")
}

/// Parse ~/.claude/settings.json as JSON. If the file is malformed, move
/// it aside to `.bak.malformed.<timestamp>` and return a user-facing error
/// that points at the backup — never silently corrupts user state.
fn load_settings(path: &std::path::Path) -> Result<serde_json::Value> {
    use std::fs;

    if !path.exists() {
        return Ok(serde_json::json!({}));
    }
    let content = fs::read_to_string(path)?;
    match serde_json::from_str(&content) {
        Ok(v) => Ok(v),
        Err(e) => {
            let ts = chrono::Local::now().format("%Y%m%d-%H%M%S");
            let backup = path.with_extension(format!("json.bak.malformed.{}", ts));
            fs::rename(path, &backup)?;
            bail!(
                "settings.json was malformed ({}). Backed up to {}. Re-run `hive setup` to write a clean one.",
                e,
                backup.display()
            );
        }
    }
}

/// Write settings.json atomically (write `.tmp`, rename). Creates a
/// sibling `.bak` of the prior contents if the file existed.
fn save_settings(path: &std::path::Path, settings: &serde_json::Value) -> Result<()> {
    use std::fs;

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    if path.exists() {
        let backup = path.with_extension("json.bak");
        fs::copy(path, &backup)?;
    }
    let tmp = path.with_extension("json.tmp");
    let content = serde_json::to_string_pretty(settings)?;
    fs::write(&tmp, content)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

/// Setup hooks in ~/.claude/settings.json
pub fn run_setup(yes: bool) -> Result<()> {
    use std::fs;

    warn_missing_deps();

    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("Cannot find home directory"))?;
    let settings_path = home.join(".claude").join("settings.json");

    // Find the binary path: prefer installed location, fall back to current exe
    let installed_path = home.join(".local").join("bin").join("hive");
    let binary_path = if installed_path.exists() {
        installed_path
    } else {
        std::env::current_exe().unwrap_or_else(|_| installed_path.clone())
    };

    let binary_str = binary_path.to_string_lossy();

    // Read existing settings (malformed files are safely moved aside).
    let settings = load_settings(&settings_path)?;

    let hook_events = [
        "Stop",
        "PreToolUse",
        "PostToolUse",
        "UserPromptSubmit",
        "PermissionRequest",
    ];

    // Check which hooks are already installed (with correct binary path)
    let mut hooks_missing: Vec<&str> = Vec::new();
    let mut hooks_stale: Vec<&str> = Vec::new(); // installed but wrong binary path
    let mut hooks_ok: Vec<&str> = Vec::new();

    if let Some(hooks_obj) = settings.get("hooks").and_then(|h| h.as_object()) {
        for event in &hook_events {
            let expected_cmd = format!("{} hook {}", binary_str, event);
            let mut found_exact = false;
            let mut found_other_hive = false;

            if let Some(groups) = hooks_obj.get(*event).and_then(|g| g.as_array()) {
                for group in groups {
                    if let Some(hooks) = group.get("hooks").and_then(|h| h.as_array()) {
                        for hook in hooks {
                            if let Some(cmd) = hook.get("command").and_then(|c| c.as_str()) {
                                if cmd == expected_cmd {
                                    found_exact = true;
                                } else if is_hive_hook_command(cmd) {
                                    found_other_hive = true;
                                }
                            }
                        }
                    }
                }
            }

            if found_exact {
                hooks_ok.push(event);
            } else if found_other_hive {
                hooks_stale.push(event);
            } else {
                hooks_missing.push(event);
            }
        }
    } else {
        hooks_missing.extend_from_slice(&hook_events);
    }

    // Check tmux keybindings
    let tmux_keys = std::process::Command::new("tmux")
        .args(["list-keys"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .unwrap_or_default();

    let tmux_s_bound = tmux_keys
        .lines()
        .any(|line| line.contains("prefix") && line.contains(" s ") && line.contains("hive"));
    let tmux_d_bound = tmux_keys.lines().any(|line| {
        line.contains("prefix")
            && line.contains(" d ")
            && line.contains("hive")
            && line.contains("--detail")
    });
    let tmux_cn_bound = tmux_keys
        .lines()
        .any(|line| line.contains("C-n") && line.contains("hive") && line.contains("cycle-next"));
    let tmux_cp_bound = tmux_keys
        .lines()
        .any(|line| line.contains("C-p") && line.contains("hive") && line.contains("cycle-prev"));

    // Report status
    println!("hive setup status:");
    println!();

    if !hooks_ok.is_empty() {
        for event in &hooks_ok {
            println!("  [ok]      {} hook", event);
        }
    }
    if !hooks_stale.is_empty() {
        for event in &hooks_stale {
            println!("  [update]  {} hook (different binary path)", event);
        }
    }
    if !hooks_missing.is_empty() {
        for event in &hooks_missing {
            println!("  [missing] {} hook", event);
        }
    }

    if tmux_s_bound {
        println!("  [ok]      tmux prefix+s keybinding (list)");
    } else {
        println!("  [missing] tmux prefix+s keybinding (list)");
    }
    if tmux_d_bound {
        println!("  [ok]      tmux prefix+d keybinding (detail)");
    } else {
        println!("  [missing] tmux prefix+d keybinding (detail)");
    }
    if tmux_cn_bound {
        println!("  [ok]      tmux Ctrl+n keybinding (cycle next)");
    } else {
        println!("  [missing] tmux Ctrl+n keybinding (cycle next)");
    }
    if tmux_cp_bound {
        println!("  [ok]      tmux Ctrl+p keybinding (cycle prev)");
    } else {
        println!("  [missing] tmux Ctrl+p keybinding (cycle prev)");
    }

    // Check janus-wt-portal agent
    let agent_dest = home
        .join(".claude")
        .join("agents")
        .join("janus-wt-portal.md");
    let agent_status = if agent_dest.exists() {
        let existing = fs::read_to_string(&agent_dest).unwrap_or_default();
        if existing == JANUS_AGENT_CONTENT {
            "ok"
        } else {
            "update"
        }
    } else {
        "missing"
    };

    match agent_status {
        "ok" => println!("  [ok]      janus-wt-portal agent"),
        "update" => println!("  [update]  janus-wt-portal agent (content differs)"),
        _ => println!("  [missing] janus-wt-portal agent"),
    }

    // Check create-project command
    let cmd_dest = home
        .join(".claude")
        .join("commands")
        .join("hive")
        .join("create-project.md");
    let cmd_status = if cmd_dest.exists() {
        let existing = fs::read_to_string(&cmd_dest).unwrap_or_default();
        if existing == CREATE_PROJECT_CMD_CONTENT {
            "ok"
        } else {
            "update"
        }
    } else {
        "missing"
    };

    match cmd_status {
        "ok" => println!("  [ok]      create-project command"),
        "update" => println!("  [update]  create-project command (content differs)"),
        _ => println!("  [missing] create-project command"),
    }

    let needs_hook_changes = !hooks_missing.is_empty() || !hooks_stale.is_empty();
    let all_tmux_bound = tmux_s_bound && tmux_d_bound && tmux_cn_bound && tmux_cp_bound;
    let agent_needs_install = agent_status != "ok";
    let cmd_needs_install = cmd_status != "ok";

    if !needs_hook_changes && all_tmux_bound && !agent_needs_install && !cmd_needs_install {
        println!();
        println!("Everything is already set up!");
        return Ok(());
    }

    // Install missing/stale hooks
    if needs_hook_changes {
        let events_to_install: Vec<&str> = hooks_missing
            .iter()
            .chain(hooks_stale.iter())
            .copied()
            .collect();

        println!();
        println!(
            "Install {} hook(s)? Existing hooks for other tools will be preserved.",
            events_to_install.len()
        );
        print!("[Y/n] ");
        std::io::Write::flush(&mut std::io::stdout()).ok();

        if !read_yn(yes)? {
            println!("Skipped hooks.");
        } else {
            let mut settings = settings.clone();
            let needs_matcher = ["PreToolUse", "PostToolUse", "PermissionRequest"];

            let hooks_obj = settings
                .as_object_mut()
                .ok_or_else(|| anyhow::anyhow!("settings.json is not a JSON object"))?
                .entry("hooks")
                .or_insert_with(|| serde_json::json!({}));

            let hooks_map = hooks_obj
                .as_object_mut()
                .ok_or_else(|| anyhow::anyhow!("hooks is not a JSON object"))?;

            for event in &events_to_install {
                let hive_command = format!("{} hook {}", binary_str, event);
                let hive_hook_entry = serde_json::json!({
                    "type": "command",
                    "command": hive_command
                });

                let event_array = hooks_map
                    .entry(event.to_string())
                    .or_insert_with(|| serde_json::json!([]));

                let groups = event_array
                    .as_array_mut()
                    .ok_or_else(|| anyhow::anyhow!("hooks.{} is not an array", event))?;

                // Remove any existing hive hooks from all groups
                for group in groups.iter_mut() {
                    if let Some(hooks_arr) = group.get_mut("hooks").and_then(|h| h.as_array_mut()) {
                        hooks_arr.retain(|h| {
                            h.get("command")
                                .and_then(|c| c.as_str())
                                .map(|c| !is_hive_hook_command(c))
                                .unwrap_or(true)
                        });
                    }
                }

                // Remove empty groups
                groups.retain(|group| {
                    group
                        .get("hooks")
                        .and_then(|h| h.as_array())
                        .map(|a| !a.is_empty())
                        .unwrap_or(true)
                });

                if needs_matcher.contains(event) {
                    let wildcard_group = groups.iter_mut().find(|g| {
                        g.get("matcher")
                            .and_then(|m| m.as_str())
                            .map(|m| m == "*")
                            .unwrap_or(false)
                    });

                    if let Some(group) = wildcard_group {
                        if let Some(hooks_arr) =
                            group.get_mut("hooks").and_then(|h| h.as_array_mut())
                        {
                            hooks_arr.push(hive_hook_entry);
                        }
                    } else {
                        groups.push(serde_json::json!({
                            "matcher": "*",
                            "hooks": [hive_hook_entry]
                        }));
                    }
                } else {
                    let no_matcher_group = groups.iter_mut().find(|g| g.get("matcher").is_none());

                    if let Some(group) = no_matcher_group {
                        if let Some(hooks_arr) =
                            group.get_mut("hooks").and_then(|h| h.as_array_mut())
                        {
                            hooks_arr.push(hive_hook_entry);
                        }
                    } else {
                        groups.push(serde_json::json!({
                            "hooks": [hive_hook_entry]
                        }));
                    }
                }
            }

            save_settings(&settings_path, &settings)?;

            println!("Hooks installed. Restart Claude Code sessions for hooks to take effect.");
        }
    }

    // Install tmux keybindings
    if !all_tmux_bound {
        let tmux_s_cmd = format!("display-popup -E -w 80% -h 70% \"{}\"", binary_str);
        let tmux_d_cmd = format!("display-popup -E -w 80% -h 70% \"{} --detail\"", binary_str);
        let tmux_cn_cmd = format!("run-shell \"{} cycle-next\"", binary_str);
        let tmux_cp_cmd = format!("run-shell \"{} cycle-prev\"", binary_str);

        let bindings: Vec<(&str, &str, &str, bool)> = vec![
            ("prefix", "s", &tmux_s_cmd, tmux_s_bound),
            ("prefix", "d", &tmux_d_cmd, tmux_d_bound),
            ("root", "C-n", &tmux_cn_cmd, tmux_cn_bound),
            ("root", "C-p", &tmux_cp_cmd, tmux_cp_bound),
        ];

        let missing: Vec<_> = bindings.iter().filter(|(_, _, _, bound)| !bound).collect();

        println!();
        println!("Register tmux keybindings?");
        for (table, key, _cmd, _) in &missing {
            let label = if *table == "root" {
                key.to_string()
            } else {
                format!("prefix+{}", key)
            };
            println!("  {} -> hive", label);
        }
        print!("[Y/n] ");
        std::io::Write::flush(&mut std::io::stdout()).ok();

        if read_yn(yes)? {
            let mut registered = Vec::new();
            let mut failed = Vec::new();

            for (table, key, cmd, bound) in &bindings {
                if *bound {
                    continue;
                }
                let args = if *table == "root" {
                    vec!["bind-key", "-n", key, cmd]
                } else {
                    vec!["bind-key", key, cmd]
                };
                // stderr is suppressed because tmux prints "no server
                // running on ..." once per invocation; we surface a
                // single friendly line from the `failed` branch below.
                match std::process::Command::new("tmux")
                    .args(&args)
                    .stderr(std::process::Stdio::null())
                    .status()
                {
                    Ok(s) if s.success() => registered.push(*key),
                    _ => failed.push(*key),
                }
            }

            if !registered.is_empty() {
                println!("Tmux keybindings registered (current session only).");
            }
            if !failed.is_empty() {
                println!("No running tmux server — keybindings not applied yet.");
            }

            // Always print the tmux.conf snippets for every non-bound
            // key, so the user can paste them into ~/.tmux.conf to
            // persist (or start tmux and re-run `hive setup`).
            let unbound: Vec<_> = bindings.iter().filter(|(_, _, _, b)| !*b).collect();
            if !unbound.is_empty() {
                println!("Add to ~/.tmux.conf to persist:");
                for (table, key, cmd, _) in &unbound {
                    if *table == "root" {
                        println!("  bind-key -n {} {}", key, cmd);
                    } else {
                        println!("  bind-key {} {}", key, cmd);
                    }
                }
            }
        } else {
            println!("Skipped tmux keybindings.");
        }
    }

    // Install janus-wt-portal agent
    if agent_needs_install {
        println!();
        print!("Install janus-wt-portal agent to ~/.claude/agents/? [Y/n] ");
        std::io::Write::flush(&mut std::io::stdout()).ok();

        if read_yn(yes)? {
            if let Some(parent) = agent_dest.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&agent_dest, JANUS_AGENT_CONTENT)?;
            println!("Agent installed to {:?}", agent_dest);
        } else {
            println!("Skipped agent installation.");
        }
    }

    // Install create-project command
    if cmd_needs_install {
        println!();
        print!("Install create-project command to ~/.claude/commands/hive/? [Y/n] ");
        std::io::Write::flush(&mut std::io::stdout()).ok();

        if read_yn(yes)? {
            if let Some(parent) = cmd_dest.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&cmd_dest, CREATE_PROJECT_CMD_CONTENT)?;
            println!("Command installed to {:?}", cmd_dest);
        } else {
            println!("Skipped command installation.");
        }
    }

    println!();
    println!("Setup complete. Try:");
    println!("  hive project add <key> --path <dir>   # register your first project");
    println!("  hive connect <key>                    # launch a tmux session");
    println!("  hive                                  # open the dashboard");

    Ok(())
}

/// Remove hive hooks from ~/.claude/settings.json and related files.
pub fn run_uninstall(yes: bool) -> Result<()> {
    use std::fs;

    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("Cannot find home directory"))?;
    let settings_path = home.join(".claude").join("settings.json");

    // --- Hooks in settings.json ---------------------------------------
    let mut hooks_action = "none"; // "removed" | "none-found" | "file-missing"
    if !settings_path.exists() {
        hooks_action = "file-missing";
        println!(
            "No settings file found at {:?}, skipping hook removal.",
            settings_path
        );
    } else {
        let mut settings = load_settings(&settings_path)?;

        let had_hive_hooks = {
            let mut found: Vec<String> = Vec::new();
            if let Some(map) = settings.get("hooks").and_then(|h| h.as_object()) {
                for (event, groups) in map.iter() {
                    if let Some(arr) = groups.as_array() {
                        for group in arr {
                            if let Some(hooks) = group.get("hooks").and_then(|h| h.as_array()) {
                                for hook in hooks {
                                    if let Some(cmd) = hook.get("command").and_then(|c| c.as_str())
                                    {
                                        if is_hive_hook_command(cmd) {
                                            found.push(event.clone());
                                            break;
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            found.sort();
            found.dedup();
            found
        };

        if had_hive_hooks.is_empty() {
            hooks_action = "none-found";
            println!("No hive hooks found in settings.json.");
        } else {
            println!("Found hive hooks for the following events:");
            for event in &had_hive_hooks {
                println!("  {}", event);
            }
            println!();
            println!("Other hooks will be preserved.");
            print!("Remove hive hooks? [Y/n] ");
            std::io::Write::flush(&mut std::io::stdout()).ok();

            if read_yn(yes)? {
                let hooks_map = settings
                    .get_mut("hooks")
                    .and_then(|h| h.as_object_mut())
                    .expect("checked above");
                for (_event, groups) in hooks_map.iter_mut() {
                    if let Some(arr) = groups.as_array_mut() {
                        for group in arr.iter_mut() {
                            if let Some(hooks_arr) =
                                group.get_mut("hooks").and_then(|h| h.as_array_mut())
                            {
                                hooks_arr.retain(|h| {
                                    h.get("command")
                                        .and_then(|c| c.as_str())
                                        .map(|c| !is_hive_hook_command(c))
                                        .unwrap_or(true)
                                });
                            }
                        }
                        arr.retain(|group| {
                            group
                                .get("hooks")
                                .and_then(|h| h.as_array())
                                .map(|a| !a.is_empty())
                                .unwrap_or(true)
                        });
                    }
                }
                hooks_map.retain(|_, groups| {
                    groups.as_array().map(|a| !a.is_empty()).unwrap_or(true)
                });

                save_settings(&settings_path, &settings)?;
                hooks_action = "removed";
                println!("Hive hooks removed from {:?}", settings_path);
            } else {
                println!("Skipped hook removal.");
            }
        }
    }

    // --- Tmux keybindings -------------------------------------------
    println!();
    print!("Unbind tmux keybindings (prefix+s, prefix+d, Ctrl+n, Ctrl+p)? [Y/n] ");
    std::io::Write::flush(&mut std::io::stdout()).ok();

    if read_yn(yes)? {
        for key in &["s", "d"] {
            let _ = std::process::Command::new("tmux")
                .args(["unbind-key", key])
                .stderr(std::process::Stdio::null())
                .status();
        }
        for key in &["C-n", "C-p"] {
            let _ = std::process::Command::new("tmux")
                .args(["unbind-key", "-n", key])
                .stderr(std::process::Stdio::null())
                .status();
        }
        println!("Tmux keybindings unbound (current session only).");
        println!("Remove from ~/.tmux.conf manually if present.");
    } else {
        println!("Skipped tmux keybinding removal.");
    }

    // --- janus-wt-portal agent --------------------------------------
    let agent_path = home
        .join(".claude")
        .join("agents")
        .join("janus-wt-portal.md");
    if agent_path.exists() {
        println!();
        print!("Remove janus-wt-portal agent? [Y/n] ");
        std::io::Write::flush(&mut std::io::stdout()).ok();

        if read_yn(yes)? {
            fs::remove_file(&agent_path)?;
            println!("Removed {:?}", agent_path);
            rmdir_if_empty(agent_path.parent());
        } else {
            println!("Skipped agent removal.");
        }
    }

    // --- create-project command -------------------------------------
    let cmd_path = home
        .join(".claude")
        .join("commands")
        .join("hive")
        .join("create-project.md");
    if cmd_path.exists() {
        println!();
        print!("Remove create-project command? [Y/n] ");
        std::io::Write::flush(&mut std::io::stdout()).ok();

        if read_yn(yes)? {
            fs::remove_file(&cmd_path)?;
            println!("Removed {:?}", cmd_path);
            rmdir_if_empty(cmd_path.parent());
        } else {
            println!("Skipped create-project command removal.");
        }
    }

    // --- Completion footer: what was left behind --------------------
    println!();
    println!("Uninstall complete.");
    let hive_dir = home.join(".hive");
    if hive_dir.exists() {
        println!("Your hive data is still at {}", hive_dir.display());
        println!("  Remove it with: rm -rf {}", hive_dir.display());
    }
    if let Ok(exe) = std::env::current_exe() {
        println!("The hive binary is at {}", exe.display());
        println!("  Remove it with: rm {}", exe.display());
    }
    let _ = hooks_action; // marker for future telemetry; kept to avoid unused-variable cleanup churn

    Ok(())
}

/// Remove a directory if it is empty. Ignores all errors (best-effort cleanup).
fn rmdir_if_empty(path: Option<&std::path::Path>) {
    let Some(path) = path else {
        return;
    };
    if let Ok(mut entries) = std::fs::read_dir(path) {
        if entries.next().is_none() {
            let _ = std::fs::remove_dir(path);
        }
    }
}
