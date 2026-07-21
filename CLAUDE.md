# hive

Interactive Claude Code session dashboard for tmux. Runs as a popup (`prefix + d`) to monitor, switch between, and manage parallel Claude sessions.

## Quick Reference

```bash
cargo test                # 255 tests (233 unit + 22 CLI smoke)
cargo build               # dev build
cargo clippy --all-targets -- -D warnings
cargo fmt                 # CI has a fmt gate — run before committing
cargo install --path . --root ~/.local  # install binary
hive setup                # register hooks + tmux keybinding
```

> `cargo test` prints ~454 passing because `common/` + `ipc/` compile into **both** the lib and
> bin targets and run twice. Distinct tests: 233 unit + 22 smoke.

## The TUI (conversation-first)

hive has a single interactive view: the **conversation-first** TUI in
`src/cli/conversations.rs`. Its base entity is the **Claude conversation** (UUID-keyed), so
closed/resumable and frozen conversations are first-class, not just live tmux sessions. `main.rs`
dispatches bare `hive` / `hive tui` to it; see **Conversations TUI** below.

The old **classic session-first TUI** (`src/tui/`) was retired in the cutover — `git log` for
its history. Its one non-ported feature, permission approve/reject from the dashboard, is
documented in `docs/permission-approve-reject.md` for future revival. The web dashboard
(`serve/`) has also been ported to the conversation model: it now projects the shared
`ConversationRegistry` (via `common/conversations.rs`) into its wire shapes, so both the TUI
and the web run off one gather. The old per-endpoint session gather (`gather_session_data`) and
`/api/sessions` are gone; `ClaudeStatus` survives only as the JSONL-parse intermediate the
registry's live-status recovery uses.

## Architecture

File-based state, no daemon, no async runtime:

```
Claude hook fires → hive hook <event>  → reads stdin JSON, updates state.json, sends notification
TUI (1s refresh)  → reads state.json   + tmux sessions + sysinfo + libproc + Chrome JXA (on-demand)
```

**Data flow**: `hive hook` writes `state.json` atomically (write .tmp, rename). TUI reads it each refresh cycle. No locking needed.

**Important**: `state.json` is *ephemeral* — the hook handler prunes entries after 10 min of
inactivity, so a long-idle-but-running Claude window often has **no** hook entry. Anything
that must see every live Claude window (the conversation model) cannot rely on it alone.

## CLI

```
hive                    # open conversations TUI (default; also prefix + s)
hive --detail           # conversations detail for the current window (prefix + d)
hive --picker           # conversations, start in Browse + search
hive --filter <q>       # conversations, start in Browse + search pre-filled with <q>
hive conversations      # explicit; --list for a static listing (also used when piped)
hive stats [--days N]   # usage summary from the activity log (default 7 days)
hive start              # auto-attach to first available session (or fall through to picker)
hive --detail           # open TUI with detail view for current session
hive --debug            # enable debug logging
hive hook <event>       # process hook event from stdin (Stop, PreToolUse, PostToolUse, PermissionRequest, UserPromptSubmit, Notification)
hive setup              # register hooks, agent, and tmux keybindings
hive uninstall          # remove hooks + tmux keybindings
hive update             # update to latest version from GitHub + re-run setup
hive --version          # print current version
hive cycle-next         # switch to next tmux session (skipping skipped)
hive cycle-prev         # switch to previous tmux session
hive cycle-free         # jump to next non-busy Claude window (current session first)
hive window-next        # switch to next tmux window in the current session
hive window-prev        # switch to previous tmux window in the current session
hive connect <key>      # create/attach tmux session for a registered project
hive project add <key>  # add a project to the registry (supports all config flags)
hive project remove <key> # remove a project from the registry
hive project archive <key>   # archive a project (hide from picker + default list)
hive project unarchive <key> # unarchive a project
hive project list [--all]    # list configured projects (--all includes archived)
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
├── main.rs                 CLI entry point: arg parsing + dispatch (~125 lines)
├── lib.rs                  exports common + ipc modules for the bench binary
├── bin/bench.rs            benchmark tool
├── cli/
│   ├── mod.rs              Args, Command, subcommand enums, PostAction
│   ├── conversations.rs    ★ conversation-first TUI (self-contained: gather, views, render, keys) — ~3.9k lines
│   ├── hook.rs             run_hook(): parse stdin JSON, update state, notify (honors muted projects)
│   ├── setup.rs            run_setup(), run_uninstall(), is_hive_hook_command()
│   ├── worktree.rs         run_wt_new/delete/list/import(): full worktree lifecycle
│   ├── project.rs          run_project_add/remove/list/import()
│   ├── todo.rs             run_todo*(), resolve_session()
│   ├── session.rs          run_cycle/spread/collapse/connect/start()
│   ├── stats.rs            run_stats(): usage summary from the activity log
│   └── update.rs           run_update(): self-update from GitHub
├── common/
│   ├── types.rs            TmuxSession/Window/Pane, ProcessInfo, ClaudeStatus (ClaudeStatus shared with serve/)
│   ├── registry.rs         ★ conversation model: Conversation, ConversationId, Lifecycle, TmuxPlacement,
│   │                         ConversationRegistry::from_shadow(), resolve_parent(), sidecar/overlay
│   ├── instances.rs        ★ per-WINDOW Claude detection (a session may host many); HookIndex pane→id resolve
│   ├── frozen.rs           freeze/thaw one Claude window (FrozenState/FrozenEntry, frozen.json)
│   ├── activity.rs         append-only activity log + open-windows snapshot; compute_stats()
│   ├── machine.rs          machine sleep/wake intervals from `pmset -g log` (macOS), for accurate usage time
│   ├── tmux.rs             tmux helpers (list-sessions/windows, switch, send-keys, kill, get_all_windows, layout)
│   ├── process.rs          Claude process detection, process tree, build_cmdline_map/parse_resume_id
│   ├── ports.rs            listening port detection via libproc (macOS only, #[cfg] guarded)
│   ├── chrome.rs           Chrome tab detection via AppleScript (macOS only, #[cfg] guarded)
│   ├── iterm.rs            iTerm2 pane spread/collapse via AppleScript (macOS only, #[cfg] guarded)
│   ├── jsonl.rs            JSONL parsing + transcript scan (all auth profiles), mtime-keyed scan cache
│   ├── persistence.rs      file persistence for all txt-based state (favorites, todos, muted, etc.)
│   ├── projects.rs         project registry (projects.toml), replaces sesh dependency
│   ├── worktree.rs         worktree lifecycle (types, state, git ops, file ops, hooks, memory seed)
│   └── debug.rs            debug logging to cache dir
├── daemon/
│   ├── hooks.rs            handle_hook_event(): maps HookEvent → SessionState updates
│   └── notifier.rs         platform-native notifications (terminal-notifier/osascript/notify-send)
├── ipc/
│   └── messages.rs         HookEvent, SessionState, HookState (load/save), SessionStatus
├── serve/                  web dashboard — projects the ConversationRegistry (conversation model)
│   ├── mod.rs              module registration (server, web, web_types)
│   ├── server.rs           gather_active_views()/build_conversation_views() — project the registry
│   ├── web.rs              HTTP web server (tiny_http), API endpoints, TTS proxy
│   ├── web.html            embedded mobile-first SPA (HTML/CSS/JS)
│   └── web_types.rs        SessionView, ProcessView, ConversationMessage, ToolSummary (web JSON)
└── tests/
    └── cli_smoke.rs        22 integration tests: CLI arg parsing, read-only commands, todo roundtrip
```

(The classic session-first TUI lived in `src/tui/` — removed in the cutover; see git history
and `docs/permission-approve-reject.md`.)

## Key Types

**Conversation model** (the re-rooted model behind `hive conversations`):

- `Conversation` (common/registry.rs) — the base entity, keyed by the Claude conversation UUID
  (the `<uuid>.jsonl` basename): id, cwd, lifecycle, status, last_activity, placement, parent,
  frozen, note, pinned, archived, title, auth_config_dir, and runtime-only `cpu`/`mem_kb`
- `ConversationId` — newtype over the UUID (machine-independent, unlike tmux names)
- `Lifecycle` — `Live` | `Closed`. **A live tmux placement is the SOLE discriminator** for Live
- `TmuxPlacement` — where a conversation currently runs (session_name, window_index, window_name,
  pane_id). Ephemeral — tmux names are per-host, the UUID is not
- `ConversationRegistry` (common/registry.rs) — `HashMap<id, Conversation>`, built read-only via
  `from_shadow(hook, disk_ids, live_placements, sidecar)` — a left-join over state.json + a disk
  scan + the overlay sidecar
- `ConversationSidecar` / `ConversationOverlay` — writable per-conversation overlay
  (note/pinned/archived), persisted to `conversations.json`
- `ClaudeInstance` (common/instances.rs) — one running Claude **window** (session, window_index,
  window_name, pane, cwd, pids, session_id, `cwd_shared`)

**Session model** (hooks + web dashboard):

- `HookState` (ipc/messages.rs) — `HashMap<session_id, SessionState>`, serialized to state.json
- `SessionState` — session_id, cwd, status, needs_attention, last_activity
- `SessionStatus` — Working, Waiting, NeedsPermission, EditApproval, PlanReview, QuestionAsked, RunningWorkflow (derived; see Background Tasks)
- `ClaudeStatus` (common/types.rs) — JSONL-parsed status enum; `serve/` maps it to wire `SessionStatus`
- `ProjectRegistry` (common/projects.rs) — `HashMap<name, ProjectConfig>`, loaded from projects.toml
- `ProjectConfig` (common/projects.rs) — project definition (emoji, path, startup, ports, files, hooks_dir, auth_profile, etc.)
- `WorktreeState` (common/worktree.rs) — `HashMap<"{project}/{branch}", WorktreeEntry>`, persisted to worktrees.json
- `WorktreeEntry` (common/worktree.rs) — worktree record (project_key, branch, type, path, session_name, metadata, created_at)
- `FrozenState` (common/frozen.rs) — `HashMap<entry_key, FrozenEntry>` (key = claude_session_id, else `session#window`), persisted to frozen.json
- `FrozenEntry` (common/frozen.rs) — one frozen Claude *window* (session_name, window_name, window_index, cwd, claude_session_id, claude_config_dir, note, frozen_at)
- `FreezeTarget` (common/frozen.rs) — the window chosen to freeze (session_name, window_index, window_name, cwd, claude_session_id)
- `SessionView` (serve/web_types.rs) — serializable session data for the web API (name, status, cpu, ports, pane, skipped, messages)
- `ProcessView` (serve/web_types.rs) — minimal process info for the web dashboard
- `ConversationMessage` (serve/web_types.rs) — chat message with role, text, and tool summaries
- `ToolSummary` (serve/web_types.rs) — compact tool use info (name, summary, detail for modal)

## Data Directory

All hive data lives under `~/.hive/`. The janus-wt-portal agent is installed to `~/.claude/agents/`.

```
~/.hive/
├── projects.toml              # project registry
├── cache/                     # runtime state
│   ├── state.json             # hook state (session statuses) — PRUNED after 10min idle
│   ├── worktrees.json         # registered worktrees
│   ├── frozen.json            # frozen (hibernated) Claude windows — resume metadata + notes
│   ├── conversations.json     # per-conversation overlay sidecar: note / pinned / archived
│   ├── conversation-scan.json # mtime-keyed transcript scan cache (warm gather ~6x faster)
│   ├── activity.jsonl         # append-only lifecycle/focus event log (feeds `hive stats`)
│   ├── open-windows.json      # snapshot of currently-open windows (recovery projection)
│   ├── favorites.txt          # favorite session names
│   ├── todos.txt              # per-session todo lists (active)
│   ├── todos-done.txt         # per-session completed todos
│   ├── muted.txt              # muted session names
│   ├── muted-projects.txt     # muted PROJECT keys (remembered preference; honored by the hook notifier)
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

Files are created on demand — `conversations.json` / `muted-projects.txt` only exist once you
pin/note a conversation or mute a project. (Old installs may also have stale `parked.txt` /
`remote-*.json` on disk; no current code reads them.)

## Platform Guards

macOS-only features use `#[cfg(target_os = "macos")]` with empty stubs for other platforms:
- `ports.rs`: `get_listening_ports_for_pids()` — uses `libproc`
- `chrome.rs`: `get_chrome_tabs()`, `focus_all_matched_tabs()` — uses JXA (sees all Chrome profiles)
- `iterm.rs`: `get_iterm_pane_count()`, `spread_panes()`, `collapse_panes()` — uses AppleScript

## Conversations TUI (`hive` / `hive conversations`, `prefix + s`, `prefix + d`)

The conversation-first view. Self-contained in `src/cli/conversations.rs` (~3.9k lines) over
the `common/registry.rs` model. Base entity is the **conversation** (Claude UUID), not the tmux
session — so closed/resumable and frozen conversations are first-class, and identity survives
tmux renames (a prerequisite for the future distributed/offload tier).

### Two views

**Active** (default) — live conversations grouped by the tmux session running them. It is
**session-first-complete**: every live tmux session shows, bucketed claude / other / skipped,
so nothing is invisible.

```
🐝 hive  (2, 1 live)          ← sessions running claude
 1 ● abc12345  my task          → working  (2m ago)  12%  340M
 2 ❯ 2         server             window     ← non-claude window of that session

  ── other ──                 ← live sessions with NO claude
📁 00-main
 3 ❯ 1         ssh                window

  ── skipped ──               ← skipped sessions (dim blue)
🌳 Clear Session  (2, 2 live)
```

- A session's tmux windows that host **no** conversation render as dimmed `❯ … window` rows.
  They're switch targets like conversations (numbered 1-9, Enter switches).
- Bare (0-conversation) headers show just the session name; Enter attaches.
- `s/v/m/!` on a window row or session header act on the **parent session** (these flags are
  session-level).

**Browse** (`/`) — a project launchpad: flat project list (empty registered projects included),
`💤 frozen` bucket pinned first. Typing searches projects-first then conversations. Archived
projects hide on the full list but surface on search or via `Ctrl+R`.

### Detail screens

Stack above the list; `←`/`h`/`Esc` pops back.
- **Project detail** — config, worktrees w/ live·frozen counts, all its conversations;
  `n` new conversation, `r` resume-last, `w`/`x` create/delete worktree.
- **Conversation detail** — **async** (`std::thread` + `Arc<Mutex<…>>`, no tokio): paints
  instantly from the registry, then fills CPU/mem, processes, ports (+Chrome titles), git
  commits, and a transcript tail as they arrive.

### Keys

| Key | Action |
|---|---|
| `1-9` | switch to the Nth switch target (conversation **or** window) |
| `f` | hint-jump — 2-char home-row labels on every conversation (Vimium-style) |
| `Enter` | conversation → switch/resume · window → switch · header → project detail |
| `→`/`l` | drill into detail · `←`/`h`/`Esc` back |
| `/` | Browse + search (types immediately) |
| `v` `m` `s` `!` | favorite · mute · skip · auto-approve (session-level) |
| `M` | global mute · `P` pin conversation · `e` edit note |
| `z`/`Z` | freeze a Claude window (prompts for a note) |
| `Del` | close live conv (kill window) · discard frozen · archive project |
| `L` `N` | iTerm spread/collapse · new-project wizard |
| `Ctrl+R` | Browse: reveal/hide archived |

**Mute has three levels**: `m` per-session, `M` global, and `m` on a *project* (Browse header /
project detail) = a remembered preference in `muted-projects.txt` honored by the hook notifier,
so future sessions of that project stay silent.

### How a live conversation is resolved (the tricky part)

`gather_conversations_inner()` must map each running Claude **window** to its conversation UUID.
`state.json` can't be trusted for this (pruned after 10min idle — often 1 entry while 10 windows
run), and a cwd shared by several Claude windows makes "newest transcript in this cwd" ambiguous.
Resolution order per window:

1. **Hook id** — pane-bound `state.json` entry (authoritative when present).
2. **`claude --resume <id>` from process argv** — pruning-proof and per-window exact. Read via
   `process::build_cmdline_map()` (`ps -axww`), because **sysinfo's `cmd()` is empty for claude
   on macOS** — which is also why `is_claude_process` falls back to the version-string process
   *name*. Validated against an on-disk transcript. Covers every hive thaw/reopen (they launch
   with `--resume <id>`).
3. **Recency fill** — a plain `claude` (no id in argv) takes the newest transcript for its cwd
   not already claimed by a sibling window (N live windows ↔ N newest transcripts).

*Known residual*: fresh **parallel** `claude` sessions in one cwd are matched by recency — a
best-effort guess, not a true window↔transcript link (none exists off-process: claude doesn't
hold the jsonl open). Exact wherever argv has `--resume`.

### Performance

`jsonl::scan_all_disk_conversations_cached()` keys an on-disk cache
(`conversation-scan.json`) by transcript mtime: it stats every transcript but only re-parses
changed ones. Warm gather **0.43s → 0.07s (~6x)**. The list auto-refreshes on a 2s idle tick,
preserving selection by conversation id.

## Frozen Windows (Freeze / Thaw)

Granularity is the **Claude window**, not the tmux session. A project session can host several
Claude windows (see `common/instances.rs`); each can be frozen/thawed independently. **Skip vs
Freeze** are two distinct ways to set work aside:

| | Skip (`S`) | Freeze (`Z`) |
|---|---|---|
| Scope | whole session | one Claude window |
| tmux | stays alive | **window killed** (its Claude process + RAM/CPU freed) |
| Use case | watching a server, waiting for input — don't kill | postpone one task, tackle later |
| Restore | un-skip | re-add window + `claude --resume <id>` |

Freeze works because Claude persists every conversation to JSONL on disk.

- **Freeze** (`freeze_window`): captures the window's Claude `session_id`, cwd, window name,
  and `CLAUDE_CONFIG_DIR` (from `tmux show-environment`) into a `FrozenEntry` keyed by the
  conversation id, then `tmux kill-window` on just that window. The session and its other
  windows keep running; freezing the last window lets tmux drop the empty session. Worktree
  dirs and conversation history on disk are untouched.
- **Thaw** (`thaw_window`): if the parent session is still alive, `tmux new-window` in it +
  `claude --resume <id>` (falls back to `claude -c`); if the session is gone, recreate it via
  `ensure_tmux_session` with that window. Then the entry is removed.

Window enumeration at freeze time uses `instances::instances_for_session` (builds the process
table + hook index on demand — fine for a one-shot action).

**TUI**: `Z` in the detail view freezes a Claude window — if the session has several, a window
picker (`FreezeWindowPick`) appears first; then a note prompt (`FreezeNote`). Frozen windows
are pinned as a `💤 … [frozen]` group at the top of the search picker (`/`), showing
`session · window`, the note, and relative time; the main-list header shows a `💤 N frozen`
count. Enter on a frozen row thaws + switches to it; `Del` discards a frozen entry (history
stays on disk). Frozen rows sit alongside the parent session row (which may still be live), not
in place of it.

**Web**: same `frozen.rs` layer via `/api/frozen` (list), `/api/freeze`, `/api/thaw`,
`/api/discard-frozen`. The info modal shows a per-window Freeze button (the window identity
comes from the live `/api/active` `windows` array); a `FROZEN` section in the session list
thaws on tap and discards via a trash icon.

## Background Tasks (workflow detection)

Claude can launch work that runs **in the background** while the main thread goes idle:
a `Workflow`, or an `Agent`/`Bash` tool call with `run_in_background`. When that happens the
main transcript's last entries are the launch followed by a `Stop`, so naive status detection
(hook state *or* jsonl) reports the session as **idle** even though work is in flight — the
session looks free when it's actually busy.

`jsonl.rs::detect_active_background_tasks` recovers the real state from the transcript. Every
background launch is a `tool_use` (name `Workflow`, or input `run_in_background: true`), and
when the task finishes the harness injects a `<task-notification>` whose `<tool-use-id>` equals
the launching tool_use's id. Pairing launches with notifications by that id yields the tasks
still running. This is windowing-safe (a completion always follows its launch), so reading a
bounded transcript tail is sufficient.

When a session's resolved status is otherwise **Waiting**, the call sites overlay it with
`RunningWorkflow(summary)` if `background_running_summary()` reports in-flight work. The overlay
fires only on an idle base status (so it never masks a permission/plan/question prompt) and is
applied at every status site: TUI single-window + multi-window (`tui/app.rs`) and the web
per-window builder (`serve/server.rs`). Display: TUI shows `flow` (blue) in the list / `⚙ <summary>`
in detail; the web shows a blue **Workflow** badge (falls back to a generic Busy if the JS is
older). Known limitation: a task that dies without a completion notification (e.g. Claude killed
mid-workflow) leaves an unmatched launch; it only mis-reports while the session is idle, and a
new turn clears it.

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
┌──────────────────┐     ┌──────────────────────────────────────┐
│  Data Thread      │     │  HTTP Thread (main, blocking recv)   │
│  (1s refresh)     │     │                                      │
│  gather registry  │     │  GET /             → embedded HTML   │
│  once, project →  │     │  GET /api/active   → session view    │
│  active + convs   │     │  GET /api/conversations → resume set │
│  → Arc<Mutex>     │     │  GET /api/messages → conversation    │
│                   │     │  POST /api/send    → tmux send-keys  │
└──────────────────┘     │  POST /api/resume  → reopen closed   │
                         │  POST /api/tts-hls → HLS via TTS    │
                         │  GET /hls/*        → proxy segments  │
                         └──────────────────────────────────────┘
```

**API endpoints:**

| Method | Path | Description |
|--------|------|-------------|
| GET | `/` | Serve embedded HTML (or from disk in `--dev` mode) |
| GET | `/api/active` | Active view (live convs grouped by tmux session + bare/startable sessions), projected from the registry; SessionView shape, polled every 1.5s |
| GET | `/api/conversations` | Closed/frozen conversations for the Resume view (ConversationView) |
| POST | `/api/resume` | Reopen a closed conversation by id (`claude --resume <id>` in its project session) |
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
| GET | `/api/frozen` | List frozen Claude windows (key, session_name, window_label, note, relative) |
| POST | `/api/freeze` | Freeze one window: `{"session","window_index","window_name","cwd","session_id","note"}` |
| POST | `/api/thaw` | Thaw a frozen window by key: `{"key": "..."}` |
| POST | `/api/discard-frozen` | Discard a frozen window (no restore): `{"key": "..."}` |
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
- Tappable session header opens info modal: CWD, ports, processes, flag toggles (favorite, auto-approve, skip), todo management (add/done/delete), per-window Freeze buttons, kill session button
- Frozen windows: a `FROZEN` section (💤) in the session list — tap to thaw (resume), trash icon to discard; freeze from the info modal prompts for a note
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

- `prefix + s` — conversations popup, list view (`hive`)
- `prefix + d` — conversations popup, detail for the current window (`hive --detail`)
- `Ctrl+n` / `Ctrl+p` — cycle next/prev session
- `Ctrl+g` — jump to the next non-busy Claude window (current session first, then others)
- `Ctrl+\` — cycle to next window in the current session (`window-prev` is CLI-only)
- Configured in `~/.tmux.conf`, also set by `hive setup`

## Testing

255 distinct tests. Run with `cargo test`.

> `cargo test` prints ~454 passing: `common/` + `ipc/` compile into **both** the lib and bin
> targets and run twice. Per target: lib 199 · bin 233 (the superset — adds cli/daemon/serve)
> · smoke 22.

**Unit tests (233)** — in-module `#[cfg(test)]` blocks:
- `common/`: types, projects, worktree, jsonl, chrome, process (claude detection,
  `parse_resume_id`), persistence (escape/unescape, set/todo file roundtrips), registry
  (from_shadow left-join, `resolve_parent` determinism, bounding, frozen overlay), instances,
  frozen, activity
- `ipc/messages.rs`: HookState operations, cleanup, serialization roundtrips
- `daemon/hooks.rs`: all HookEvent variants, status transitions, session lifecycle
- `cli/conversations.rs`: `build_active` bucketing (normal/other/skipped), bare-session
  surfacing, covered-window exclusion, `build_browse` archived visibility, hint labels,
  `sh_quote`

**Integration tests (22)** — `tests/cli_smoke.rs`, run the actual binary:
- `--version`, `--help`, all subcommand help pages
- Read-only commands exit 0 (project list, wt list, todo list, conversations)
- Invalid args exit non-zero
- Todo full roundtrip (add → list → next → done → clear)
- Project archive roundtrip (add → archive → list hides → `--all` shows → unarchive), isolated via a temp `$HOME`

No TUI rendering tests (interactive). No tmux-dependent tests (would need integration test
infrastructure) — hence the pure `build_active`/`build_browse` functions take their tmux/flag
inputs as parameters, which is what makes them unit-testable.

## Conventions

- No `tokio` or async — everything is synchronous. Background work (conversation detail) uses
  `std::thread` + `Arc<Mutex<…>>`, not a runtime
- Atomic file writes for state.json (write to .tmp, rename) — same idiom for frozen.json,
  conversations.json, worktrees.json
- `anyhow::Result` for error handling throughout
- `sysinfo::System` is kept alive in `App` / the conversations loop for CPU delta accuracy
  (needs two refresh calls)
- Stale sessions cleaned up after 10 minutes of inactivity (in hook handler) — so **never treat
  `state.json` as a complete list of live Claude windows**
- **Gotcha**: `sysinfo`'s `cmd()` is empty for claude on macOS — use `ps` (`build_cmdline_map`)
  for argv, and note `is_claude_process` therefore keys off the version-string process *name*
- Prefer pure functions taking their environment as parameters (e.g. `build_active(reg, skipped,
  session_windows)`) so tmux-dependent logic stays unit-testable

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
