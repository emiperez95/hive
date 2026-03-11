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
TUI (1s refresh)  → reads state.json   + tmux sessions + sysinfo + libproc + Chrome JXA (on-demand)
```

**Data flow**: `hive hook` writes `state.json` atomically (write .tmp, rename). TUI reads it each refresh cycle. No locking needed.

## CLI

```
hive                    # open TUI (default)
hive start              # auto-attach to first available session (or fall through to picker)
hive --detail           # open TUI with detail view for current session
hive --debug            # enable debug logging
hive hook <event>       # process hook event from stdin (Stop, PreToolUse, PostToolUse, PermissionRequest, UserPromptSubmit, Notification)
hive setup              # register hooks, agent, and tmux keybindings
hive update             # update to latest version from GitHub + re-run setup
hive --version          # print current version
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
hive todo list [--session <name>] [--done]  # list active (or completed) todos
hive todo next [--session <name>]           # print first active todo (exit 1 if none)
hive todo add <text> [--session <name>]     # add a todo
hive todo done [index] [--session <name>]   # mark todo as done (default: 1)
hive todo clear [--session <name>]          # clear completed todos
hive spread <N>                             # spread N sessions into vertical iTerm2 panes
hive collapse                               # collapse iTerm2 panes back to one
```

## Janus WT Portal

The **janus-wt-portal** agent (`.claude/agents/janus-wt-portal.md`) ships with hive and is installed by `hive setup`. It's the primary way to manage worktrees interactively — detects ticket mentions, extracts branch names, resolves the project from git remote, and runs `hive wt` commands.

Installed to `~/.claude/agents/janus-wt-portal.md` globally so it's available in all projects.

## Project Structure

```
src/
├── main.rs                 CLI entry point, key handlers, hook/setup/cycle subcommands
├── lib.rs                  exports common module for bench binary
├── bin/bench.rs            benchmark tool
├── common/
│   ├── types.rs            TmuxSession, SessionInfo, ClaudeStatus, ProcessInfo, PERMISSION_KEYS
│   ├── tmux.rs             tmux command helpers (list-sessions, switch, send-keys, kill, resolve_tmux_path)
│   ├── process.rs          Claude process detection, process tree traversal (sysinfo)
│   ├── ports.rs            listening port detection via libproc (macOS only, #[cfg] guarded)
│   ├── chrome.rs           Chrome tab detection via AppleScript (macOS only, #[cfg] guarded)
│   ├── iterm.rs            iTerm2 pane spread/collapse via AppleScript (macOS only, #[cfg] guarded)
│   ├── jsonl.rs            JSONL parsing for Claude status from ~/.claude/projects/
│   ├── persistence.rs      file persistence for all txt-based state (favorites, todos, muted, etc.)
│   ├── projects.rs         project registry (projects.toml), replaces sesh dependency
│   ├── worktree.rs         worktree lifecycle (types, state, git ops, file ops, hooks, memory seed)
│   └── debug.rs            debug logging to cache dir
├── daemon/
│   ├── hooks.rs            handle_hook_event(): maps HookEvent → SessionState updates
│   └── notifier.rs         platform-native notifications (terminal-notifier/osascript/notify-send)
├── ipc/
│   └── messages.rs         HookEvent, SessionState, HookState (load/save), SessionStatus
└── tui/
    ├── app.rs              App struct, refresh(), session management, search, favorites, todos
    └── ui.rs               ratatui rendering (list, detail, search, help, input modals)
```

## Key Types

- `HookState` (ipc/messages.rs) — `HashMap<session_id, SessionState>`, serialized to state.json
- `SessionState` — session_id, cwd, status, needs_attention, last_activity
- `SessionStatus` — Working, Waiting, NeedsPermission, EditApproval, PlanReview, QuestionAsked
- `App` (tui/app.rs) — all TUI state: sessions, selection, input mode, favorites, todos, flags
- `SessionInfo` (common/types.rs) — enriched session data for display (processes, ports, status)
- `ClaudeStatus` (common/types.rs) — TUI-side status enum mapped from SessionStatus
- `ProjectRegistry` (common/projects.rs) — `HashMap<name, ProjectConfig>`, loaded from projects.toml
- `ProjectConfig` (common/projects.rs) — project definition (emoji, path, startup, ports, files, hooks_dir, etc.)
- `WorktreeState` (common/worktree.rs) — `HashMap<"{project}/{branch}", WorktreeEntry>`, persisted to worktrees.json
- `WorktreeEntry` (common/worktree.rs) — worktree record (project_key, branch, type, path, session_name, metadata, created_at)

## Data Directory

All hive data lives under `~/.hive/`. The janus-wt-portal agent is installed to `~/.claude/agents/`.

```
~/.hive/
├── projects.toml              # project registry
├── cache/                     # runtime state
│   ├── state.json             # hook state (session statuses)
│   ├── worktrees.json         # registered worktrees
│   ├── favorites.txt           # favorite session names
│   ├── todos.txt              # per-session todo lists (active)
│   ├── todos-done.txt         # per-session completed todos
│   ├── muted.txt              # muted session names
│   ├── auto-approve.txt       # auto-approve session names
│   ├── skipped.txt            # skipped-from-cycling session names
│   ├── restore.txt            # sessions to restore
│   ├── muted-global           # global mute flag (empty file)
│   └── debug.log              # debug log (--debug)
└── projects/                  # per-project config
    └── {project_key}/
        ├── hooks/             # lifecycle hook scripts
        └── lib/               # shared shell libraries for hooks
```

## Platform Guards

macOS-only features use `#[cfg(target_os = "macos")]` with empty stubs for other platforms:
- `ports.rs`: `get_listening_ports_for_pids()` — uses `libproc`
- `chrome.rs`: `get_chrome_tabs()`, `open_chrome_tab()`, `focus_chrome_tab()`, `focus_all_matched_tabs()` — uses JXA (sees all Chrome profiles)
- `iterm.rs`: `get_iterm_pane_count()`, `spread_panes()`, `collapse_panes()` — uses AppleScript

## Key Handling

All key input is in `main.rs::run_tui()`. Events are filtered to `KeyEventKind::Press` only (crossterm 0.28 sends release events that break Esc in tmux popups). The if/else chain priority:

1. Help screen → `?`/Esc dismiss, `Q` quit
2. AddTodo input → text entry modal
3. SpreadPrompt → digit 1-9 triggers spread, Esc cancels
4. Search mode → filter, navigate, select
5. Detail view → todos, ports, switch, favorite, flags, `O` open Chrome tabs
6. Normal list → navigate, switch (exits app), approve permissions, search, `L` spread/collapse, quit

Switching sessions (1-9, Enter in detail, connect project) always exits the app.

## `hive start`

Auto-attach to a tmux session. Designed as iTerm2's startup command for new tabs/panes.

1. Find the first non-skipped session **not attached** to another client → `exec tmux attach`
2. If all non-skipped sessions are attached, attach to **any** non-skipped session (duplicates are fine)
3. If no sessions exist at all → fall through to TUI picker (search mode)

When the picker is used (case 3), selecting a session returns `PostAction::Attach(name)` which `exec`s into tmux after the TUI is cleaned up.

## `hive spread/collapse`

`hive spread N` opens N-1 new vertical iTerm2 panes via AppleScript (`split vertically`). Each new pane runs `env PATH='...' /path/to/hive start` — PATH is captured from the current process since iTerm split panes have minimal environment. Each `hive start` independently picks a session.

`hive collapse` closes all iTerm2 panes except the current one. Tmux sessions stay alive (just detached).

In the TUI, `L` toggles: if multiple panes exist → collapse, otherwise → show SpreadPrompt for digit input.

## Chrome Integration

Hive detects Chrome tabs matching a session's listening ports (`localhost:PORT`, `127.0.0.1:PORT`, `[::1]:PORT`).

Uses **JXA (JavaScript for Automation)** instead of AppleScript because Chrome's AppleScript dictionary only exposes windows from the main profile. JXA sees all windows across all profiles and incognito.

- **Detail view**: Chrome tab titles shown next to matching ports (fetched once on entering detail view)
- **`O` key** (detail view): Focus all Chrome windows/tabs matching the session's ports. Uses `AXRaise` via System Events to bring only matched windows to front (other Chrome windows may still appear behind due to macOS limitations)
- **`Enter` on a port** (detail view): Focus the matching Chrome tab, or open `localhost:PORT` if no tab exists
- Chrome tabs are fetched **on-demand** (not every refresh cycle) to avoid spawning `osascript` every second

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

Project hooks live in `~/.hive/projects/{project_key}/hooks/` (or custom `hooks_dir`). Shell scripts named `<hook>.sh`:

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
