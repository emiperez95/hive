# hive

Interactive Claude Code session dashboard for tmux. Runs as a popup (`prefix + d`) to monitor, switch between, and manage parallel Claude sessions.

## Quick Reference

```bash
cargo test                # 90 unit tests
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
hive web                                    # start web dashboard on port 8375
hive web --dev                              # dev mode: serve HTML from disk (live reload)
hive web --tts-host <url>                   # enable TTS read-aloud via TTSQwen service
hive web --port <N>                         # custom port (default: 8375)
```

## Auth Profiles

Per-project Claude credentials via `CLAUDE_CONFIG_DIR`. Each profile (`~/.claude-{name}/`) has its own OAuth identity and conversation history, with shared resources (agents, commands, hooks, skills, plugins, settings) symlinked back to `~/.claude/`.

Set `auth_profile = "work"` on a project in `projects.toml` → hive passes `-e CLAUDE_CONFIG_DIR=~/.claude-work` to `tmux new-session` → Claude uses the work identity. Worktrees inherit the parent project's profile.

JSONL conversation lookup (`jsonl.rs`) searches across all `~/.claude*/projects/` dirs, so the TUI and web dashboard display conversations regardless of which profile created them.

See `docs/claude-auth-profiles.md` for full setup guide.

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
│   ├── tmux.rs             tmux command helpers (list-sessions, switch, send-keys, kill, resolve_tmux_path, set_all_sessions_layout)
│   ├── process.rs          Claude process detection, process tree traversal (sysinfo)
│   ├── ports.rs            listening port detection via libproc (macOS only, #[cfg] guarded)
│   ├── chrome.rs           Chrome tab detection via AppleScript (macOS only, #[cfg] guarded)
│   ├── iterm.rs            iTerm2 pane spread/collapse via AppleScript (macOS only, #[cfg] guarded)
│   ├── jsonl.rs            JSONL parsing for Claude status + conversation extraction from ~/.claude/projects/ (searches all auth profiles)
│   ├── persistence.rs      file persistence for all txt-based state (favorites, todos, muted, etc.)
│   ├── projects.rs         project registry (projects.toml), replaces sesh dependency
│   ├── worktree.rs         worktree lifecycle (types, state, git ops, file ops, hooks, memory seed)
│   └── debug.rs            debug logging to cache dir
├── daemon/
│   ├── hooks.rs            handle_hook_event(): maps HookEvent → SessionState updates
│   └── notifier.rs         platform-native notifications (terminal-notifier/osascript/notify-send)
├── ipc/
│   ├── messages.rs         HookEvent, SessionState, HookState (load/save), SessionStatus
│   └── remote_protocol.rs  Wire protocol types (RemoteSessionData, ConversationMessage, ToolSummary)
├── serve/
│   ├── mod.rs              module registration (server, web, protocol)
│   ├── protocol.rs         re-exports from ipc/remote_protocol.rs
│   ├── server.rs           stdio server + gather_session_data() shared by TUI and web
│   ├── web.rs              HTTP web server (tiny_http), API endpoints, TTS proxy
│   └── web.html            embedded mobile-first SPA (HTML/CSS/JS)
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
- `ProjectConfig` (common/projects.rs) — project definition (emoji, path, startup, ports, files, hooks_dir, auth_profile, etc.)
- `WorktreeState` (common/worktree.rs) — `HashMap<"{project}/{branch}", WorktreeEntry>`, persisted to worktrees.json
- `WorktreeEntry` (common/worktree.rs) — worktree record (project_key, branch, type, path, session_name, metadata, created_at)
- `RemoteSessionData` (ipc/remote_protocol.rs) — session data for wire protocol (name, status, cpu, ports, pane, skipped, messages)
- `ConversationMessage` (ipc/remote_protocol.rs) — chat message with role, text, and tool summaries
- `ToolSummary` (ipc/remote_protocol.rs) — compact tool use info (name, summary, detail for modal)

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

**Tmux pane layout adjustment**: Spread and collapse also rearrange tmux panes within each session to optimize for the available space. Only windows with 2 or 3 panes are affected (1 or 4+ are left untouched):

| Panes | Spread (narrow iTerm columns) | Collapse (full width) |
|-------|-------------------------------|----------------------|
| 2 | top 70% / bottom 30% | left 70% / right 30% |
| 3 | top 70% / bottom two side-by-side in 30% | left 70% / right two stacked in 30% |

Layout logic lives in `tmux.rs::set_all_sessions_layout()`.

## Chrome Integration

Hive detects Chrome tabs matching a session's listening ports (`localhost:PORT`, `127.0.0.1:PORT`, `[::1]:PORT`).

Uses **JXA (JavaScript for Automation)** instead of AppleScript because Chrome's AppleScript dictionary only exposes windows from the main profile. JXA sees all windows across all profiles and incognito.

- **Detail view**: Chrome tab titles shown next to matching ports (fetched once on entering detail view)
- **`O` key** (detail view): Focus all Chrome windows/tabs matching the session's ports. Uses `AXRaise` via System Events to bring only matched windows to front (other Chrome windows may still appear behind due to macOS limitations)
- **`Enter` on a port** (detail view): Focus the matching Chrome tab, or open `localhost:PORT` if no tab exists
- Chrome tabs are fetched **on-demand** (not every refresh cycle) to avoid spawning `osascript` every second

## Web Dashboard (`hive web`)

Mobile-first web app for monitoring and interacting with Claude sessions from a phone browser. Runs a local HTTP server using `tiny_http` (sync, no async runtime).

```
hive web                                        # start on default port 8375
hive web --dev                                  # serve web.html from disk (edit + refresh)
hive web --tts-host http://10.18.1.2:9800       # enable TTS read-aloud via TTSQwen service
hive web --port <N>                             # custom port (default: 8375)
hive web --dev --tts-host http://10.18.1.2:9800 # both
```

**Architecture:**

```
┌─────────────────┐     ┌──────────────────────────────────────┐
│  Data Thread     │     │  HTTP Thread (main, blocking recv)   │
│  (1s refresh)    │     │                                      │
│  sysinfo.refresh │     │  GET /             → embedded HTML   │
│  HookState::load │     │  GET /api/sessions → session JSON    │
│  gather_sessions │     │  GET /api/messages → conversation    │
│  → Arc<Mutex>    │     │  GET /api/config   → {tts: bool}    │
│                  │     │  POST /api/send    → tmux send-keys  │
└─────────────────┘     │  POST /api/tts-hls → HLS via TTS    │
                        │  GET /hls/*        → proxy segments  │
                        └──────────────────────────────────────┘
```

**API endpoints:**

| Method | Path | Description |
|--------|------|-------------|
| GET | `/` | Serve embedded HTML (or from disk in `--dev` mode) |
| GET | `/api/sessions` | Session list with status, CPU, ports, skipped flag (polled every 1.5s) |
| GET | `/api/messages?session=X` | Full conversation for a session (user + assistant + tool uses) |
| GET | `/api/config` | Feature flags (`{"tts": true/false}`) |
| GET | `/api/projects` | All registered projects with exists flag |
| GET | `/api/session-info?session=X` | Enriched session data: CWD, ports, processes, flags, todos |
| POST | `/api/send` | Send text to session: `{"session": "...", "text": "..."}` |
| POST | `/api/tts-hls` | Create HLS TTS session, waits for first segment: `{"text": "...", ...}` |
| POST | `/api/tts-cancel` | Cancel TTS generation: `{"session_id": "..."}` |
| POST | `/api/toggle-flag` | Toggle favorite/auto_approve/skip: `{"session": "...", "flag": "..."}` |
| POST | `/api/todos` | Manage todos: `{"session": "...", "action": "add|done|delete", ...}` |
| POST | `/api/connect` | Create/attach session: `{"session_name": "..."}` |
| POST | `/api/kill-session` | Kill tmux session (with frontend confirmation): `{"session": "..."}` |
| GET | `/hls/{id}/playlist.m3u8` | Proxy HLS playlist from TTS server (same-origin for iOS) |
| GET | `/hls/{id}/*.m4s` | Proxy HLS fMP4 segments from TTS server |

**Frontend features (web.html):**

- Light theme based on Google Stitch designs (Inter font, Material Symbols icons, glass blur effects)
- Session list: emoji in rounded squares, status labels (Idle=green, Busy=red), CPU/mem, todo count badges
- Swipe left on session items to skip/unskip
- Floating + button opens session picker (search/filter projects, connect/create)
- Skipped sessions in separate section with solid gray background
- Full conversation view with markdown rendering (headers, bold, italic, code blocks, lists, links, tables)
- Syntax highlighting via Prism.js CDN (JS, TS, Rust, Python, Bash, YAML, JSON, TOML)
- Tool use cards (Bash, Write, Edit, Read, Grep, Glob, Agent) with expandable detail modals
- Styled tool modals: dark terminal block for Bash (copy button), unified LCS diff for Edit, file viewer for Write/Read
- Tappable session header opens info modal: CWD, ports, processes, flag toggles (favorite, auto-approve, skip), todo management (add/done/delete), kill session button
- Quick action buttons (Approve/Reject/yes) — only shown when session needs attention
- Text input with Send button for typing messages to sessions
- TTS buttons (only when `--tts-host` configured):
  - **Read Last** — reads the most recent assistant message aloud
  - **TLDR** — summarizes all assistant messages + tool uses since user's last message into a spoken briefing
- iOS keyboard handling via `visualViewport` API (body is `position: fixed`, height set by JS)
- Browser back gesture navigation via History API (all modals use pushState)
- Auto-scroll to bottom, preserves scroll position when reading older messages
- Jump-to-bottom button appears when scrolled up, bottom bar auto-hides on scroll
- Consecutive same-role messages collapse the role label
- Smart re-rendering: session list and messages only update on actual data changes (JSON comparison)
- Auto-approved sessions show as Busy (not Permission) since Claude auto-approves before the UI updates
- Honeycomb app icon (SVG source in `assets/icon.svg`, embedded as favicon + iOS touch icon)

**JSONL conversation extraction (`jsonl.rs`):**

- `get_conversation_messages(cwd)` — reads the full JSONL file, extracts all user + assistant messages
- Handles both string content (user messages) and array content blocks (assistant messages)
- Extracts tool_use blocks with per-tool summaries (command, filename, pattern, etc.)
- UTF-8 safe string truncation for tool details

**Process tree traversal:**

- Uses `ps -eo pid,ppid` instead of `sysinfo` for accurate parent-child relationships
- `sysinfo` caches stale/dead processes on macOS, inflating CPU/memory counts by 10-50x
- Resources counted only from the Claude pane's process tree (not all session panes)

**TTS integration:**

- Uses HLS streaming via hls.js (12KB CDN) for ~3x faster time-to-audio vs full WAV download
- Flow: POST `/api/tts-hls` → creates HLS session on TTS server → waits for first segment → returns playlist URL → hls.js handles segment fetching/playback
- HLS segments proxied through hive at `/hls/*` (same-origin avoids iOS Safari CORS issues)
- fMP4 segments with AAC-LC audio at 44100Hz stereo
- Cancel endpoint (`POST /api/tts-cancel`) stops TTS generation on navigation/stop
- Default config: Michael Caine voice (`voice: "michael_caine"`), English, 1.0x speed, `summarize: true`
- iOS audio unlock: plays silent WAV synchronously in tap handler before async HLS fetch
- Logs TTS session creation time and first-segment latency to stderr

**Dev mode (`--dev`):**

- Reads `src/serve/web.html` from disk on every `GET /` request
- Edit HTML/CSS/JS → refresh phone browser → see changes (no recompile needed)
- Falls back to embedded HTML if file not found
- For web development without affecting installed binary: `cargo run -- web --dev`

## Tmux Integration

- `prefix + s` — hive popup (list view)
- `prefix + d` — hive popup (detail view for current session)
- `Ctrl+n` / `Ctrl+p` — cycle next/prev session
- Configured in `~/.tmux.conf`, also set by `hive setup`

## Testing

90 tests in common/ modules. No TUI tests (interactive). Run with `cargo test`.

## Conventions

- No `tokio` or async — everything is synchronous
- Atomic file writes for state.json (write to .tmp, rename)
- `anyhow::Result` for error handling throughout
- `sysinfo::System` is kept alive in `App` for CPU delta accuracy (needs two refresh_all calls)
- Stale sessions cleaned up after 10 minutes of inactivity (in hook handler)

## Session Naming

- **Projects**: `{emoji} {display_name|key}` — e.g. `🌳 Clear Session`, `🐝 hive`
- **Worktrees (default type)**: `{emoji} [{project_key}] {branch}` — e.g. `🌳 [clear-session] CSD-2527`
- **Worktrees (non-default type)**: `{emoji} [{project_key}] {type}-{branch}` — e.g. `🌳 [clear-session] spike-CSD-2597`

The `[project_key]` tag identifies which project a worktree belongs to. The `worktree` type prefix is omitted since it's the default; other types (spike, feature, etc.) are shown.

On first TUI launch after upgrade, old-format names (`{emoji} {type}-{branch}`) are automatically migrated: worktrees.json entries are updated, live tmux sessions are renamed, and all persistence files (favorites, skipped, auto-approve, restore, todos) are updated.

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
