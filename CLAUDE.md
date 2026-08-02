# hive

Interactive Claude Code session dashboard for tmux. Runs as a popup (`prefix + d`) to monitor, switch between, and manage parallel Claude sessions.

## Quick Reference

```bash
cargo test                # 289 tests (267 unit + 22 CLI smoke)
cargo build               # dev build
cargo clippy --all-targets -- -D warnings
cargo fmt                 # CI has a fmt gate — run before committing
cargo install --path . --root ~/.local  # install binary
hive setup                # register hooks + tmux keybinding
```

> `cargo test` prints ~482 passing because `common/` + `ipc/` compile into **both** the lib and
> bin targets and run twice. Distinct tests: 267 unit + 22 smoke.

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
hive --project-detail   # PROJECT detail for the current window (prefix + a)
hive --picker           # conversations, start in Browse + search
hive --filter <q>       # conversations, start in Browse + search pre-filled with <q>
hive conversations      # explicit; --list for a static listing (also used when piped)
hive stats [--days N]   # usage summary from the activity log (default 7 days)
hive start              # auto-attach to first available session (or fall through to picker)
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
│   ├── projects.rs         project registry (projects.toml)
│   ├── worktree.rs         worktree lifecycle (types, state, git ops, file ops, hooks, memory seed)
│   └── debug.rs            debug logging to cache dir
├── daemon/
│   ├── hooks.rs            handle_hook_event(): maps HookEvent → SessionState updates
│   └── notifier.rs         platform-native notifications (terminal-notifier/osascript/notify-send)
├── ipc/
│   └── messages.rs         HookEvent, SessionState, HookState (load/save), SessionStatus
├── serve/                  web dashboard — projects the ConversationRegistry (conversation model)
│   ├── mod.rs              module registration (metrics, server, web, web_types)
│   ├── server.rs           gather_active_views()/build_conversation_views() — project the registry
│   ├── metrics.rs          Prometheus text exposition for GET /metrics (scraped by OTel)
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
  frozen, note, pinned, archived (+ `archive_reason`/`archived_at`), title, auth_config_dir,
  and runtime-only `cpu`/`mem_kb`
- `ConversationId` — newtype over the UUID (machine-independent, unlike tmux names)
- `Lifecycle` — `Live` | `Closed`. **A live tmux placement is the SOLE discriminator** for Live
- `TmuxPlacement` — where a conversation currently runs (session_name, window_index, window_name,
  pane_id). Ephemeral — tmux names are per-host, the UUID is not
- `ConversationRegistry` (common/registry.rs) — `HashMap<id, Conversation>`, built read-only via
  `from_shadow(hook, disk_ids, live_placements, sidecar)` — a left-join over state.json + a disk
  scan + the overlay sidecar
- `ConversationSidecar` / `ConversationOverlay` — writable per-conversation overlay
  (note/pinned/archived + `archive_reason`/`archived_at`), persisted to `conversations.json`
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
├── config.toml                # global settings — `[web]` autostart (common/config.rs)
├── cache/                     # runtime state
│   ├── state.json             # hook state (session statuses) — PRUNED after 10min idle
│   ├── worktrees.json         # registered worktrees
│   ├── frozen.json            # frozen (hibernated) Claude windows — resume metadata + notes
│   ├── conversations.json     # per-conversation overlay sidecar: note / pinned / archived (+ reason, when)
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

## Conversations TUI (`hive` / `hive conversations`, `prefix + s`, `prefix + d`, `prefix + a`)

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
`💤 frozen` bucket pinned first. Typing searches **projects first** — a query is matched against
every registered project's key and `display_name` (`projects_matching`), and a named project
surfaces *even with zero conversations* and sorts above the conversation hits — then conversations
(`matches_query`). Archived projects hide on the full list but surface on search or via `Ctrl+R`.

> Search must match project *names*, not just conversations: the registry is age-bounded, so a
> long-archived project has no conversations left to match — before this it was unreachable, i.e.
> archiving was a one-way door.

**Worktrees are Browse rows too.** Each project's registered worktrees (from `worktrees.json`,
*not* from conversations) render above its conversations as `● branch … worktree` rows —
`●` = its tmux session is up, `○` = not running — and the header carries an `N wt, M up` badge.
`browse_worktrees()` filters them by branch / session name / path, and every worktree of a
project the query NAMED comes along; `build_browse` then surfaces that project even with zero
conversations, exactly as `matched_projects` does for a name hit. `Enter` on a worktree row
opens its session (`connect_worktree` — `ensure_tmux_session` at the worktree path with the
project's startup command + auth env when it isn't running), `→` opens the project detail,
`Del` deletes the worktree behind the usual y/n confirm.

> Same age-bounding problem as archived projects, one level down: a worktree with no
> conversations — fresh, or old enough that its conversations aged out — matched nothing, so
> `/` couldn't reach it at all. Keying the rows off the worktree registry is what fixes that.

**`Tab` expands/collapses a project.** The unsearched flat list starts fully collapsed (25
projects, 30 worktrees would otherwise be a wall), so `Tab` is how you open one project and see
its worktrees + conversations without typing a query.

**Each section pages at `BROWSE_PAGE` (5).** An expanded project shows its 5 latest worktrees
and 5 latest conversations, each followed by a `… N more <section>` row; Enter/`→` on that row
reveals the rest and flips it to `… show fewer`. `visible_rows` takes the `page` cap and the
`expanded` set (keyed by `(group key, Section)`, so it survives a rebuild reordering the groups
and resets whenever the query changes). Both sections page independently, and the More row sits
*after* its items — so expanding in place leaves the cursor on the first revealed row.

> `page` is `Some(5)` for Browse and **`None` for Active**: a live window that isn't listed is a
> window you can't get back to, so the Active view stays session-first-complete.

The **project detail** pages the same way, but with its own mechanism: a single `show_all` flag on
`ProjectDetailState` toggles both sections, `Tab` flips it, and each section ends with a
non-selectable `… N more <section> — Tab to show all` line (there's no per-section cursor to hang
an expandable row on). `num_items()` counts only the VISIBLE rows, so the cursor can't walk off
into rows that aren't drawn, and digits `1-9` stop at the cap for the same reason. `show_all` is
carried across the detail's rebuilds (archive/unarchive/discard) so a refresh doesn't silently
re-cap a list you just opened.

"Latest" for worktrees means the newest activity across the worktree's conversations, falling
back to its `created_at` (a worktree created five minutes ago has no conversations yet and must
still rank as recent). Running worktrees still sort above everything.

**Browse is always in search mode**: entering it (`/`, `--picker`, `--filter`) sets `searching`,
and only Esc clears it — which also returns to Active. So `view == Browse` ⟹ `searching`, the
`if searching { … } continue;` block shadows the main key match, and **every Browse action must
live in that block**. Only non-letter keys can act there (letters are query text): Enter, `Tab`,
`→`, arrows, `Del`, and Ctrl-chords. Project-level actions otherwise belong in the project detail.

### Detail screens

Stack above the list; `←`/`h`/`Esc` pops back.
- **Project detail** — config, todos, its conversations, then worktrees w/ live·frozen counts;
  `n` new conversation, `r` resume-last, `w`/`x` create/delete worktree, `m` mute, `a`
  archive/unarchive the PROJECT (the `[archived]` tag reads from `Flags`, reloaded on each
  toggle), `A` archive/unarchive the selected CONVERSATION — see below. The worktree section
  is **dropped entirely when the project has none** (an empty "Worktrees (0) / none" block is
  noise), and sits *below* the conversations: worktrees are places to go, not work in flight.
- **Worktree detail** — the same screen, drilled into from a worktree row (`Enter`/`→`). Keyed by
  the worktree key (`project/branch`), which `conv_in_project` already matches exactly, so it needs
  no new filter: what changes is `wt_info` being set — its own path + session in the header, no
  nested worktree list, todos from its own registered session, `n` opening a conversation there and
  `x` deleting *it*. Details **stack**: `detail_stack` holds the parent, so `Esc` pops worktree →
  project → list (`q` leaves outright).
- **Conversation detail** — **async** (`std::thread` + `Arc<Mutex<…>>`, no tokio): paints
  instantly from the registry, then fills CPU/mem, processes, ports (+Chrome titles), git
  commits, and a transcript tail as they arrive.

The detail's cursor runs **todos → conversations → worktrees**, matching the drawn order;
`selected_todo()` / `selected_conv()` / `selected_worktree()` slice `sel` by the VISIBLE counts, so
the offsets follow the cap and `item_pos` must be pushed in that same order.

**Opening a detail straight from tmux.** `prefix + d` (`--detail`) opens the current window's
CONVERSATION detail; `prefix + a` (`--project-detail`) opens its PROJECT detail.
`current_window_project()` resolves the project two ways, because the window you press `a` from
may not be running Claude at all: the current window's conversation names its `parent`, and
failing that the pane's cwd goes through `registry::resolve_parent` — the same rule the registry
groups by. A worktree resolves **up to its project** (that screen lists the worktree, its
siblings, and every conversation under any of them; the worktree's own detail is one `Enter`
away). The two flags are independent and compose: with both, the conversation detail draws on
top and `Esc` pops to the project rather than out to the list.

**Conversation sections.** The list is sectioned by sub-headers so a frozen conversation is
visible as *pending*, not lost: `sort_project_convs` puts live first, then **frozen**, then plain
closed, then the archived tail, and the draw emits `💤 Frozen (n)` / `Closed (n)` / `Archived (n)`
at each boundary (`frozen_from()` / `closed_from()` / `archived_from()`).

> Frozen used to sort *below* closed, which put the one conversation you froze in order to
> remember it past the 5-row cap — invisible on the screen whose title bar counts it.

`Closed (n)` only appears when a Frozen section precedes it (otherwise closed rows just continue
the list unlabelled, as before), and `frozen_from()` returns `None` when *everything* is frozen —
on the 💤 bucket the header would only restate the screen title and each row's own 💤 marker.

**The buckets (`💤 frozen`, `(unassigned)`) are not projects** — `bucket: true` on the state. They
own no config, no worktrees and no "new conversation" target, so the worktree section and the
project-level footer actions are dropped, and because their rows come from *everywhere* each one is
labelled with `project_label()` (`🌳 Clear Session / CSD-2723`), which conv_line renders as a
leading column right after the marker and *before* the id — where a row lands is the first thing
you need when you can't place it — and which suppresses the subpath column (in a cross-project list
the cwd's shared-prefix remainder just restates the project).

> That column is padded with `fit_cells()`, not `{:<n}`: an emoji is one char but **two display
> cells**, so char padding shifts every row whose project has an icon. (Residual drift on emoji
> written with a VS16 selector — `👁️` — is tmux's width table disagreeing with the terminal, not
> the padding.) `unicode-width` is a direct dependency for this; it was already in the tree via
> ratatui.

### Keys

| Key | Action |
|---|---|
| `1-9` | switch to the Nth switch target (conversation **or** window) |
| `f` | hint-jump — 2-char home-row labels on every conversation (Vimium-style) |
| `Enter` | conversation → switch/resume · worktree → open its session · window → switch · header → project detail |
| `→`/`l` | drill into detail · `←`/`h`/`Esc` back |
| `/` | Browse + search (types immediately) — projects, worktrees, conversations |
| `Tab` | Browse: expand/collapse the project under the cursor · project detail: full lists ⇄ latest 5 |
| `Enter`/`→` on `… N more` | reveal the rest of that section (5 shown by default) / fold it back |
| `Enter`/`→` on a worktree row | project detail: open that worktree's own detail (`Esc` pops back) |
| `v` `m` `s` `!` | favorite · mute · skip · auto-approve (session-level) |
| `M` | global mute · `P` pin conversation · `e` edit note |
| `z`/`Z` | freeze a Claude window (prompts for a note) |
| `Del` | close live conv (kill window) · discard frozen · archive project (Browse header) · delete worktree (confirm) |
| `a` | project detail: archive / unarchive the project |
| `A` | project detail: archive the selected conversation (prompts for a reason) / unarchive |
| `L` `N` | iTerm spread/collapse · new-project wizard |
| `Ctrl+R` | Browse: reveal/hide archived |

**Mute has three levels**: `m` per-session, `M` global, and `m` on a *project* (project detail)
= a remembered preference in `muted-projects.txt` honored by the hook notifier, so future
sessions of that project stay silent.

### Archived conversations (`A`)

Archiving sets a conversation aside **without losing it**. `A` in the project detail prompts
for a reason ("archive reason: …"; empty is allowed) and writes `archived` / `archive_reason` /
`archived_at` to the conversation's overlay in `conversations.json`. `A` again unarchives and
clears all three, so a re-archived conversation never carries a stale reason.

The whole point is that it is hidden *somewhere* and explained *somewhere*:

| Surface | Archived conversation |
|---|---|
| Browse (`/`), `--list`, web Resume view | **hidden** |
| Project detail | shown, under an `Archived (n)` section at the bottom |

The project detail renders it dimmed with an `[archived]` tag plus an indented
`archived <when> — <reason>` line (`archive_reason_line`; "no reason given" when empty), and the
title bar carries an `N archived` count separate from `closed`. `sort_project_convs` puts the
archived tail last — `archived_from()` is where the section header goes.

Two consequences worth knowing:

- **The registry keeps archived conversations.** `should_surface_closed` deliberately does *not*
  treat archived as a bound (it once did, which is why archived conversations were unreachable
  rather than merely hidden) — they're still bounded by age/parent/pin like anything else, and
  each consumer decides whether to display them. That's the one filter to remember when adding
  a new listing surface.
- **Opening an archived conversation unarchives it** (`activate`), mirroring un-skip-on-switch:
  otherwise it would run while invisible in Browse. `r` (resume-last) skips archived ones, so
  archiving the newest conversation doesn't make `r` reopen the thing you just set aside.

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

**Autostart (opt-in).** So the dashboard is live whenever you're at your machine without
manually running `hive web`, the TUI can ensure a server exists on launch. Add to
`~/.hive/config.toml`:

```toml
[web]
autostart = true
port = 8375                          # optional (default 8375)
tts_host = "http://10.18.1.2:9800"   # optional — passed as --tts-host
```

`run_conversations_tui` then calls `serve::web::ensure_web_autostart()` **once** at startup (never
in the refresh loop, so there's no ongoing polling cost). It's idempotent and best-effort:

- A sub-millisecond localhost probe (`web_server_listening`) short-circuits if a server is already
  up — so a second popup, or a manually-run `hive web`, never double-spawns.
- Otherwise it launches the server as a **detached, hidden tmux session** named `__hive_web`
  (`common::config::WEB_SESSION`), so it outlives the ephemeral TUI popup (a thread inside the
  popup would die when the popup closes). Runs non-`--dev` (embedded HTML — the popup's cwd is
  arbitrary). Stop it with `tmux kill-session -t __hive_web`.
- `__hive_web` is filtered out of every session listing (`common::tmux` — `get_all_windows`,
  `get_tmux_sessions`, `get_current_tmux_session_names`), so it never shows as a switch/cycle
  target in the TUI, the web dashboard, or `hive start`.

Default is **off**: autostart binds `0.0.0.0:<port>`, a LAN-exposed surface, so it must be opted
into explicitly.

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
| GET | `/metrics` | Prometheus text exposition — see **Metrics** below |
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

## Metrics (`GET /metrics`)

`serve/metrics.rs` renders hive's state as **Prometheus text exposition**, so an OTel collector
can scrape it into the same Grafana as Claude Code's native `claude_code.*` cost/token
telemetry. The point is one pane of glass: *what the agents cost* next to *how many were
running, on what, for how long*.

A scrape endpoint rather than an OTLP push, deliberately: `opentelemetry-otlp`'s gRPC transport
pulls in tonic + tokio, and hive is synchronous by design (see **Conventions**). The web server
already runs and autostarts (`__hive_web`), so exposing text costs zero new dependencies.

**Cheap series** — computed per scrape from files + tmux:

| Series | Type | Notes |
|---|---|---|
| `hive_windows_open` | gauge | **the concurrency signal** — agents running side by side |
| `hive_frozen_windows` | gauge | frozen, awaiting resume |
| `hive_worktrees{project,state}` | gauge | `state="dead"` is the **worktree-debt backlog** |
| `hive_projects{state}` | gauge | active vs archived |
| `hive_todos{state}` | gauge | active / done, summed across sessions |
| `hive_tmux_sessions` | gauge | live sessions (`__hive_web` excluded) |
| `hive_active_seconds_total{session}` | counter | focused time, machine-sleep subtracted |
| `hive_web_seconds_total{session}` | counter | dashboard viewing time |
| `hive_windows_{opened,closed,frozen,thawed}_total` | counter | lifecycle |
| `hive_sessions_killed_total`, `hive_focus_switches_total`, `hive_web_views_total` | counter | |

**Registry series** — from `RegistrySnapshot`, the conversation model. These are what Claude
Code's own telemetry structurally *cannot* report: it knows what a session spent, not how many
sessions exist, where they live, or whether they're stuck waiting on a human.

| Series | Type | Notes |
|---|---|---|
| `hive_conversations{lifecycle}` | gauge | live vs closed-but-resumable |
| `hive_conversations_blocked` | gauge | **the attention bottleneck** — live convs awaiting a human decision |
| `hive_conversations_needs_attention` | gauge | registry's own attention flag |
| `hive_conversations_by_status{status}` | gauge | working / waiting / needs_permission / plan_review / question_asked / running_workflow / edit_approval / unknown |
| `hive_conversations_by_project{project,lifecycle}` | gauge | grouped by PROJECT (a worktree conv rolls up to its project) |
| `hive_conversations_by_auth_profile{profile}` | gauge | `work` vs `default` — which identity is running |
| `hive_conversations_{archived,pinned}` | gauge | overlay counts |
| `hive_claude_cpu_percent{project}` | gauge | live process trees — the *local* cost of parallelism |
| `hive_claude_memory_bytes{project}` | gauge | ditto; RAM bounds concurrency before spend does |

Counters are computed over **all time** (`ALL_TIME_DAYS`), not a rolling window: the activity
log is append-only, and a rolling window would sawtooth as events age out — Prometheus reads
every decrease as a counter reset.

### Where the registry series come from

`/metrics` must **never** trigger a registry gather — that's a full process/tmux/JSONL sweep.
The web data thread already gathers once per second, so it also builds a
`metrics::RegistrySnapshot` and publishes it through a third `Arc<Mutex<…>>` alongside
`shared_active` / `shared_conversations`. The handler only formats it.

Two correctness constraints, both load-bearing:

- The snapshot is built from the **full registry**, not from `build_conversation_views` — that
  view filters archived conversations out, and a backlog count that silently omits set-aside
  work is wrong.
- `render(None)` **omits** every registry series rather than emitting zeros, so the first
  second of `hive web` (before any gather) can't be misread as "no conversations exist".

Status labels are payload-free (`needs_permission`, not the tool name) — the payload variants
carry unbounded free text and would explode series cardinality.

> **Gotcha**: `compute_stats` calls `machine::sleep_intervals`, which shells out to
> `pmset -g log` — a **~1.8s** call that renders ~45k lines. That is fine for a one-shot
> `hive stats`, but this endpoint is scraped on a timer and `/api/stats` is hit on every
> dashboard load. `machine.rs` therefore memoizes the raw `pmset` output for 5 minutes
> (process-lifetime, success-only). `/metrics` answers in ~25ms as a result. One-shot CLI runs
> see a cold cache and behave exactly as before.

> **Gotcha**: `compute_stats` calls `machine::sleep_intervals`, which shells out to
> `pmset -g log` — a **~1.8s** call that renders ~45k lines. That is fine for a one-shot
> `hive stats`, but this endpoint is scraped on a timer and `/api/stats` is hit on every
> dashboard load. `machine.rs` therefore memoizes the raw `pmset` output for 5 minutes
> (process-lifetime, success-only). `/metrics` answers in ~25ms as a result. One-shot CLI runs
> see a cold cache and behave exactly as before.

The collector stack lives outside this repo, in `claude-logging/otel-stack/`.

## Tmux Integration

- `prefix + s` — conversations popup, list view (`hive`)
- `prefix + d` — conversations popup, detail for the current window (`hive --detail`)
- `prefix + a` — conversations popup, **project** detail for the current window
  (`hive --project-detail`) — see `current_window_project()`
- `Ctrl+n` / `Ctrl+p` — cycle next/prev session
- `Ctrl+g` — jump to the next non-busy Claude window (current session first, then others)
- `Ctrl+\` — cycle to next window in the current session (`window-prev` is CLI-only)
- Configured in `~/.tmux.conf`, also set by `hive setup`

## Testing

289 distinct tests. Run with `cargo test`.

> `cargo test` prints ~482 passing: `common/` + `ipc/` compile into **both** the lib and bin
> targets and run twice. Per target: lib 206 · bin 267 (the superset — adds cli/daemon/serve)
> · smoke 22.

**Unit tests (267)** — in-module `#[cfg(test)]` blocks:
- `common/tmux.rs`: `exact`/`exact_window`/`exact_pane` target building — the `=` that stops
  tmux prefix/fnmatch-matching a session name onto a longer one (see **Conventions**)
- `common/`: types, projects, worktree, jsonl, chrome, process (claude detection,
  `parse_resume_id`), persistence (escape/unescape, set/todo file roundtrips), registry
  (from_shadow left-join, `resolve_parent` determinism, bounding, frozen overlay), instances,
  frozen, activity (incl. `entry_line` single-line/one-syscall invariant), config
  (`[web]` parse, defaults-off, legacy `[defaults]` ignored)
- `ipc/messages.rs`: HookState operations, cleanup, serialization roundtrips
- `daemon/hooks.rs`: all HookEvent variants, status transitions, session lifecycle
- `cli/conversations.rs`: `build_active` bucketing (normal/other/skipped), bare-session
  surfacing, covered-window exclusion, `build_browse` archived visibility + name-matched
  (conversation-less) projects surfacing first, `projects_matching`, `browse_worktrees`
  (branch/session/path filtering, project-named pass-through, live-conversation counts,
  recency ordering) + `visible_rows` section paging (cap, per-section expand, Active uncapped) +
  project-detail paging + cursor offsets (`visible_convs`/`num_items` under the cap,
  `selected_todo`/`selected_conv`/`selected_worktree` boundaries, `detail_more_line`),
  `project_label`, `fit_cells` (display-width padding/truncation) +
  `build_browse` surfacing a project for its worktrees alone, archived conversations
  (hidden in `build_browse` unless live, `sort_project_convs` tail, `archived_from` /
  resume-last skipping them, `archive_reason_line`), frozen conversations sectioned above
  closed (`sort_project_convs` order, `frozen_from`/`closed_from`/`section_counts`, no header
  on an all-frozen list), hint labels, `sh_quote`
- `serve/metrics.rs`: Prometheus label escaping (reserved chars, emoji/space passthrough),
  `scalar` HELP/TYPE/sample shape, `render()` well-formedness (every non-comment line ends in a
  parseable numeric value) + registry series omitted without a snapshot, and `RegistrySnapshot`
  (lifecycle counts, worktree convs rolling up to their project, archived still counted,
  blocked-vs-working split, closed convs contributing no status/resources, auth-profile
  defaulting, payload-free status labels)

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
- **Gotcha — every tmux session-name target needs `tmux::exact`.** tmux resolves `-t <name>`
  as exact match → fnmatch pattern → **prefix**, so `-t "📊 Avateen"` silently resolves to a
  running `📊 Avateen Hub`. Bare targets meant `has-session` reported the wrong session alive
  (new conversations opened in the *other* project), and `kill-session` / `rename-session`
  acted on it. Wrap session names in `tmux::exact` (`=name`), window/pane targets in
  `exact_window` / `exact_pane` — the `=` also stops the `[project]` in a worktree session
  name being read as an fnmatch character class. Pane (`%12`) and window (`@34`) ids are
  already unambiguous — never wrap those.
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
