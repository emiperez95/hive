# Changelog

All notable changes to hive are recorded here. Format loosely follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **`prefix + a` / `hive --project-detail`** — open the current window's *project* detail
  straight from tmux, alongside `prefix + s` (list) and `prefix + d` (conversation detail). The
  project resolves from the current window's conversation, or — when the window isn't running
  Claude — from the pane's cwd. A worktree resolves up to its project. `hive setup` registers
  and reports the binding; `hive uninstall` already removed it.
- **Frozen conversations are sectioned in the project detail** under a `💤 Frozen (n)` header,
  with `Closed (n)` marking where the parked ones end.

### Changed

- **Project detail lists worktrees *after* conversations**, and omits the section entirely when
  the project has none. The cursor order follows the drawn order: todos → conversations →
  worktrees.

### Fixed

- **Frozen conversations were effectively invisible in the project detail.** They sorted below
  every plain closed conversation, so the one you froze in order to come back to it fell past
  the 5-row page cap — on the very screen whose title bar counts it. They now sort directly
  under the live rows.
- **README documented the retired classic TUI.** The keyboard-shortcut tables still listed
  single-keypress permission approval (never ported to the conversation-first TUI — see
  `docs/permission-approve-reject.md`) and flags that no longer exist (`hive -w`). Rewritten
  against the shipped key map: list, project detail, conversation detail.
- **`.githooks/pre-commit` announced version bumps it never made.** It wrote the version with
  sed's `0,/re/` address, a GNU extension that BSD sed (macOS) accepts, exits 0 on, and ignores
  — so `Cargo.toml` sat at `0.1.0` while every commit reported a bump. Now uses awk and verifies
  the write, failing the commit if the version didn't land. Also swapped `cargo generate-lockfile`
  for `cargo update --workspace`, so unrelated third-party upgrades stop being swept into
  whatever commit is in flight.

## [0.1.0] — First public release

First tagged public release. macOS-only (Apple Silicon + Intel). Linux is deferred.

### Added

- **Interactive TUI** for monitoring multiple parallel Claude Code sessions across tmux.
- **Permission approval** via single-keypress (`y/z/x/w/v` once, `Y/Z/X/W/V` always).
- **Detail view** per session: todos, listening ports, Chrome tab matches, process tree.
- **Project registry** (`hive project add/list/remove/import`) stored in `~/.hive/projects.toml`.
- **Worktree lifecycle** (`hive wt new/delete/list`) with six project-level hook points.
- **Mobile web dashboard** (`hive web`) — phone-friendly UI with markdown conversation view, tool-use cards, message sending, permission approval, HLS-streamed TTS playback, per-session info modal.
- **Chrome tab matching** and **iTerm2 pane spread** (`hive spread N` / `hive collapse`).
- **Per-project auth profiles** via `CLAUDE_CONFIG_DIR` — multiple Claude identities on one machine.
- **Prebuilt binary install path**: `install.sh` downloads the latest tarball from GitHub Releases, no Rust toolchain required.
- **`hive update`** fetches the latest release, replaces the running executable, and re-runs `hive setup`.
- **`hive setup`** registers hooks in `~/.claude/settings.json`, installs the `janus-wt-portal` agent and `create-project` slash command, and offers to bind tmux keys (`prefix+s`, `prefix+d`, `Ctrl+n`, `Ctrl+p`).
- **Safety**: atomic writes + `.bak` backups for `settings.json`; malformed JSON moved to `.bak.malformed.<ts>`; `--yes` flag for scripted setup/uninstall; `HIVE_NO_NOTIFY=1` suppresses notifications.
- **Quick-start walkthrough** in the README, issue + PR templates, GitHub repo description + topics.

### Behavior notes

- Tmux bindings applied by `hive setup` are scoped to the live tmux server. Setup prints the `bind-key …` snippets — paste them into `~/.tmux.conf` to persist.
- Hooks in `~/.claude/settings.json` use the absolute path to the `hive` binary at the time setup was run. If you move the binary, re-run `hive setup` to refresh the paths.
- State lives in `~/.hive/` (projects, cache, todos). Uninstall does not remove this directory — `rm -rf ~/.hive/` for a full wipe.

[Unreleased]: https://github.com/emiperez95/hive/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/emiperez95/hive/releases/tag/v0.1.0
