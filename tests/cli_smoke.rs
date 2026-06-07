//! CLI smoke tests — verify the binary parses args correctly and dispatches
//! to the right handler. These test the public contract that users rely on,
//! catching any breakage from file reorganization or refactoring.
//!
//! These tests run the actual compiled binary, so they don't need tmux.
//! They only test commands that are safe to run without side effects.

use std::process::Command;

fn hive_cmd() -> Command {
    Command::new(env!("CARGO_BIN_EXE_hive"))
}

// --- Version & Help ---

#[test]
fn version_exits_zero() {
    let output = hive_cmd().arg("--version").output().unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.starts_with("hive "));
}

#[test]
fn help_exits_zero() {
    let output = hive_cmd().arg("--help").output().unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Interactive Claude Code session dashboard"));
}

#[test]
fn help_lists_all_subcommands() {
    let output = hive_cmd().arg("--help").output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    for cmd in [
        "hook", "setup", "uninstall", "connect", "project", "todo", "wt", "spread", "collapse",
        "start", "web", "update",
    ] {
        assert!(
            stdout.contains(cmd),
            "Missing subcommand '{}' in --help output",
            cmd
        );
    }
}

// --- Subcommand help pages ---

#[test]
fn hook_help_exits_zero() {
    let output = hive_cmd().args(["hook", "--help"]).output().unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("hook"));
}

#[test]
fn setup_help_exits_zero() {
    let output = hive_cmd().args(["setup", "--help"]).output().unwrap();
    assert!(output.status.success());
}

#[test]
fn project_help_exits_zero() {
    let output = hive_cmd().args(["project", "--help"]).output().unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    for sub in ["add", "remove", "archive", "unarchive", "list", "import"] {
        assert!(
            stdout.contains(sub),
            "Missing project subcommand '{}'",
            sub
        );
    }
}

#[test]
fn wt_help_exits_zero() {
    let output = hive_cmd().args(["wt", "--help"]).output().unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    for sub in ["new", "delete", "list", "import"] {
        assert!(stdout.contains(sub), "Missing wt subcommand '{}'", sub);
    }
}

#[test]
fn todo_help_exits_zero() {
    let output = hive_cmd().args(["todo", "--help"]).output().unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    for sub in ["list", "next", "add", "done", "clear"] {
        assert!(stdout.contains(sub), "Missing todo subcommand '{}'", sub);
    }
}

#[test]
fn web_help_exits_zero() {
    let output = hive_cmd().args(["web", "--help"]).output().unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("--port"));
    assert!(stdout.contains("--dev"));
    assert!(stdout.contains("--tts-host"));
}

// --- Read-only commands (no side effects) ---

#[test]
fn project_list_exits_zero() {
    let output = hive_cmd().args(["project", "list"]).output().unwrap();
    assert!(output.status.success());
}

#[test]
fn wt_list_exits_zero() {
    let output = hive_cmd().args(["wt", "list"]).output().unwrap();
    assert!(output.status.success());
}

#[test]
fn todo_list_exits_zero() {
    let output = hive_cmd()
        .args(["todo", "list", "--session", "nonexistent-test-session"])
        .output()
        .unwrap();
    assert!(output.status.success());
}

#[test]
fn todo_list_done_exits_zero() {
    let output = hive_cmd()
        .args(["todo", "list", "--done", "--session", "nonexistent-test-session"])
        .output()
        .unwrap();
    assert!(output.status.success());
}

#[test]
fn todo_next_exits_one_when_empty() {
    let output = hive_cmd()
        .args(["todo", "next", "--session", "nonexistent-test-session"])
        .output()
        .unwrap();
    // exit 1 is expected when there are no todos
    assert_eq!(output.status.code(), Some(1));
}

// --- Invalid args ---

#[test]
fn unknown_subcommand_exits_nonzero() {
    let output = hive_cmd().arg("nonexistent-command").output().unwrap();
    assert!(!output.status.success());
}

#[test]
fn hook_missing_event_arg_exits_nonzero() {
    let output = hive_cmd().arg("hook").output().unwrap();
    assert!(!output.status.success());
}

#[test]
fn project_add_missing_args_exits_nonzero() {
    let output = hive_cmd().args(["project", "add"]).output().unwrap();
    assert!(!output.status.success());
}

#[test]
fn wt_new_missing_args_exits_nonzero() {
    let output = hive_cmd().args(["wt", "new"]).output().unwrap();
    assert!(!output.status.success());
}

#[test]
fn connect_missing_key_exits_nonzero() {
    let output = hive_cmd().arg("connect").output().unwrap();
    assert!(!output.status.success());
}

// --- Todo roundtrip (write + read + cleanup) ---

#[test]
fn todo_add_and_done_roundtrip() {
    let session = "hive-smoke-test-session";

    // Add a todo
    let output = hive_cmd()
        .args(["todo", "add", "smoke test item", "--session", session])
        .output()
        .unwrap();
    assert!(output.status.success());

    // Verify it appears in list
    let output = hive_cmd()
        .args(["todo", "list", "--session", session])
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("smoke test item"),
        "Todo not found in list: {}",
        stdout
    );

    // Verify next returns it
    let output = hive_cmd()
        .args(["todo", "next", "--session", session])
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("smoke test item"));

    // Mark done
    let output = hive_cmd()
        .args(["todo", "done", "--session", session])
        .output()
        .unwrap();
    assert!(output.status.success());

    // Verify it moved to done list
    let output = hive_cmd()
        .args(["todo", "list", "--done", "--session", session])
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("smoke test item"),
        "Todo not found in done list: {}",
        stdout
    );

    // Clear completed
    let output = hive_cmd()
        .args(["todo", "clear", "--session", session])
        .output()
        .unwrap();
    assert!(output.status.success());

    // Verify next returns exit 1 (empty)
    let output = hive_cmd()
        .args(["todo", "next", "--session", session])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
}

// --- Project archive roundtrip (isolated HOME so the real registry is untouched) ---

#[test]
fn project_archive_roundtrip() {
    // Isolate ~/.hive by pointing HOME at a temp dir; dirs::home_dir() honors $HOME.
    let home = std::env::temp_dir().join(format!("hive-smoke-archive-{}", std::process::id()));
    std::fs::create_dir_all(&home).unwrap();

    let run = |args: &[&str]| {
        hive_cmd().args(args).env("HOME", &home).output().unwrap()
    };

    // Add a project to the isolated registry.
    let output = run(&["project", "add", "demo", "--emoji", "🧪", "--path", "/tmp/demo"]);
    assert!(output.status.success());

    // Archive it.
    let output = run(&["project", "archive", "demo"]);
    assert!(output.status.success());

    // Default list hides it.
    let output = run(&["project", "list"]);
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.contains("demo"), "Archived project should be hidden: {}", stdout);

    // --all shows it, marked.
    let output = run(&["project", "list", "--all"]);
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("demo"), "--all should show archived: {}", stdout);
    assert!(stdout.contains("[archived]"), "--all should mark archived: {}", stdout);

    // Persisted as archived = true.
    let toml = std::fs::read_to_string(home.join(".hive/projects.toml")).unwrap();
    assert!(toml.contains("archived = true"), "TOML missing archived flag: {}", toml);

    // Unarchive removes the flag and the project reappears in the default list.
    let output = run(&["project", "unarchive", "demo"]);
    assert!(output.status.success());
    let output = run(&["project", "list"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("demo"), "Unarchived project should reappear: {}", stdout);
    let toml = std::fs::read_to_string(home.join(".hive/projects.toml")).unwrap();
    assert!(!toml.contains("archived"), "archived flag should be gone: {}", toml);

    // Archiving a missing key fails.
    let output = run(&["project", "archive", "does-not-exist"]);
    assert!(!output.status.success());

    std::fs::remove_dir_all(&home).ok();
}
