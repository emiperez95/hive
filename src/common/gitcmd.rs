//! How hive runs `git` against a tree it didn't make.
//!
//! Every probe (`worktree_health`, `prune`, the detail's commit list, fork's branch
//! lookup) points git at a directory chosen by a transcript's cwd — i.e. possibly a
//! freshly cloned repo whose `.git/config` we have never read. Two of git's config
//! keys execute programs on a plain `git status`:
//!
//! - `core.fsmonitor` — a command run to learn which files changed
//! - `core.hooksPath` / hooks — run by the commands that fire them
//!
//! A read-only probe needs neither, so they are switched off per invocation with `-c`
//! (which beats every config file). `GIT_OPTIONAL_LOCKS=0` is kept: it stops `status`
//! rewriting the index, which would reset the idle clock `prune` reads and contend with
//! the Claude working in that tree.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// A `git -C <dir> <args…>` that takes no lock, writes nothing and runs no
/// config-defined program.
pub fn read_only(dir: &str, args: &[&str]) -> Command {
    let mut c = Command::new("git");
    c.env("GIT_OPTIONAL_LOCKS", "0")
        .args([
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .arg("-C")
        .arg(dir)
        .args(args);
    c
}

/// Run `cmd`, killing it after `limit`. `None` on spawn failure, non-zero exit or
/// timeout. For commands that touch the network: a dead remote must not hang
/// `hive wt new` for minutes. Prompts are disabled so a credential helper can't wait on a
/// terminal nobody is watching.
pub fn run_capped(mut cmd: Command, limit: Duration) -> Option<String> {
    cmd.env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_SSH_COMMAND", "ssh -o BatchMode=yes")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = cmd.spawn().ok()?;
    let start = Instant::now();
    loop {
        match child.try_wait().ok()? {
            Some(status) => {
                if !status.success() {
                    return None;
                }
                let mut out = String::new();
                if let Some(mut so) = child.stdout.take() {
                    use std::io::Read;
                    so.read_to_string(&mut out).ok()?;
                }
                return Some(out);
            }
            None if start.elapsed() >= limit => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            None => std::thread::sleep(Duration::from_millis(25)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_only_pins_fsmonitor_and_hooks_off() {
        let c = read_only("/x", &["status"]);
        let args: Vec<_> = c
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert!(args.windows(2).any(|w| w == ["-c", "core.fsmonitor=false"]));
        assert!(args
            .windows(2)
            .any(|w| w == ["-c", "core.hooksPath=/dev/null"]));
        // -c options must precede the subcommand or git treats them as its arguments.
        let sub = args.iter().position(|a| a == "status").unwrap();
        let cfg = args
            .iter()
            .position(|a| a == "core.fsmonitor=false")
            .unwrap();
        assert!(cfg < sub);
    }

    #[test]
    fn run_capped_returns_output_and_kills_the_slow() {
        let mut ok = Command::new("echo");
        ok.arg("hi");
        assert_eq!(run_capped(ok, Duration::from_secs(5)).unwrap().trim(), "hi");

        let mut slow = Command::new("sleep");
        slow.arg("5");
        let t = Instant::now();
        assert!(run_capped(slow, Duration::from_millis(150)).is_none());
        assert!(t.elapsed() < Duration::from_secs(2));

        let fail = Command::new("false");
        assert!(run_capped(fail, Duration::from_secs(5)).is_none());
    }

    #[test]
    fn fsmonitor_hook_in_repo_config_does_not_run() {
        // A repo whose config names an fsmonitor program: a plain `git status`
        // would execute it; the hardened probe must not.
        let dir = std::env::temp_dir().join(format!("hive-gitcmd-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let d = dir.to_str().unwrap();
        let g = |a: &[&str]| {
            Command::new("git")
                .arg("-C")
                .arg(d)
                .args(a)
                .output()
                .unwrap();
        };
        g(&["init", "-q"]);
        let marker = dir.join("RAN");
        let hook = dir.join("fsm.sh");
        std::fs::write(&hook, format!("#!/bin/sh\ntouch '{}'\n", marker.display())).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        g(&["config", "core.fsmonitor", hook.to_str().unwrap()]);
        let out = read_only(d, &["status", "--porcelain"]).output().unwrap();
        assert!(out.status.success());
        assert!(
            !marker.exists(),
            "fsmonitor program ran during a read-only probe"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
