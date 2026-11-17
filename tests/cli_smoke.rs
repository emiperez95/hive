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
fn conversations_command_exits_zero() {
    // Read-only registry listing. With an isolated empty HOME there is no state
    // to read, so it must still exit 0 and print its header.
    let home = std::env::temp_dir().join(format!("hive-smoke-sessions-{}", std::process::id()));
    std::fs::create_dir_all(&home).unwrap();
    let output = hive_cmd()
        .arg("conversations")
        .env("HOME", &home)
        .output()
        .unwrap();
    std::fs::remove_dir_all(&home).ok();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("hive conversations —"));
}

#[test]
fn help_lists_all_subcommands() {
    let output = hive_cmd().arg("--help").output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    for cmd in [
        "hook",
        "setup",
        "uninstall",
        "connect",
        "project",
        "todo",
        "wt",
        "spread",
        "collapse",
        "start",
        "web",
        "update",
        "conversations",
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
    for sub in ["add", "remove", "archive", "unarchive", "list"] {
        assert!(stdout.contains(sub), "Missing project subcommand '{}'", sub);
    }
}

#[test]
fn wt_help_exits_zero() {
    let output = hive_cmd().args(["wt", "--help"]).output().unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    for sub in ["new", "delete", "list", "import", "prune"] {
        assert!(stdout.contains(sub), "Missing wt subcommand '{}'", sub);
    }
}

/// `wt prune` over an empty registry: nothing to scan, nothing touched, exit 0.
/// Isolated HOME so the real registry's repos are never even read.
#[test]
fn wt_prune_with_no_projects_is_a_no_op() {
    let home = std::env::temp_dir().join(format!("hive-smoke-prune-{}", std::process::id()));
    std::fs::create_dir_all(&home).unwrap();
    let output = hive_cmd()
        .args(["wt", "prune", "--dry-run"])
        .env("HOME", &home)
        .output()
        .unwrap();
    std::fs::remove_dir_all(&home).ok();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("Nothing to prune."));
}

/// `wt new` switches you into the new session; `--no-switch` is the opt-out that
/// `/hive:fork-to-worktree` depends on, so the flag must stay on the help page.
#[test]
fn wt_new_help_lists_no_switch() {
    let output = hive_cmd().args(["wt", "new", "--help"]).output().unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("--no-switch"), "Missing --no-switch flag");
}

/// `task set` is the only way to edit a task's body; agents are told about these flags by
/// `/hive:tasks`, so they must stay on the help page.
#[test]
fn task_set_help_lists_the_body_flags() {
    let output = hive_cmd().args(["task", "set", "--help"]).output().unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    for flag in ["--body", "--append"] {
        assert!(stdout.contains(flag), "Missing {flag}");
    }
}

/// A body edit counts as something to set, but nothing at all is still a usage error,
/// and replacing and appending together is ambiguous.
#[test]
fn task_set_needs_something_to_set_and_body_excludes_append() {
    let bare = hive_cmd().args(["task", "set", "abc"]).output().unwrap();
    assert!(!bare.status.success());
    let both = hive_cmd()
        .args(["task", "set", "abc", "--body", "x", "--append", "y"])
        .output()
        .unwrap();
    assert!(!both.status.success());
    assert!(String::from_utf8_lossy(&both.stderr).contains("cannot be used with"));
}

#[test]
fn fork_help_lists_its_targets() {
    let output = hive_cmd().args(["fork", "--help"]).output().unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    for flag in ["--wt", "--to", "--id", "--switch"] {
        assert!(stdout.contains(flag), "Missing fork flag '{}'", flag);
    }
}

#[test]
fn fork_outside_a_conversation_says_how_to_name_one() {
    let output = hive_cmd()
        .arg("fork")
        .env_remove("CLAUDE_PID")
        .env_remove("CLAUDE_CODE_SESSION_ID")
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("--id"), "unhelpful error: {stderr}");
}

#[test]
fn stage_freeze_close_help_list_their_flags() {
    for (cmd, flags) in [
        ("stage", &["--id", "--now"][..]),
        ("freeze", &["--id", "--now"][..]),
        ("close", &["--id", "--now"][..]),
    ] {
        let output = hive_cmd().args([cmd, "--help"]).output().unwrap();
        assert!(output.status.success(), "{cmd} --help failed");
        let stdout = String::from_utf8_lossy(&output.stdout);
        for flag in flags {
            assert!(stdout.contains(flag), "Missing {cmd} flag '{flag}'");
        }
    }
}

/// Outside a conversation there's no "me" to act on: each says how to name one
/// instead of guessing — and never reaches tmux.
#[test]
fn stage_freeze_close_outside_a_conversation_point_at_id() {
    for args in [&["stage", "done"][..], &["freeze"][..], &["close"][..]] {
        let output = hive_cmd()
            .args(args)
            .env_remove("CLAUDE_PID")
            .env_remove("CLAUDE_CODE_SESSION_ID")
            .output()
            .unwrap();
        assert!(!output.status.success(), "{args:?} succeeded");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("--id"),
            "unhelpful error for {args:?}: {stderr}"
        );
    }
}

#[test]
fn stage_rejects_an_unknown_stage_before_touching_anything() {
    let output = hive_cmd()
        .args(["stage", "unsorted", "--id", "x"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("unknown stage"), "{stderr}");
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
        .args([
            "todo",
            "list",
            "--done",
            "--session",
            "nonexistent-test-session",
        ])
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

/// A project lives on one machine: once its checkout has shipped, nothing starts work on the
/// stale copy left here. Both refusals happen before tmux or git is touched.
#[test]
fn a_project_that_lives_on_another_machine_refuses_local_work() {
    let home = std::env::temp_dir().join(format!("hive-smoke-liveson-{}", std::process::id()));
    std::fs::create_dir_all(home.join(".hive/cache")).unwrap();
    let run = |args: &[&str]| hive_cmd().args(args).env("HOME", &home).output().unwrap();
    assert!(run(&[
        "project",
        "add",
        "demo",
        "--emoji",
        "🧪",
        "--path",
        "/tmp/demo"
    ])
    .status
    .success());
    std::fs::write(
        home.join(".hive/cache/project-hosts.json"),
        r#"{"demo":"mini"}"#,
    )
    .unwrap();
    for args in [
        vec!["connect", "demo"],
        vec!["wt", "new", "demo", "feat", "--no-switch"],
    ] {
        let o = run(&args);
        let err = String::from_utf8_lossy(&o.stderr).to_string();
        assert!(!o.status.success(), "{args:?} should be refused");
        assert!(err.contains("lives on mini"), "{args:?}: {err}");
    }
}

#[test]
fn project_archive_roundtrip() {
    // Isolate ~/.hive by pointing HOME at a temp dir; dirs::home_dir() honors $HOME.
    let home = std::env::temp_dir().join(format!("hive-smoke-archive-{}", std::process::id()));
    std::fs::create_dir_all(&home).unwrap();

    let run = |args: &[&str]| hive_cmd().args(args).env("HOME", &home).output().unwrap();

    // Add a project to the isolated registry.
    let output = run(&[
        "project",
        "add",
        "demo",
        "--emoji",
        "🧪",
        "--path",
        "/tmp/demo",
    ]);
    assert!(output.status.success());

    // Archive it.
    let output = run(&["project", "archive", "demo"]);
    assert!(output.status.success());

    // Default list hides it.
    let output = run(&["project", "list"]);
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains("demo"),
        "Archived project should be hidden: {}",
        stdout
    );

    // --all shows it, marked.
    let output = run(&["project", "list", "--all"]);
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("demo"),
        "--all should show archived: {}",
        stdout
    );
    assert!(
        stdout.contains("[archived]"),
        "--all should mark archived: {}",
        stdout
    );

    // Persisted as archived = true.
    let toml = std::fs::read_to_string(home.join(".hive/projects.toml")).unwrap();
    assert!(
        toml.contains("archived = true"),
        "TOML missing archived flag: {}",
        toml
    );

    // Unarchive removes the flag and the project reappears in the default list.
    let output = run(&["project", "unarchive", "demo"]);
    assert!(output.status.success());
    let output = run(&["project", "list"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("demo"),
        "Unarchived project should reappear: {}",
        stdout
    );
    let toml = std::fs::read_to_string(home.join(".hive/projects.toml")).unwrap();
    assert!(
        !toml.contains("archived"),
        "archived flag should be gone: {}",
        toml
    );

    // Archiving a missing key fails.
    let output = run(&["project", "archive", "does-not-exist"]);
    assert!(!output.status.success());

    std::fs::remove_dir_all(&home).ok();
}

// --- Auth profile flag (the field that used to need a hand-edit of projects.toml) ---

#[test]
fn project_add_persists_auth_profile() {
    let home = std::env::temp_dir().join(format!("hive-smoke-auth-{}", std::process::id()));
    std::fs::create_dir_all(&home).unwrap();

    let output = hive_cmd()
        .args([
            "project",
            "add",
            "workproj",
            "--emoji",
            "🧪",
            "--path",
            "/tmp/workproj",
            "--auth-profile",
            "work",
        ])
        .env("HOME", &home)
        .output()
        .unwrap();
    assert!(output.status.success());

    let toml = std::fs::read_to_string(home.join(".hive/projects.toml")).unwrap();
    assert!(
        toml.contains(r#"auth_profile = "work""#),
        "auth_profile not persisted: {}",
        toml
    );

    // Omitting the flag leaves the field out entirely (it's skip_serializing_if None).
    let output = hive_cmd()
        .args([
            "project",
            "add",
            "plainproj",
            "--emoji",
            "🧪",
            "--path",
            "/tmp/plainproj",
        ])
        .env("HOME", &home)
        .output()
        .unwrap();
    assert!(output.status.success());
    let toml = std::fs::read_to_string(home.join(".hive/projects.toml")).unwrap();
    assert_eq!(
        toml.matches("auth_profile").count(),
        1,
        "only the profiled project should carry auth_profile: {}",
        toml
    );

    std::fs::remove_dir_all(&home).ok();
}

// --- Boxes (phase 1 of docs/remote-boxes.md) ---

fn git_in(dir: &std::path::Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// pack → destroy the worktree, its branch and the local transcript → unpack --thaw.
/// Everything runs against an isolated `$HOME` and a private tmux server (never the
/// user's): the "target machine" is just the same box with everything removed.
#[test]
fn box_pack_unpack_thaw_roundtrip() {
    let root = std::fs::canonicalize(std::env::temp_dir())
        .unwrap()
        .join(format!("hive-smoke-box-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let home = root.join("home");
    let tmux_dir = root.join("tmux");
    let bin = root.join("bin");
    for d in [&home, &tmux_dir, &bin] {
        std::fs::create_dir_all(d).unwrap();
    }
    // A stand-in `claude` that records how it was resumed.
    let argv_log = root.join("claude-argv");
    let fake = bin.join("claude");
    std::fs::write(
        &fake,
        format!(
            "#!/bin/sh\necho \"$@\" > '{}'\nsleep 30\n",
            argv_log.display()
        ),
    )
    .unwrap();
    std::process::Command::new("chmod")
        .arg("+x")
        .arg(&fake)
        .output()
        .unwrap();
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    let run = |args: &[&str]| {
        hive_cmd()
            .args(args)
            .env("HOME", &home)
            .env("PATH", &path)
            .env("SHELL", "/bin/sh")
            .env("TMUX_TMPDIR", &tmux_dir)
            .env_remove("TMUX")
            .output()
            .unwrap()
    };
    let ok = |args: &[&str]| {
        let o = run(args);
        assert!(
            o.status.success(),
            "hive {args:?}\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&o.stdout),
            String::from_utf8_lossy(&o.stderr)
        );
        String::from_utf8_lossy(&o.stdout).to_string()
    };

    // A project with a pushed base, plus a linked worktree carrying unpushed commits,
    // a staged file, an unstaged edit, an untracked file and an ignored one.
    let origin = root.join("origin.git");
    let proj = home.join("proj");
    let wt = home.join("proj-feat");
    Command::new("git")
        .args(["init", "-q", "--bare"])
        .arg(&origin)
        .output()
        .unwrap();
    Command::new("git")
        .args(["clone", "-q"])
        .arg(&origin)
        .arg(&proj)
        .output()
        .unwrap();
    git_in(&proj, &["config", "user.email", "a@b"]);
    git_in(&proj, &["config", "user.name", "t"]);
    std::fs::write(proj.join("a.txt"), "base\n").unwrap();
    std::fs::write(proj.join(".gitignore"), "*.log\n").unwrap();
    git_in(&proj, &["add", "."]);
    git_in(&proj, &["commit", "-qm", "base"]);
    git_in(&proj, &["branch", "-M", "main"]);
    git_in(&proj, &["push", "-q", "origin", "main"]);
    git_in(
        &proj,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "feat",
            wt.to_str().unwrap(),
            "origin/main",
        ],
    );
    for n in ["c1", "c2"] {
        std::fs::write(wt.join(format!("{n}.txt")), n).unwrap();
        git_in(&wt, &["add", "."]);
        git_in(&wt, &["commit", "-qm", n]);
    }
    std::fs::write(wt.join("s.txt"), "staged").unwrap();
    git_in(&wt, &["add", "s.txt"]);
    std::fs::write(wt.join("a.txt"), "edited\n").unwrap();
    std::fs::write(wt.join("untracked.txt"), "un").unwrap();
    std::fs::write(wt.join("x.log"), "ignored").unwrap();
    let status = git_in(&wt, &["status", "--short"]);
    let log = git_in(&wt, &["log", "--oneline"]);
    let head = git_in(&wt, &["rev-parse", "HEAD"]);

    // Register the project and the worktree.
    ok(&[
        "project",
        "add",
        "proj",
        "--emoji",
        "📁",
        "--path",
        proj.to_str().unwrap(),
        "--worktrees-dir",
        home.to_str().unwrap(),
    ]);
    std::fs::create_dir_all(home.join(".hive/cache")).unwrap();
    std::fs::write(
        home.join(".hive/cache/worktrees.json"),
        format!(
            r#"{{"worktrees":{{"proj/feat":{{"project_key":"proj","branch":"feat","worktree_type":"worktree","path":"{}","session_name":"wt-sess","created_at":"2026-01-01T00:00:00Z"}}}}}}"#,
            wt.display()
        ),
    )
    .unwrap();

    // The conversation: a transcript launched in the worktree, under the default profile.
    let id = "11111111-2222-3333-4444-555555555555";
    let slug = wt.to_str().unwrap().replace('/', "-");
    let slug_dir = home.join(".claude/projects").join(&slug);
    std::fs::create_dir_all(slug_dir.join("memory")).unwrap();
    std::fs::write(slug_dir.join("memory/MEMORY.md"), "remember\n").unwrap();
    let transcript = slug_dir.join(format!("{id}.jsonl"));
    let line = format!(
        r#"{{"type":"user","cwd":"{}","sessionId":"{id}","timestamp":"2026-10-20T10:00:00Z","message":{{"role":"user","content":"hello"}}}}"#,
        wt.display()
    );
    std::fs::write(&transcript, format!("{line}\n")).unwrap();
    let transcript_bytes = std::fs::read(&transcript).unwrap();

    // A project hook, as `hive wt new` would run it: unpack must run it on the fresh worktree.
    let hooks = home.join(".hive/projects/proj/hooks");
    std::fs::create_dir_all(&hooks).unwrap();
    let hook_marker = root.join("hook-ran");
    std::fs::write(
        hooks.join("post-worktree.sh"),
        format!(
            "#!/bin/sh\necho \"$HIVE_PROJECT_KEY $HIVE_BRANCH $HIVE_WORKTREE_PATH\" > '{}'\n",
            hook_marker.display()
        ),
    )
    .unwrap();

    std::process::Command::new("chmod")
        .arg("+x")
        .arg(hooks.join("post-worktree.sh"))
        .output()
        .unwrap();

    // pack
    let out = ok(&["box", "pack", &id[..8]]);
    assert!(out.contains("packed"), "{out}");
    let box_dir = home.join(".hive/boxes").join(id);
    assert!(box_dir.join("manifest.json").is_file());
    assert!(box_dir.join("commits.bundle").is_file());
    assert!(ok(&["box", "list"]).contains("proj/feat"));

    // Destroy everything that makes this "the origin machine".
    git_in(
        &proj,
        &["worktree", "remove", "--force", wt.to_str().unwrap()],
    );
    git_in(&proj, &["branch", "-D", "feat"]);
    std::fs::remove_dir_all(&slug_dir).unwrap();
    std::fs::remove_file(home.join(".hive/cache/worktrees.json")).unwrap();

    // unpack + thaw
    let out = ok(&["box", "unpack", &id[..8], "--thaw"]);
    assert!(out.contains("resumed in session"), "{out}");

    assert_eq!(git_in(&wt, &["status", "--short"]), status);
    assert_eq!(git_in(&wt, &["log", "--oneline"]), log);
    assert_eq!(git_in(&wt, &["rev-parse", "HEAD"]), head);
    assert_eq!(
        std::fs::read_to_string(wt.join("x.log")).unwrap(),
        "ignored"
    );
    assert_eq!(std::fs::read(&transcript).unwrap(), transcript_bytes);
    // The unpacked worktree is pre-trusted, so the resumed Claude doesn't stop at the trust prompt.
    let claude_json = std::fs::read_to_string(home.join(".claude.json")).unwrap();
    assert!(
        claude_json.contains(wt.to_str().unwrap())
            && claude_json.contains("hasTrustDialogAccepted"),
        "worktree pre-trusted: {claude_json}"
    );
    assert!(slug_dir.join("memory/MEMORY.md").is_file());
    let wts = std::fs::read_to_string(home.join(".hive/cache/worktrees.json")).unwrap();
    assert!(wts.contains("proj/feat"), "worktree re-registered: {wts}");

    // thaw ran `claude --resume <id>` in the restored window
    let mut resumed = String::new();
    for _ in 0..50 {
        if let Ok(s) = std::fs::read_to_string(&argv_log) {
            resumed = s;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert_eq!(resumed.trim(), format!("--resume {id}"));

    // A second unpack over the restored worktree must refuse, not clobber.
    let again = run(&["box", "unpack", id]);
    assert!(!again.status.success());

    // Turns that exist only here are never overwritten: with the worktree gone (so that
    // guard cannot be the one refusing) and a transcript longer than the box's, unpack
    // refuses, touches nothing, and `--force` is what replaces it.
    git_in(
        &proj,
        &["worktree", "remove", "--force", wt.to_str().unwrap()],
    );
    let mut longer = transcript_bytes.clone();
    longer.extend_from_slice(b"{\"type\":\"user\",\"message\":\"a turn the box never saw\"}\n");
    std::fs::write(&transcript, &longer).unwrap();
    let refused = run(&["box", "unpack", id]);
    assert!(!refused.status.success());
    let why = String::from_utf8_lossy(&refused.stderr);
    assert!(why.contains("longer transcript"), "{why}");
    assert_eq!(std::fs::read(&transcript).unwrap(), longer);
    assert!(!wt.exists(), "nothing restored by a refused unpack");
    ok(&["box", "unpack", id, "--force"]);
    assert_eq!(std::fs::read(&transcript).unwrap(), transcript_bytes);
    assert!(wt.join("a.txt").exists() || wt.join(".git").exists());

    // Tear down only the private tmux server (TMUX_TMPDIR is ours).
    Command::new("tmux")
        .arg("kill-server")
        .env("TMUX_TMPDIR", &tmux_dir)
        .env_remove("TMUX")
        .output()
        .ok();
    std::fs::remove_dir_all(&root).ok();
}

/// push → the other "machine" resumes it → pull brings it back. Two `$HOME`s and a fake
/// `ssh` that runs the command under the second one stand in for two machines. The repo
/// and worktree live outside both homes at one shared path (paths must match across
/// machines), so the shim removes the sender's copy before the receiver unpacks — which on
/// real hardware is simply a different disk.
#[test]
fn box_push_pull_between_two_homes() {
    let root = std::fs::canonicalize(std::env::temp_dir())
        .unwrap()
        .join(format!("hive-smoke-push-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let (ha, hb) = (root.join("home-a"), root.join("home-b"));
    let (ta, tb) = (root.join("tmux-a"), root.join("tmux-b"));
    let bin = root.join("bin");
    for d in [&ha, &hb, &ta, &tb, &bin, &hb.join(".claude")] {
        std::fs::create_dir_all(d).unwrap();
    }
    let argv_log = root.join("claude-argv");
    let fake = bin.join("claude");
    std::fs::write(
        &fake,
        format!(
            // Like a real Claude it writes its own status record, which the pack gate
            // requires of a running window (`$2` is the id after `--resume`).
            "#!/bin/sh\necho \"$HOME $@\" > '{}'\n\
             mkdir -p \"$HOME/.claude/sessions\"\n\
             printf '{{\"pid\":%s,\"sessionId\":\"%s\",\"status\":\"idle\"}}' $$ \"$2\" \
             > \"$HOME/.claude/sessions/$$.json\"\nsleep 30\n",
            argv_log.display()
        ),
    )
    .unwrap();
    let proj = root.join("proj");
    let wt = root.join("proj-feat");
    // Fake ssh: `ssh [-o X] host cmd…` → run cmd under machine B's HOME and tmux. Just
    // before B unpacks, drop the shared-path worktree (A's copy) to model separate disks.
    let shim = bin.join("fakessh");
    std::fs::write(
        &shim,
        format!(
            r#"#!/bin/sh
while [ $# -gt 0 ]; do case "$1" in -o|-l|-p|-i) shift 2;; -*) shift;; *) break;; esac; done
shift
cd '{hb}' || exit 1
case "$*" in *unpack*) git -C '{proj}' worktree remove --force '{wt}' 2>/dev/null;; esac
HOME='{hb}' TMUX_TMPDIR='{tb}' exec sh -c "$*"
"#,
            hb = hb.display(),
            tb = tb.display(),
            proj = proj.display(),
            wt = wt.display()
        ),
    )
    .unwrap();
    for f in [&fake, &shim] {
        Command::new("chmod").arg("+x").arg(f).output().unwrap();
    }
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    let run_as = |home: &std::path::Path, tmux: &std::path::Path, args: &[&str]| {
        let o = hive_cmd()
            .args(args)
            .env("HOME", home)
            .env("PATH", &path)
            .env("SHELL", "/bin/sh")
            .env("TMUX_TMPDIR", tmux)
            .env("HIVE_SSH", &shim)
            .env("HIVE_BOX_SHARED_PATHS", "1")
            .env_remove("TMUX")
            .output()
            .unwrap();
        assert!(
            o.status.success(),
            "hive {args:?}\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&o.stdout),
            String::from_utf8_lossy(&o.stderr)
        );
        String::from_utf8_lossy(&o.stdout).to_string()
    };
    let a = |args: &[&str]| run_as(&ha, &ta, args);

    // Repo with a worktree carrying unpushed + dirty state.
    let origin = root.join("origin.git");
    Command::new("git")
        .args(["init", "-q", "--bare"])
        .arg(&origin)
        .output()
        .unwrap();
    Command::new("git")
        .args(["clone", "-q"])
        .arg(&origin)
        .arg(&proj)
        .output()
        .unwrap();
    git_in(&proj, &["config", "user.email", "a@b"]);
    git_in(&proj, &["config", "user.name", "t"]);
    std::fs::write(proj.join("a.txt"), "base\n").unwrap();
    git_in(&proj, &["add", "."]);
    git_in(&proj, &["commit", "-qm", "base"]);
    git_in(&proj, &["branch", "-M", "main"]);
    git_in(&proj, &["push", "-q", "origin", "main"]);
    git_in(
        &proj,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "feat",
            wt.to_str().unwrap(),
            "origin/main",
        ],
    );
    std::fs::write(wt.join("c1.txt"), "c1").unwrap();
    git_in(&wt, &["add", "."]);
    git_in(&wt, &["commit", "-qm", "c1"]);
    std::fs::write(wt.join("a.txt"), "edited\n").unwrap();
    std::fs::write(wt.join("untracked.txt"), "un").unwrap();
    let status = git_in(&wt, &["status", "--short"]);
    let log = git_in(&wt, &["log", "--oneline"]);

    // Machine A: project, worktree registry, conversation. Machine B: the project only.
    for (h, t) in [(&ha, &ta), (&hb, &tb)] {
        run_as(
            h,
            t,
            &[
                "project",
                "add",
                "proj",
                "--emoji",
                "📁",
                "--path",
                proj.to_str().unwrap(),
                "--worktrees-dir",
                root.to_str().unwrap(),
            ],
        );
    }
    std::fs::create_dir_all(ha.join(".hive/cache")).unwrap();
    std::fs::write(
        ha.join(".hive/cache/worktrees.json"),
        format!(
            r#"{{"worktrees":{{"proj/feat":{{"project_key":"proj","branch":"feat","worktree_type":"worktree","path":"{}","session_name":"wt-sess","created_at":"2026-01-01T00:00:00Z"}}}}}}"#,
            wt.display()
        ),
    )
    .unwrap();
    let id = "aaaaaaaa-2222-3333-4444-555555555555";
    let slug = wt.to_str().unwrap().replace('/', "-");
    let slug_a = ha.join(".claude/projects").join(&slug);
    std::fs::create_dir_all(&slug_a).unwrap();
    std::fs::write(
        slug_a.join(format!("{id}.jsonl")),
        format!(
            r#"{{"type":"user","cwd":"{}","sessionId":"{id}","timestamp":"2026-10-20T10:00:00Z","message":{{"role":"user","content":"hello"}}}}
"#,
            wt.display()
        ),
    )
    .unwrap();

    a(&[
        "remote",
        "add",
        "b",
        "--ssh",
        "me@b",
        "--hive-path",
        env!("CARGO_BIN_EXE_hive"),
    ]);
    assert!(a(&["remote", "list"]).contains("me@b"));

    // push
    let out = a(&["box", "push", &id[..8], "b"]);
    assert!(out.contains("now lives on b"), "{out}");
    assert_eq!(
        git_in(&wt, &["status", "--short"]),
        status,
        "B restored the tree"
    );
    assert_eq!(git_in(&wt, &["log", "--oneline"]), log);
    let slug_b = hb.join(".claude/projects").join(&slug);
    assert!(
        slug_b.join(format!("{id}.jsonl")).is_file(),
        "transcript reached B"
    );
    let mut resumed = String::new();
    for _ in 0..50 {
        if let Ok(s) = std::fs::read_to_string(&argv_log) {
            resumed = s;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert_eq!(
        resumed.trim(),
        format!("{} --resume {id}", hb.display()),
        "resumed on B"
    );
    assert!(
        a(&["box", "list"]).contains("→"),
        "A records that it lives elsewhere"
    );
    let sidecar = std::fs::read_to_string(ha.join(".hive/cache/conversations.json")).unwrap();
    assert!(sidecar.contains("\"location\""), "{sidecar}");

    // pull. Without the shared-filesystem escape the copy still sitting at the worktree path
    // (here: the remote's own, the rig being one disk) must stop the pull BEFORE the remote
    // is asked to pack or release anything.
    let refused = hive_cmd()
        .args(["box", "pull", &id[..8], "--remove-remote"])
        .env("HOME", &ha)
        .env("PATH", &path)
        .env("SHELL", "/bin/sh")
        .env("TMUX_TMPDIR", &ta)
        .env("HIVE_SSH", &shim)
        .env_remove("TMUX")
        .env_remove("HIVE_BOX_SHARED_PATHS")
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&refused.stderr).to_string();
    assert!(!refused.status.success(), "{err}");
    assert!(err.contains("nothing was changed on"), "{err}");
    assert!(wt.join("c1.txt").exists(), "the remote copy must survive");

    std::fs::remove_file(&argv_log).unwrap();
    let out = a(&["box", "pull", &id[..8], "--remove-remote"]);
    assert!(out.contains("back on this machine"), "{out}");
    assert_eq!(
        git_in(&wt, &["status", "--short"]),
        status,
        "A restored the tree"
    );
    assert_eq!(git_in(&wt, &["log", "--oneline"]), log);
    let mut resumed = String::new();
    for _ in 0..50 {
        if let Ok(s) = std::fs::read_to_string(&argv_log) {
            resumed = s;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert_eq!(
        resumed.trim(),
        format!("{} --resume {id}", ha.display()),
        "resumed on A"
    );
    let sidecar = std::fs::read_to_string(ha.join(".hive/cache/conversations.json")).unwrap();
    assert!(
        !sidecar.contains("\"location\""),
        "A owns it again: {sidecar}"
    );
    let sidecar_b = std::fs::read_to_string(hb.join(".hive/cache/conversations.json")).unwrap();
    assert!(
        sidecar_b.contains("\"location\""),
        "B records it shipped back: {sidecar_b}"
    );

    for t in [&ta, &tb] {
        Command::new("tmux")
            .arg("kill-server")
            .env("TMUX_TMPDIR", t)
            .env_remove("TMUX")
            .output()
            .ok();
    }
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn project_export_import_roundtrip() {
    let root = std::fs::canonicalize(std::env::temp_dir())
        .unwrap()
        .join(format!("hive-smoke-pexp-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let (ha, hb) = (root.join("a"), root.join("b"));
    std::fs::create_dir_all(&ha).unwrap();
    std::fs::create_dir_all(&hb).unwrap();
    let run = |h: &std::path::Path, args: &[&str]| {
        let o = hive_cmd().args(args).env("HOME", h).output().unwrap();
        assert!(
            o.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&o.stderr)
        );
        String::from_utf8_lossy(&o.stdout).to_string()
    };
    run(
        &ha,
        &[
            "project",
            "add",
            "p1",
            "--emoji",
            "🧪",
            "--path",
            "/srv/p1",
            "--auth-profile",
            "work",
        ],
    );
    let exported = run(&ha, &["project", "export"]);
    let file = root.join("projects.toml");
    std::fs::write(&file, &exported).unwrap();
    assert!(run(&hb, &["project", "import", file.to_str().unwrap()]).contains("imported 1"));
    assert!(run(&hb, &["project", "list"]).contains("p1"));
    // Existing keys are kept, not clobbered.
    assert!(run(&hb, &["project", "import", file.to_str().unwrap()]).contains("kept 1"));
    std::fs::remove_dir_all(&root).ok();
}

// --- Federation (phases 3-4 of docs/remote-boxes.md) ---

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn http(port: u16, method: &str, path: &str, body: &str) -> String {
    use std::io::{Read, Write};
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(std::time::Duration::from_secs(8)))
        .unwrap();
    write!(
        s,
        "{method} {path} HTTP/1.0\r\nHost: x\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    out.split_once("\r\n\r\n")
        .map(|x| x.1.to_string())
        .unwrap_or_default()
}

struct Server(std::process::Child);
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[allow(clippy::zombie_processes)] // reaped by `Server::drop`
fn start_web(home: &std::path::Path, tmux: &std::path::Path, path: &str, port: u16) -> Server {
    let child = hive_cmd()
        .args(["web", "--port", &port.to_string()])
        .env("HOME", home)
        .env("PATH", path)
        .env("SHELL", "/bin/sh")
        .env("TMUX_TMPDIR", tmux)
        .env("HIVE_NO_NOTIFY", "1")
        .env_remove("TMUX")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    for _ in 0..100 {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return Server(child);
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    panic!("web server on {port} never came up");
}

/// Machine B parks a conversation; machine A's dashboard lists it as `host: b`, a dead
/// remote contributes nothing without stalling A, and thawing it from A runs `claude
/// --resume` on B (not A) through B's own API.
#[test]
fn federation_lists_a_remote_parked_conversation_and_thaws_it_there() {
    let root = std::fs::canonicalize(std::env::temp_dir())
        .unwrap()
        .join(format!("hive-smoke-fed-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let (ha, hb) = (root.join("home-a"), root.join("home-b"));
    let (ta, tb) = (root.join("tmux-a"), root.join("tmux-b"));
    let bin = root.join("bin");
    let work = root.join("work");
    for d in [&ha, &hb, &ta, &tb, &bin, &work] {
        std::fs::create_dir_all(d).unwrap();
    }
    let argv_log = root.join("claude-argv");
    let fake = bin.join("claude");
    std::fs::write(
        &fake,
        format!(
            // Like a real Claude it writes its own status record, which the pack gate
            // requires of a running window (`$2` is the id after `--resume`).
            "#!/bin/sh\necho \"$HOME $@\" > '{}'\n\
             mkdir -p \"$HOME/.claude/sessions\"\n\
             printf '{{\"pid\":%s,\"sessionId\":\"%s\",\"status\":\"idle\"}}' $$ \"$2\" \
             > \"$HOME/.claude/sessions/$$.json\"\nsleep 30\n",
            argv_log.display()
        ),
    )
    .unwrap();
    Command::new("chmod").arg("+x").arg(&fake).output().unwrap();
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    // B: a transcript and a frozen entry for it.
    let id = "bbbbbbbb-2222-3333-4444-555555555555";
    let slug = work.to_str().unwrap().replace('/', "-");
    let slug_dir = hb.join(".claude/projects").join(slug);
    std::fs::create_dir_all(&slug_dir).unwrap();
    std::fs::write(
        slug_dir.join(format!("{id}.jsonl")),
        format!(
            r#"{{"type":"user","cwd":"{}","sessionId":"{id}","timestamp":"2026-10-20T10:00:00Z","message":{{"role":"user","content":"hi"}}}}
"#,
            work.display()
        ),
    )
    .unwrap();
    std::fs::create_dir_all(hb.join(".hive/cache")).unwrap();
    std::fs::write(
        hb.join(".hive/cache/frozen.json"),
        format!(
            r#"{{"frozen":{{"{id}":{{"session_name":"bsess","window_name":"w","window_index":"1","cwd":"{}","claude_session_id":"{id}","note":"parked on b","frozen_at":"2026-10-20T10:00:00Z"}}}}}}"#,
            work.display()
        ),
    )
    .unwrap();

    let (pa, pb) = (free_port(), free_port());
    let _b = start_web(&hb, &tb, &path, pb);
    // A knows two remotes: the live B, and one nothing listens on.
    std::fs::create_dir_all(ha.join(".hive")).unwrap();
    std::fs::write(
        ha.join(".hive/config.toml"),
        format!(
            "[[remotes]]\nname = \"b\"\nssh = \"x@b\"\napi = \"http://127.0.0.1:{pb}\"\n\n\
             [[remotes]]\nname = \"dead\"\nssh = \"x@dead\"\napi = \"http://127.0.0.1:{}\"\n",
            free_port()
        ),
    )
    .unwrap();
    let _a = start_web(&ha, &ta, &path, pa);

    // B's registry export holds the parked conversation, and only local rows.
    let mut listed = String::new();
    for _ in 0..80 {
        listed = http(pa, "GET", "/api/conversations", "");
        if listed.contains(id) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    let views: serde_json::Value = serde_json::from_str(&listed).expect(&listed);
    let row = views
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["id"] == id)
        .unwrap_or_else(|| panic!("A never listed B's conversation: {listed}"));
    assert_eq!(row["host"], "b");
    assert_eq!(row["frozen"], true);
    // Nothing of A's own was echoed into B's export.
    assert!(!http(pb, "GET", "/api/registry", "").contains("\"host\""));

    // Thaw it from A: B resumes it, in B's HOME.
    let out = http(
        pa,
        "POST",
        "/api/thaw",
        &format!(r#"{{"key":"{id}","host":"b"}}"#),
    );
    assert!(out.contains("\"ok\":true"), "{out}");
    let mut resumed = String::new();
    for _ in 0..50 {
        if let Ok(s) = std::fs::read_to_string(&argv_log) {
            resumed = s;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert_eq!(resumed.trim(), format!("{} --resume {id}", hb.display()));

    // An action aimed at a host that isn't there fails cleanly instead of hanging.
    let t = std::time::Instant::now();
    let out = http(
        pa,
        "POST",
        "/api/thaw",
        &format!(r#"{{"key":"{id}","host":"dead"}}"#),
    );
    assert!(out.contains("error"), "{out}");
    assert!(t.elapsed() < std::time::Duration::from_secs(8));

    for t in [&ta, &tb] {
        Command::new("tmux")
            .arg("kill-server")
            .env("TMUX_TMPDIR", t)
            .env_remove("TMUX")
            .output()
            .ok();
    }
    drop((_a, _b));
    std::fs::remove_dir_all(&root).ok();
}

// --- Sandbox-defined boxes (phase 5) ---

/// Thawing with `--sandbox` runs the conversation under `srt` with a write set derived from
/// the box: a write inside is allowed, one outside is refused by the kernel. Needs the real
/// `srt`, so it skips where that is not installed.
#[test]
fn sandboxed_thaw_confines_writes_to_the_boxs_path_set() {
    if !Command::new("srt")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
    {
        eprintln!("skipped: srt not installed");
        return;
    }
    let root = std::fs::canonicalize(std::env::temp_dir())
        .unwrap()
        .join(format!("hive-smoke-sbx-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let home = root.join("home");
    let tmux = root.join("tmux");
    let bin = root.join("bin");
    for d in [&home, &tmux, &bin] {
        std::fs::create_dir_all(d).unwrap();
    }
    // Outside every allowed path: not temp, not home, not the project.
    let outside = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join(format!("sbx-outside-{}", std::process::id()));
    let _ = std::fs::remove_file(&outside);
    // A stand-in for "another program's database": readable by anyone, denied by config.
    let secret_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join(format!("sbx-secret-{}", std::process::id()));
    std::fs::create_dir_all(&secret_dir).unwrap();
    std::fs::write(secret_dir.join("wa.sqlite"), "TOPSECRET").unwrap();
    std::fs::create_dir_all(home.join(".hive")).unwrap();
    std::fs::write(
        home.join(".hive/config.toml"),
        format!("[sandbox]\ndeny_read = [\"{}\"]\n", secret_dir.display()),
    )
    .unwrap();
    let proj = home.join("proj");
    let marker = root.join("marker");
    // `srt` may be a version-manager shim that needs the REAL home; hive runs under an
    // isolated one, so reach it through a wrapper that restores it.
    let real_srt = String::from_utf8_lossy(
        &Command::new("sh")
            .args(["-c", "command -v srt"])
            .output()
            .unwrap()
            .stdout,
    )
    .trim()
    .to_string();
    std::fs::write(
        bin.join("srt"),
        format!(
            "#!/bin/sh\nHOME='{}' exec '{real_srt}' \"$@\"\n",
            std::env::var("HOME").unwrap()
        ),
    )
    .unwrap();
    Command::new("chmod")
        .arg("+x")
        .arg(bin.join("srt"))
        .output()
        .unwrap();
    let fake = bin.join("claude");
    std::fs::write(
        &fake,
        format!(
            "#!/bin/sh\necho in > '{}/inside.txt' 2>/dev/null && echo inside-ok >> '{m}'\n\
             echo out > '{o}' 2>/dev/null && echo outside-WROTE >> '{m}' || echo outside-denied >> '{m}'\n\
             cat '{sec}/wa.sqlite' >/dev/null 2>&1 && echo secret-READ >> '{m}' || echo secret-denied >> '{m}'\n\
             echo done >> '{m}'\nsleep 30\n",
            proj.display(),
            m = marker.display(),
            o = outside.display(),
            sec = secret_dir.display()
        ),
    )
    .unwrap();
    Command::new("chmod").arg("+x").arg(&fake).output().unwrap();
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let run = |args: &[&str]| {
        let o = hive_cmd()
            .args(args)
            .env("HOME", &home)
            .env("PATH", &path)
            .env("SHELL", "/bin/sh")
            .env("TMUX_TMPDIR", &tmux)
            .env_remove("TMUX")
            .output()
            .unwrap();
        assert!(
            o.status.success(),
            "hive {args:?}\n{}\n{}",
            String::from_utf8_lossy(&o.stdout),
            String::from_utf8_lossy(&o.stderr)
        );
        String::from_utf8_lossy(&o.stdout).to_string()
    };

    std::fs::create_dir_all(&proj).unwrap();
    git_in(&proj, &["init", "-q", "-b", "main"]);
    git_in(&proj, &["config", "user.email", "a@b"]);
    git_in(&proj, &["config", "user.name", "t"]);
    std::fs::write(proj.join("a.txt"), "x\n").unwrap();
    git_in(&proj, &["add", "."]);
    git_in(&proj, &["commit", "-qm", "base"]);
    run(&[
        "project",
        "add",
        "proj",
        "--emoji",
        "📁",
        "--path",
        proj.to_str().unwrap(),
    ]);
    let id = "cccccccc-2222-3333-4444-555555555555";
    let slug_dir = home
        .join(".claude/projects")
        .join(proj.to_str().unwrap().replace('/', "-"));
    std::fs::create_dir_all(&slug_dir).unwrap();
    std::fs::write(
        slug_dir.join(format!("{id}.jsonl")),
        format!(
            r#"{{"type":"user","cwd":"{}","sessionId":"{id}","timestamp":"2026-10-20T10:00:00Z","message":{{"role":"user","content":"hi"}}}}
"#,
            proj.display()
        ),
    )
    .unwrap();

    let out = run(&["box", "pack", &id[..8]]);
    assert!(out.contains("packed"), "{out}");
    let out = run(&["box", "unpack", &id[..8], "--thaw", "--sandbox"]);
    assert!(out.contains("sandbox:"), "{out}");

    let settings =
        std::fs::read_to_string(home.join(format!(".hive/cache/sandbox/{id}.json"))).unwrap();
    assert!(settings.contains(proj.to_str().unwrap()) && settings.contains("deniedDomains"));

    let mut log = String::new();
    for _ in 0..100 {
        log = std::fs::read_to_string(&marker).unwrap_or_default();
        if log.contains("done") {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    assert!(
        log.contains("inside-ok"),
        "write inside the box's set: {log}"
    );
    assert!(log.contains("outside-denied"), "write outside it: {log}");
    assert!(!outside.exists(), "the kernel refused it");
    assert!(
        log.contains("secret-denied") && !log.contains("secret-READ"),
        "a configured deny_read path is unreadable from inside: {log}"
    );
    let _ = std::fs::remove_dir_all(&secret_dir);

    Command::new("tmux")
        .arg("kill-server")
        .env("TMUX_TMPDIR", &tmux)
        .env_remove("TMUX")
        .output()
        .ok();
    std::fs::remove_dir_all(&root).ok();
}

// --- Boxes over HTTP (no ssh, no rsync, no auth: trusted LAN, opt-in, path-checked) ---

/// `box send` / `box pull` against a peer's `hive web`: the box travels as a tarball, B
/// unpacks and resumes it in its own home, and pull brings it back. Also pins the two guards
/// that stand in for authentication: a server that has not opted in refuses, and a box that
/// names a path outside any registered project is refused even when delivered.
#[test]
fn box_transfer_over_http_is_opt_in_and_path_checked() {
    let root = std::fs::canonicalize(std::env::temp_dir())
        .unwrap()
        .join(format!("hive-smoke-http-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let (ha, hb) = (root.join("home-a"), root.join("home-b"));
    let (ta, tb) = (root.join("tmux-a"), root.join("tmux-b"));
    let bin = root.join("bin");
    for d in [&ha, &hb, &ta, &tb, &bin, &hb.join(".claude")] {
        std::fs::create_dir_all(d).unwrap();
    }
    let argv_log = root.join("claude-argv");
    let fake = bin.join("claude");
    std::fs::write(
        &fake,
        format!(
            // Like a real Claude it writes its own status record, which the pack gate
            // requires of a running window (`$2` is the id after `--resume`).
            "#!/bin/sh\necho \"$HOME $@\" > '{}'\n\
             mkdir -p \"$HOME/.claude/sessions\"\n\
             printf '{{\"pid\":%s,\"sessionId\":\"%s\",\"status\":\"idle\"}}' $$ \"$2\" \
             > \"$HOME/.claude/sessions/$$.json\"\nsleep 30\n",
            argv_log.display()
        ),
    )
    .unwrap();
    Command::new("chmod").arg("+x").arg(&fake).output().unwrap();
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let proj = root.join("proj");
    let wt = root.join("proj-feat");

    let run_as = |home: &std::path::Path, tmux: &std::path::Path, args: &[&str]| {
        hive_cmd()
            .args(args)
            .env("HOME", home)
            .env("PATH", &path)
            .env("SHELL", "/bin/sh")
            .env("TMUX_TMPDIR", tmux)
            .env("HIVE_NO_NOTIFY", "1")
            .env("HIVE_BOX_SHARED_PATHS", "1")
            .env_remove("TMUX")
            .output()
            .unwrap()
    };
    let a_ok = |args: &[&str]| {
        let o = run_as(&ha, &ta, args);
        assert!(
            o.status.success(),
            "hive {args:?}\n{}\n{}",
            String::from_utf8_lossy(&o.stdout),
            String::from_utf8_lossy(&o.stderr)
        );
        String::from_utf8_lossy(&o.stdout).to_string()
    };

    // Same repo + worktree + conversation as the ssh test.
    let origin = root.join("origin.git");
    Command::new("git")
        .args(["init", "-q", "--bare"])
        .arg(&origin)
        .output()
        .unwrap();
    Command::new("git")
        .args(["clone", "-q"])
        .arg(&origin)
        .arg(&proj)
        .output()
        .unwrap();
    git_in(&proj, &["config", "user.email", "a@b"]);
    git_in(&proj, &["config", "user.name", "t"]);
    std::fs::write(proj.join("a.txt"), "base\n").unwrap();
    git_in(&proj, &["add", "."]);
    git_in(&proj, &["commit", "-qm", "base"]);
    git_in(&proj, &["branch", "-M", "main"]);
    git_in(&proj, &["push", "-q", "origin", "main"]);
    git_in(
        &proj,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "feat",
            wt.to_str().unwrap(),
            "origin/main",
        ],
    );
    std::fs::write(wt.join("c1.txt"), "c1").unwrap();
    git_in(&wt, &["add", "."]);
    git_in(&wt, &["commit", "-qm", "c1"]);
    std::fs::write(wt.join("a.txt"), "edited\n").unwrap();
    std::fs::write(wt.join("untracked.txt"), "un").unwrap();
    let status = git_in(&wt, &["status", "--short"]);
    let log = git_in(&wt, &["log", "--oneline"]);

    for (h, t) in [(&ha, &ta), (&hb, &tb)] {
        let o = run_as(
            h,
            t,
            &[
                "project",
                "add",
                "proj",
                "--emoji",
                "📁",
                "--path",
                proj.to_str().unwrap(),
                "--worktrees-dir",
                root.to_str().unwrap(),
            ],
        );
        assert!(o.status.success());
    }
    std::fs::create_dir_all(ha.join(".hive/cache")).unwrap();
    std::fs::write(
        ha.join(".hive/cache/worktrees.json"),
        format!(
            r#"{{"worktrees":{{"proj/feat":{{"project_key":"proj","branch":"feat","worktree_type":"worktree","path":"{}","session_name":"wt-sess","created_at":"2026-01-01T00:00:00Z"}}}}}}"#,
            wt.display()
        ),
    )
    .unwrap();
    let id = "dddddddd-2222-3333-4444-555555555555";
    let slug = wt.to_str().unwrap().replace('/', "-");
    let slug_a = ha.join(".claude/projects").join(&slug);
    std::fs::create_dir_all(&slug_a).unwrap();
    std::fs::write(
        slug_a.join(format!("{id}.jsonl")),
        format!(
            r#"{{"type":"user","cwd":"{}","sessionId":"{id}","timestamp":"2026-10-20T10:00:00Z","message":{{"role":"user","content":"hello"}}}}
"#,
            wt.display()
        ),
    )
    .unwrap();

    // B serves the dashboard WITHOUT opting in first.
    let pb = free_port();
    let server_b = start_web(&hb, &tb, &path, pb);
    a_ok(&[
        "remote",
        "add",
        "b",
        "--ssh",
        "x@b",
        "--api",
        &format!("http://127.0.0.1:{pb}"),
    ]);
    let boxdir = ha.join(".hive/boxes").join(id);
    a_ok(&["box", "pack", &id[..8]]);
    assert!(boxdir.join("manifest.json").is_file());
    let o = run_as(&ha, &ta, &["box", "send", &id[..8], "b"]);
    assert!(!o.status.success());
    assert!(
        String::from_utf8_lossy(&o.stderr).contains("accept_boxes"),
        "a server that hasn't opted in says so: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    drop(server_b);

    // B opts in. A box naming a path outside any project is refused even when delivered.
    std::fs::create_dir_all(hb.join(".hive")).unwrap();
    std::fs::write(hb.join(".hive/config.toml"), "[web]\naccept_boxes = true\n").unwrap();
    let _server_b = start_web(&hb, &tb, &path, pb);
    let manifest_path = boxdir.join("manifest.json");
    let original = std::fs::read_to_string(&manifest_path).unwrap();
    let mut evil: serde_json::Value = serde_json::from_str(&original).unwrap();
    evil["root"] = "/etc/hive-evil".into();
    evil["cwd"] = "/etc/hive-evil".into();
    std::fs::write(&manifest_path, evil.to_string()).unwrap();
    let o = run_as(&ha, &ta, &["box", "send", &id[..8], "b"]);
    assert!(!o.status.success());
    assert!(
        String::from_utf8_lossy(&o.stderr).contains("outside project"),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
    assert!(!std::path::Path::new("/etc/hive-evil").exists());
    std::fs::write(&manifest_path, original).unwrap();

    // The real thing. A's copy of the worktree goes away first (separate disks on real
    // hardware; here the paths are shared).
    git_in(
        &proj,
        &["worktree", "remove", "--force", wt.to_str().unwrap()],
    );
    let out = a_ok(&["box", "send", &id[..8], "b"]);
    assert!(out.contains("now lives on b"), "{out}");
    assert_eq!(
        git_in(&wt, &["status", "--short"]),
        status,
        "B restored the tree"
    );
    assert_eq!(git_in(&wt, &["log", "--oneline"]), log);
    assert!(hb
        .join(".claude/projects")
        .join(&slug)
        .join(format!("{id}.jsonl"))
        .is_file());
    let mut resumed = String::new();
    for _ in 0..50 {
        if let Ok(s) = std::fs::read_to_string(&argv_log) {
            resumed = s;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert_eq!(resumed.trim(), format!("{} --resume {id}", hb.display()));

    // What another machine may ask of B is narrower than B's own CLI. An id prefix — fine
    // on the command line — is refused on all three actions, and a removal that does not
    // say where the conversation went is refused even with the whole id. Nothing changes.
    for action in ["pack", "unpack", "mark"] {
        let r = http(
            pb,
            "POST",
            &format!("/api/box/{}/{action}", &id[..8]),
            r#"{"force":true,"remove_worktree":true,"location":"a"}"#,
        );
        assert!(r.contains("full id is required"), "{action}: {r}");
    }
    let r = http(
        pb,
        "POST",
        &format!("/api/box/{id}/mark"),
        r#"{"remove_worktree":true}"#,
    );
    assert!(r.contains("refusing to remove"), "{r}");
    assert_eq!(
        git_in(&wt, &["status", "--short"]),
        status,
        "B's tree is untouched by the refused requests"
    );
    assert!(hb
        .join(".hive/boxes")
        .join(id)
        .join("manifest.json")
        .is_file());
    // A body that cannot be read is refused, not taken as "no location" (which would
    // claim the conversation back for the machine that got the request).
    let r = http(pb, "POST", &format!("/api/box/{id}/mark"), "not json");
    assert!(r.contains("bad request body"), "{r}");
    // `force` is not honoured from the network: with turns on B that the box lacks, an
    // unpack asking for it is refused and B's transcript is untouched.
    let b_transcript = hb
        .join(".claude/projects")
        .join(&slug)
        .join(format!("{id}.jsonl"));
    let before = std::fs::read(&b_transcript).unwrap();
    let mut longer = before.clone();
    longer.extend_from_slice(b"{\"type\":\"user\",\"message\":\"a turn only B has\"}\n");
    std::fs::write(&b_transcript, &longer).unwrap();
    let r = http(
        pb,
        "POST",
        &format!("/api/box/{id}/unpack"),
        r#"{"force":true}"#,
    );
    assert!(r.contains("longer transcript"), "{r}");
    assert_eq!(std::fs::read(&b_transcript).unwrap(), longer);
    std::fs::write(&b_transcript, &before).unwrap();

    // Pull it home over HTTP.
    std::fs::remove_file(&argv_log).unwrap();
    let out = a_ok(&["box", "pull", &id[..8], "--remove-remote"]);
    assert!(out.contains("back on this machine"), "{out}");
    assert_eq!(
        git_in(&wt, &["status", "--short"]),
        status,
        "A restored the tree"
    );
    let mut resumed = String::new();
    for _ in 0..50 {
        if let Ok(s) = std::fs::read_to_string(&argv_log) {
            resumed = s;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert_eq!(resumed.trim(), format!("{} --resume {id}", ha.display()));

    for t in [&ta, &tb] {
        Command::new("tmux")
            .arg("kill-server")
            .env("TMUX_TMPDIR", t)
            .env_remove("TMUX")
            .output()
            .ok();
    }
    std::fs::remove_dir_all(&root).ok();
}

/// `--bind 127.0.0.1` keeps the dashboard off the network: the server on a machine that holds
/// other programs' credentials is reached through an ssh forward, not a LAN port.
#[test]
fn web_bind_loopback_is_not_reachable_on_the_lan_address() {
    // This machine's outward-facing address (a UDP "connect" sends nothing).
    let lan = std::net::UdpSocket::bind("0.0.0.0:0")
        .and_then(|s| s.connect("8.8.8.8:80").map(|_| s))
        .and_then(|s| s.local_addr())
        .map(|a| a.ip());
    let Ok(lan) = lan else {
        eprintln!("skipped: no outward route to learn a LAN address from");
        return;
    };
    if lan.is_loopback() {
        eprintln!("skipped: no non-loopback address");
        return;
    }
    let home = std::fs::canonicalize(std::env::temp_dir())
        .unwrap()
        .join(format!("hive-smoke-bind-{}", std::process::id()));
    std::fs::create_dir_all(&home).unwrap();
    let port = free_port();
    let child = hive_cmd()
        .args(["web", "--port", &port.to_string(), "--bind", "127.0.0.1"])
        .env("HOME", &home)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let server = Server(child);
    let mut up = false;
    for _ in 0..100 {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            up = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(up, "reachable on loopback");
    let on_lan = std::net::TcpStream::connect_timeout(
        &std::net::SocketAddr::new(lan, port),
        std::time::Duration::from_secs(2),
    );
    assert!(on_lan.is_err(), "must not answer on {lan}:{port}");
    drop(server);
    std::fs::remove_dir_all(&home).ok();
}
