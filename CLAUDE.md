# hive

Interactive Claude Code session dashboard for tmux. Runs as a popup (`prefix + d`) to monitor, switch between, and manage parallel Claude sessions.

## Quick Reference

```bash
cargo test                # 72 unit tests
cargo build               # dev build
cargo clippy -- -D warnings
cargo install --path . --root ~/.local  # install binary
hive setup                # register hooks + tmux keybinding
```

## Architecture

File-based state, no daemon, no async runtime:

```
Claude hook fires → hive hook <event>  → reads stdin JSON, updates state.json, sends notification
TUI (1s refresh)  → reads state.json   + tmux sessions + sysinfo + libproc + Chrome AppleScript
```

**Data flow**: `hive hook` writes `state.json` atomically (write .tmp, rename). TUI reads it each refresh cycle. No locking needed.

## CLI

```
hive                    # open TUI (default)
hive --detail           # open TUI with detail view for current session
hive --debug            # enable debug logging
hive hook <event>       # process hook event from stdin (Stop, PreToolUse, PostToolUse, PermissionRequest, UserPromptSubmit, Notification)
hive setup              # register hooks in ~/.claude/settings.json + tmux keybind
hive cycle-next         # switch to next tmux session (skipping skipped)
hive cycle-prev         # switch to previous tmux session
hive connect <key>      # create/attach tmux session for a registered project
hive project add <key>  # add a project to the registry (supports all config flags)
hive project remove <key> # remove a project from the registry
hive project list       # list all configured projects
hive project import     # import projects from sesh.toml
hive wt new <project> <branch>  # create worktree + tmux session (with hooks)
hive wt delete <project> <branch>  # delete worktree + session + branch
hive wt list [project]  # list registered worktrees with tmux status
```

## Project Structure

```
src/
├── main.rs                 CLI entry point, key handlers, hook/setup/cycle subcommands
├── lib.rs                  exports common module for bench binary
├── bin/bench.rs            benchmark tool
├── common/
│   ├── types.rs            TmuxSession, SessionInfo, ClaudeStatus, ProcessInfo, PERMISSION_KEYS
│   ├── tmux.rs             tmux command helpers (list-sessions, switch, send-keys, kill)
│   ├── process.rs          Claude process detection, process tree traversal (sysinfo)
│   ├── ports.rs            listening port detection via libproc (macOS only, #[cfg] guarded)
│   ├── chrome.rs           Chrome tab detection via AppleScript (macOS only, #[cfg] guarded)
│   ├── jsonl.rs            JSONL parsing for Claude status from ~/.claude/projects/
│   ├── persistence.rs      file persistence for all txt-based state (parked, todos, muted, etc.)
│   ├── projects.rs         project registry (projects.toml), replaces sesh dependency
│   ├── worktree.rs         worktree lifecycle (types, state, git ops, file ops, hooks, memory seed)
│   └── debug.rs            debug logging to cache dir
├── daemon/
│   ├── hooks.rs            handle_hook_event(): maps HookEvent → SessionState updates
│   └── notifier.rs         platform-native notifications (terminal-notifier/osascript/notify-send)
├── ipc/
│   └── messages.rs         HookEvent, SessionState, HookState (load/save), SessionStatus
└── tui/
    ├── app.rs              App struct, refresh(), session management, search, parking, todos
    └── ui.rs               ratatui rendering (list, detail, parked, search, help, input modals)
```

## Key Types

- `HookState` (ipc/messages.rs) — `HashMap<session_id, SessionState>`, serialized to state.json
- `SessionState` — session_id, cwd, status, needs_attention, last_activity
- `SessionStatus` — Working, Waiting, NeedsPermission, EditApproval, PlanReview, QuestionAsked
- `App` (tui/app.rs) — all TUI state: sessions, selection, input mode, parked, todos, flags
- `SessionInfo` (common/types.rs) — enriched session data for display (processes, ports, status)
- `ClaudeStatus` (common/types.rs) — TUI-side status enum mapped from SessionStatus
- `ProjectRegistry` (common/projects.rs) — `HashMap<name, ProjectConfig>`, loaded from projects.toml
- `ProjectConfig` (common/projects.rs) — project definition (emoji, path, startup, ports, files, hooks_dir, etc.)
- `WorktreeState` (common/worktree.rs) — `HashMap<"{project}/{branch}", WorktreeEntry>`, persisted to worktrees.json
- `WorktreeEntry` (common/worktree.rs) — worktree record (project_key, branch, type, path, session_name, metadata, created_at)

## Config Directory

`~/Library/Application Support/hive/` (macOS) or `~/.config/hive/` (Linux):

| File | Format | Purpose |
|------|--------|---------|
| projects.toml | TOML | project registry (name, path, emoji, startup, ports, etc.) |

## Cache Directory

`~/Library/Caches/hive/` (macOS) or `~/.cache/hive/` (Linux):

| File | Format | Purpose |
|------|--------|---------|
| state.json | JSON | hook state (session statuses) |
| parked.txt | lines: `name\tnote` | parked sessions |
| todos.txt | TOML | per-session todo lists |
| muted.txt | lines | muted session names |
| auto-approve.txt | lines | auto-approve session names |
| skipped.txt | lines | skipped-from-cycling session names |
| restore.txt | lines | sessions to restore |
| muted-global | empty file | global mute flag |
| worktrees.json | JSON | registered worktrees (project, branch, path, session, metadata) |
| debug.log | text | debug log (--debug) |

## Platform Guards

macOS-only features use `#[cfg(target_os = "macos")]` with empty stubs for other platforms:
- `ports.rs`: `get_listening_ports_for_pids()` — uses `libproc`
- `chrome.rs`: `get_chrome_tabs()`, `open_chrome_tab()`, `focus_chrome_tab()` — uses AppleScript

## Key Handling

All key input is in `main.rs::run_tui()`. Events are filtered to `KeyEventKind::Press` only (crossterm 0.28 sends release events that break Esc in tmux popups). The if/else chain priority:

1. Help screen → `?`/Esc dismiss, `Q` quit
2. Parked view → navigate, unpark, back
3. ParkNote input → text entry modal
4. AddTodo input → text entry modal
5. Search mode → filter, navigate, select
6. Detail view → todos, ports, switch, park, flags
7. Parked detail → unpark, back
8. Normal list → navigate, switch (exits app), approve permissions, search, quit

Switching sessions (1-9, Enter in detail, connect project) always exits the app.

## Tmux Integration

- `prefix + s` — hive popup (list view)
- `prefix + d` — hive popup (detail view for current session)
- `Ctrl+n` / `Ctrl+p` — cycle next/prev session
- Configured in `~/.tmux.conf`, also set by `hive setup`

## Testing

72 tests in common/ modules. No TUI tests (interactive). Run with `cargo test`.

## Conventions

- No `tokio` or async — everything is synchronous
- Atomic file writes for state.json (write to .tmp, rename)
- `anyhow::Result` for error handling throughout
- `sysinfo::System` is kept alive in `App` for CPU delta accuracy (needs two refresh_all calls)
- Stale sessions cleaned up after 10 minutes of inactivity (in hook handler)

## Worktree Hooks

Project hooks live in `<project_root>/.hive/hooks/` (or custom `hooks_dir`). Shell scripts named `<hook>.sh`:

| Hook | When | Use case |
|------|------|----------|
| pre-create | Before git worktree add | Validation, pre-checks |
| post-worktree | After git worktree add | Port allocation, resource setup |
| post-copy | After file copy/symlink + memory seed | Database setup, env config |
| post-setup | After tmux session + registry | Final setup steps |
| pre-delete | Before cleanup starts | Database teardown, resource cleanup |
| post-delete | After full cleanup | Final teardown steps |

**Hook env vars**: `HIVE_PROJECT_KEY`, `HIVE_BRANCH`, `HIVE_WORKTREE_PATH`, `HIVE_PROJECT_ROOT`, `HIVE_SESSION_NAME`, `HIVE_WORKTREE_TYPE`, `HIVE_METADATA` (JSON), `HIVE_METADATA_FILE` (write path).

**Metadata protocol**: Hooks write JSON to `$HIVE_METADATA_FILE`. If `session_name` key is present, it overrides the default. All keys are stored in `worktrees.json` and passed to future hooks.
