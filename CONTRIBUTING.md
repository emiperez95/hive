# Contributing

Thanks for your interest in hive! This doc covers dev setup, the usual preflight checks, and a few conventions that will make your PRs land quickly.

The [README](./README.md) is for users. For architectural context before diving into the code, read [CLAUDE.md](./CLAUDE.md).

## Dev environment

- **macOS** (Apple Silicon or Intel). Linux compiles via CI as a regression canary but the signature features (Chrome tabs, iTerm pane spread, port detection) are mac-only.
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

The iTerm2 pane spread/collapse feature lives in `src/common/iterm.rs` and is guarded behind `#[cfg(target_os = "macos")]`. To add support for another terminal emulator:

1. **Create a new module** (e.g., `src/common/wezterm.rs` or `src/common/kitty.rs`) implementing:
   - `get_pane_count() -> usize` — return the number of panes/splits in the current tab/window.
   - `spread_panes(n: usize) -> bool` — open `n` new panes, each running `hive start` with the current PATH.
   - `collapse_panes() -> bool` — close all panes except the current one.
2. **Register the module** in `src/common/mod.rs`.
3. **Wire it up** in `src/cli/session.rs` (`run_spread` / `run_collapse`) and in the TUI's `L` key handler at `src/tui/event_loop.rs`.

Automation APIs to look at:

- **iTerm2**: AppleScript (`tell application "iTerm2"`).
- **WezTerm**: CLI (`wezterm cli split-pane`) or Lua scripting.
- **Kitty**: Remote control protocol (`kitten @ launch`, `kitten @ close-window`).
- **Alacritty / tmux-only**: No native terminal splits — fall back to tmux splits instead.

Gotcha: new panes need the full `PATH` to find `tmux`. iTerm2 split panes get a minimal environment, so hive passes `env PATH='...'` explicitly. Other terminals may or may not have this issue.

## Filing bugs / feature requests

Use the issue templates (`.github/ISSUE_TEMPLATE/`):

- **Bug**: include `hive --version`, `tmux -V`, macOS version + arch, and `~/.cache/hive/debug.log` if you ran with `hive --debug`.
- **Feature**: describe the use case in terms of a concrete scenario before sketching the UI.

Questions or discussion → [Discussions](https://github.com/emiperez95/hive/discussions).

## License

By contributing you agree that your changes are licensed under the MIT license (same as the rest of the repo).
