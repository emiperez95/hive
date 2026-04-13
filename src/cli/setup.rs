//! Setup and uninstall commands.

use anyhow::Result;

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

/// Setup hooks in ~/.claude/settings.json
pub fn run_setup() -> Result<()> {
    use std::fs;

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

    // Read existing settings
    let settings: serde_json::Value = if settings_path.exists() {
        let content = fs::read_to_string(&settings_path)?;
        serde_json::from_str(&content)?
    } else {
        serde_json::json!({})
    };

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

        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        let input = input.trim().to_lowercase();
        if !input.is_empty() && input != "y" && input != "yes" {
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

            // Ensure .claude directory exists
            if let Some(parent) = settings_path.parent() {
                fs::create_dir_all(parent)?;
            }

            let content = serde_json::to_string_pretty(&settings)?;
            fs::write(&settings_path, content)?;

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

        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        let input = input.trim().to_lowercase();
        if input.is_empty() || input == "y" || input == "yes" {
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
                match std::process::Command::new("tmux").args(&args).status() {
                    Ok(s) if s.success() => registered.push(*key),
                    _ => failed.push(*key),
                }
            }

            if !registered.is_empty() {
                println!("Tmux keybindings registered (current session only).");
                println!("Add to ~/.tmux.conf to persist:");
                for (table, key, cmd, bound) in &bindings {
                    if *bound || !registered.contains(key) {
                        continue;
                    }
                    if *table == "root" {
                        println!("  bind-key -n {} {}", key, cmd);
                    } else {
                        println!("  bind-key {} {}", key, cmd);
                    }
                }
            }
            if !failed.is_empty() {
                println!("Could not register some keybindings (tmux not running?).");
            }
        } else {
            println!("Skipped tmux keybindings.");
        }
    }

    // Install janus-wt-portal agent
    if agent_needs_install {
        println!();
        println!("Install janus-wt-portal agent to ~/.claude/agents/?");
        print!("[Y/n] ");
        std::io::Write::flush(&mut std::io::stdout()).ok();

        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        let input = input.trim().to_lowercase();
        if input.is_empty() || input == "y" || input == "yes" {
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
        println!("Install create-project command to ~/.claude/commands/hive/?");
        print!("[Y/n] ");
        std::io::Write::flush(&mut std::io::stdout()).ok();

        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        let input = input.trim().to_lowercase();
        if input.is_empty() || input == "y" || input == "yes" {
            if let Some(parent) = cmd_dest.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&cmd_dest, CREATE_PROJECT_CMD_CONTENT)?;
            println!("Command installed to {:?}", cmd_dest);
        } else {
            println!("Skipped command installation.");
        }
    }

    Ok(())
}

/// Remove hive hooks from ~/.claude/settings.json
pub fn run_uninstall() -> Result<()> {
    use std::fs;

    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("Cannot find home directory"))?;
    let settings_path = home.join(".claude").join("settings.json");

    if !settings_path.exists() {
        println!(
            "No settings file found at {:?}, nothing to do.",
            settings_path
        );
        return Ok(());
    }

    let mut settings: serde_json::Value = {
        let content = fs::read_to_string(&settings_path)?;
        serde_json::from_str(&content)?
    };

    let hooks_map = match settings.get_mut("hooks").and_then(|h| h.as_object_mut()) {
        Some(map) => map,
        None => {
            println!("No hooks found in settings, nothing to do.");
            return Ok(());
        }
    };

    // Find which events have hive hooks
    let mut found_events: Vec<String> = Vec::new();
    for (event, groups) in hooks_map.iter() {
        if let Some(arr) = groups.as_array() {
            for group in arr {
                if let Some(hooks) = group.get("hooks").and_then(|h| h.as_array()) {
                    for hook in hooks {
                        if let Some(cmd) = hook.get("command").and_then(|c| c.as_str()) {
                            if is_hive_hook_command(cmd) {
                                found_events.push(event.clone());
                                break;
                            }
                        }
                    }
                }
            }
        }
    }

    if found_events.is_empty() {
        println!("No hive hooks found in settings, nothing to do.");
        return Ok(());
    }

    found_events.sort();
    found_events.dedup();

    println!("Found hive hooks for the following events:");
    for event in &found_events {
        println!("  {}", event);
    }
    println!();
    println!("Other hooks will be preserved.");
    print!("Remove hive hooks? [Y/n] ");
    std::io::Write::flush(&mut std::io::stdout()).ok();

    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    let input = input.trim().to_lowercase();
    if !input.is_empty() && input != "y" && input != "yes" {
        println!("Aborted.");
        return Ok(());
    }

    // Remove hive hooks from all groups
    for (_event, groups) in hooks_map.iter_mut() {
        if let Some(arr) = groups.as_array_mut() {
            for group in arr.iter_mut() {
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
            arr.retain(|group| {
                group
                    .get("hooks")
                    .and_then(|h| h.as_array())
                    .map(|a| !a.is_empty())
                    .unwrap_or(true)
            });
        }
    }

    // Remove event keys that now have empty arrays
    hooks_map.retain(|_, groups| groups.as_array().map(|a| !a.is_empty()).unwrap_or(true));

    let content = serde_json::to_string_pretty(&settings)?;
    fs::write(&settings_path, content)?;

    println!("Hive hooks removed from {:?}", settings_path);

    // Offer to unbind tmux keys
    println!();
    println!("Unbind tmux keybindings (prefix+s, prefix+d, Ctrl+n, Ctrl+p)?");
    print!("[Y/n] ");
    std::io::Write::flush(&mut std::io::stdout()).ok();

    let mut input2 = String::new();
    std::io::stdin().read_line(&mut input2)?;
    let input2 = input2.trim().to_lowercase();
    if input2.is_empty() || input2 == "y" || input2 == "yes" {
        for key in &["s", "d"] {
            let _ = std::process::Command::new("tmux")
                .args(["unbind-key", key])
                .status();
        }
        for key in &["C-n", "C-p"] {
            let _ = std::process::Command::new("tmux")
                .args(["unbind-key", "-n", key])
                .status();
        }
        println!("Tmux keybindings unbound (current session only).");
        println!("Remove from ~/.tmux.conf manually if present.");
    } else {
        println!("Skipped tmux keybinding removal.");
    }

    // Remove janus-wt-portal agent
    let agent_path = home
        .join(".claude")
        .join("agents")
        .join("janus-wt-portal.md");
    if agent_path.exists() {
        println!();
        print!("Remove janus-wt-portal agent? [Y/n] ");
        std::io::Write::flush(&mut std::io::stdout()).ok();

        let mut input3 = String::new();
        std::io::stdin().read_line(&mut input3)?;
        let input3 = input3.trim().to_lowercase();
        if input3.is_empty() || input3 == "y" || input3 == "yes" {
            fs::remove_file(&agent_path)?;
            println!("Removed {:?}", agent_path);
        } else {
            println!("Skipped agent removal.");
        }
    }

    Ok(())
}
