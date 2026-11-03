# Contributing

Thanks for your interest in hive! This doc covers dev setup, the usual preflight checks, and a few conventions that will make your PRs land quickly.

The [README](./README.md) is for users. For architectural context before diving into the code, read [CLAUDE.md](./CLAUDE.md).

## Dev environment

- **macOS** (Apple Silicon or Intel). Linux compiles via CI as a regression canary but the signature features (Chrome tabs, Ghostty pane spread, port detection) are mac-only.
- **Rust 1.82+** via [rustup](https://rustup.rs).
- **tmux** and the **Claude Code CLI** on your `PATH` for manual testing.

Clone and build:

```bash
git clone https://github.com/emiperez95/hive
cd hive
cargo build
```

## Preflight (always run before opening a PR)

```bash
cargo build
cargo clippy -- -D warnings
cargo test
```

All three must pass. CI runs the same trio on macOS + Linux (`.github/workflows/ci.yml`).

Tests are:

- **~265 unit tests** in-module (`#[cfg(test)]` blocks).
- **20 CLI smoke tests** in `tests/cli_smoke.rs` that run the actual binary against tempdirs.
- **Doctests** in module-level comments.

Install the dev build locally for manual testing:

```bash
cargo install --path . --root ~/.local
```

Then exercise your change via `hive setup`, `hive <subcommand>`, or the TUI. For isolated setup/uninstall tests, use the pattern from `/tmp/hive-a5/env.sh` (see [docs/publish-plan.md](./docs/publish-plan.md) — temporary HOME + isolated tmux socket).

## Commit + PR conventions

- **Commit messages**: imperative mood, short first line (< 72 chars), blank line, then a body explaining *why* if it's non-obvious. Match the style in `git log`.
- **One logical change per commit.** If a feature needs a refactor first, separate them.
- **No commit bodies that restate the diff.** Describe the motivation, the approach, and any tradeoffs.
- **PR title** mirrors the main commit. PR description uses the `.github/pull_request_template.md` stub (Summary + Test plan).
- **Never bypass the pre-commit hook** (`.githooks/pre-commit`) with `--no-verify`.
- **Never force-push shared branches** (main, release branches).

If you're touching something that interacts with `~/.claude/`, `~/.hive/`, tmux bindings, or Claude Code hook entries — please add a manual verification note in the PR body with the commands you ran.

## Code style

The codebase is intentionally conservative:

- **No async runtime.** Everything is synchronous. Don't add `tokio`, `async-std`, etc. The web server (`tiny_http`) is a blocking single-thread accept loop.
- **No locking.** State is passed via atomic file writes (write `.tmp`, rename). See `src/common/persistence.rs` and `src/ipc/messages.rs`.
- **Platform gating** uses `#[cfg(target_os = "macos")]` with empty stubs on other platforms. Don't leak macOS-only APIs into general code.
- **`anyhow::Result`** for error handling throughout. User-facing errors go through `.context("...")` so the message stays readable.
- **Default to no comments.** Reserve them for non-obvious *why* or a subtle invariant. Don't restate what the code does.
- **No premature abstraction.** Three similar lines beat a helper that'll only ever have one caller.

## Project structure

See the [Project Structure](./CLAUDE.md#project-structure) section of CLAUDE.md for a current map of `src/`. At the top level:

- `src/main.rs` — thin arg-parsing + dispatch.
- `src/cli/` — subcommand handlers.
- `src/common/` — shared utilities (tmux, process, ports, persistence, projects, worktrees).
- `src/tui/` — ratatui rendering and event loop.
- `src/serve/` — the `hive web` HTTP server + embedded HTML.
- `src/daemon/` — hook event handling + native notifications.
- `src/ipc/` — serialized state (`state.json`).

## Adding a terminal emulator backend

Everything that scripts the terminal emulator lives in `src/common/terminal.rs`, guarded behind
`#[cfg(target_os = "macos")]`. Ghostty is the only backend, but `Terminal` is an enum on purpose:
to add another, add a variant and let the compiler list what is missing.

1. **Add a variant** to `Terminal`. Every `match` on it then fails to compile until it has an arm:
   - `app()` — the application name (AppleScript addresses it).
   - `pane_count()` — the number of panes/splits in the current tab.
   - `spread_panes(n)` — open `n` new panes, each running `hive start` with the current `PATH`.
   - `collapse_panes()` — close every pane except the focused one.
2. **Teach detection** to recognise it: `from_term_program` (`$TERM_PROGRAM`), `from_tmux_client`
   (the tmux client's `#{client_termtype}`, i.e. the terminal's XTVERSION reply) and the
   `$HIVE_TERMINAL` override in `detect()`. Most calls run inside tmux or the detached `__hive_web`
   session, where `$TERM_PROGRAM` says nothing, so the tmux client is the path that matters.
3. **Add tests** beside the existing ones for the new classifier arms.

`hive spread` / `hive collapse` and the TUI's `L` key all go through `run_spread` / `run_collapse`
in `src/cli/session.rs`, which call `terminal::spread_panes` / `collapse_panes` (and handle the tmux
layout and activity log) — nothing else to wire up.

The terminal sidebar's split helpers (`open_sidebar_split`, `resize_sidebar_split`,
`focus_work_split`) are Ghostty-only today: they find the sidebar's split by its terminal title
(`SIDEBAR_TITLE`). A backend that lacks them just can't open the sidebar automatically —
`hive sidebar` itself is a plain terminal program and runs in a split you make by hand.

Automation APIs to look at:

- **Ghostty**: AppleScript (`tell application "Ghostty"`); there is no CLI for splits.
- **WezTerm**: CLI (`wezterm cli split-pane`) or Lua scripting.
- **Kitty**: Remote control protocol (`kitten @ launch`, `kitten @ close-window`).
- **Alacritty / tmux-only**: No native terminal splits — nothing to script.

Gotcha: new panes need the full `PATH` to find `tmux`. Ghostty splits start with launchd's `PATH`
(no Homebrew), so hive passes `env PATH='...'` explicitly. Other terminals may or may not have this
issue.

## Filing bugs / feature requests

Use the issue templates (`.github/ISSUE_TEMPLATE/`):

- **Bug**: include `hive --version`, `tmux -V`, macOS version + arch, and `~/.cache/hive/debug.log` if you ran with `hive --debug`.
- **Feature**: describe the use case in terms of a concrete scenario before sketching the UI.

Questions or discussion → [Discussions](https://github.com/emiperez95/hive/discussions).

## License

By contributing you agree that your changes are licensed under the MIT license (same as the rest of the repo).
