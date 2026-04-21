# Security Policy

## Supported versions

Only the latest released version of hive receives security fixes. hive is pre-1.0, so non-trivial changes may land without deprecation notices — upgrade via `hive update` to stay current.

| Version | Supported |
| --- | --- |
| `v0.1.x` (latest) | ✅ |
| earlier / unreleased | ❌ |

## Reporting a vulnerability

Please **do not** open a public GitHub issue for security problems.

Preferred: use [GitHub's private vulnerability reporting](https://github.com/emiperez95/hive/security/advisories/new). That routes the report directly to the maintainer and keeps the discussion confidential until a fix is ready.

If that's not available, email the maintainer listed in `Cargo.toml` (`authors` field). Please include:

- A description of the issue and the impact.
- Steps to reproduce (ideally a minimal tracked project or repro script).
- The affected hive version (`hive --version`) and your OS/arch.
- Any proposed mitigation or patch.

I'll acknowledge within a few days. Once a fix is ready, I'll coordinate disclosure — credit will be given in the release notes and the published advisory unless you prefer to stay anonymous.

## Scope

What's in scope:

- The `hive` binary itself (TUI, CLI subcommands, hook processing, web dashboard).
- The `install.sh` installer and `hive update` download path.
- Generated content written to `~/.claude/settings.json`, `~/.claude/agents/`, `~/.claude/commands/`, `~/.hive/`, and the tmux server.

Out of scope:

- Vulnerabilities in upstream crates or external tools (`tmux`, `claude` CLI, `terminal-notifier`, `osascript`) — please report those to their maintainers.
- Non-security UX bugs — use the regular issue tracker.

Thanks for helping keep hive safe.
