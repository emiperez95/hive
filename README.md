# hive

[![CI](https://github.com/emiperez95/hive/actions/workflows/ci.yml/badge.svg)](https://github.com/emiperez95/hive/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](./LICENSE)
![macOS](https://img.shields.io/badge/platform-macOS-lightgrey)

Interactive [Claude Code](https://docs.claude.com/en/docs/claude-code/overview) session dashboard for tmux.

Monitor and manage multiple parallel Claude Code sessions from a single TUI. See which sessions need permission, approve them with a keypress, and switch between sessions instantly. A hive "session" is just a tmux session — hive tracks Claude's status inside each one via Claude Code hooks and mirrors it in the dashboard.

## Features

- **Session overview** — every tmux session running Claude, with CPU/memory and real-time status
- **Permission approval** — approve Bash, Write, Edit permissions with single keypresses
- **Detail view** — per-session todos, listening ports, Chrome tab matching, process tree
- **Search** — fuzzy search across active sessions and registered projects
- **Project registry** — emoji identifiers, startup commands, per-project auth profiles
- **Worktree management** — create/delete git worktrees with tmux sessions and lifecycle hooks
- **Mobile dashboard** — `hive web` exposes a phone-friendly UI with conversation view, TTS playback, and remote session control
- **iTerm2 pane spread** — split into N panes, each auto-attaching to a session
- **Notifications** — native macOS notifications when sessions need attention
- **Hook-based status** — Claude Code hooks push status into hive in real time
- **Port + Chrome matching** — discovers each session's listening TCP ports and maps them to Chrome tabs

## Prerequisites

- **macOS** — v0.1.0 is macOS-only (Apple Silicon or Intel). Linux is deferred.
- **[tmux](https://github.com/tmux/tmux)** on your `PATH`.
- **[Claude Code](https://docs.claude.com/en/docs/claude-code/overview)** CLI on your `PATH`.
- `curl` and `tar` (installed by default on macOS) for the prebuilt install path.
- Optional: iTerm2 for pane spread/collapse; Chrome for tab matching.

## Install

### Prebuilt binary (recommended)

```bash
curl -fsSL https://raw.githubusercontent.com/emiperez95/hive/main/install.sh | bash
```

This downloads the latest mac tarball from [GitHub Releases](https://github.com/emiperez95/hive/releases/latest) and drops `hive` into `~/.local/bin/`. If that's not on your `PATH`, the script tells you what to add to your shell rc.

To pin a version: `./install.sh v0.1.0`.

### Build from source (contributors)

Needs a stable Rust toolchain (`rustup` from [rustup.rs](https://rustup.rs)).

```bash
git clone https://github.com/emiperez95/hive
cd hive
cargo install --path . --root ~/.local
```

### Update

```bash
hive update
```

Downloads the latest release tarball and replaces the running binary in place, then re-runs `hive setup` to keep hook paths current.

## Setup

Register Claude Code hooks and tmux keybindings:

```bash
hive setup
```

This will:
- Add hook entries to `~/.claude/settings.json` (preserves existing hooks)
- Install the `janus-wt-portal` agent to `~/.claude/agents/`
- Install the `hive/create-project` slash command to `~/.claude/commands/`
- Optionally bind `prefix+s` (list view), `prefix+d` (detail view), `Ctrl+n`/`Ctrl+p` (cycle) in tmux

> Tmux keybindings are applied to the **currently running tmux server only** — they don't persist across reboots. `hive setup` prints the `bind-key …` snippets you should paste into `~/.tmux.conf` for persistence.

Running `hive setup` again shows what's already installed and only offers to add what's missing. Pass `--yes` (`-y`) to auto-accept every prompt — useful for scripted installs.

To remove everything (hooks, agent, command, tmux bindings):

```bash
hive uninstall            # interactive
hive uninstall --yes      # non-interactive
```

## Quick start

After `hive setup`, register your first project and watch it light up:

```bash
# 1. Register a project — emoji + path
hive project add myproj --emoji 🚀 --path ~/code/myproj

# 2. Create the tmux session and attach
hive connect myproj

# 3. Inside the tmux session, start Claude
claude

# 4. In another pane (or tmux window), open the dashboard
hive
```

As Claude runs tools, its status in the dashboard flips between Working / Waiting / Needs Permission in real time. Press `y` to approve a pending permission.

## Commands

### Core

```bash
hive                    # open TUI dashboard (default)
hive start              # auto-attach to first available tmux session
hive --detail           # open TUI with detail view for current session
hive --picker           # open TUI in search/picker mode
hive -w 5               # custom refresh interval (seconds)
hive -f pattern         # filter sessions by name
```

### Session Navigation

```bash
hive cycle-next         # switch to next tmux session (skips skipped sessions)
hive cycle-prev         # switch to previous tmux session
hive connect <key>      # create/attach tmux session for a registered project
```

### iTerm2 Panes

```bash
hive spread <N>         # split into N vertical iTerm2 panes (each runs hive start)
hive collapse           # close all panes except the current one
```

### Project Registry

```bash
hive project add <key>  # add a project (supports --emoji, --path, --startup, etc.)
hive project remove <key>
hive project list
hive project import     # import from sesh.toml (only relevant if you already use `sesh`)
```

### Worktrees

```bash
hive wt new <project> <branch>    # create worktree + tmux session (with hooks)
hive wt delete <project> <branch> # delete worktree + session + branch
hive wt list [project]            # list registered worktrees with status
```

### Todos

```bash
hive todo list [--session <name>] [--done]
hive todo next [--session <name>]
hive todo add <text> [--session <name>]
hive todo done [index] [--session <name>]
hive todo clear [--session <name>]
```

### System

```bash
hive setup              # register hooks, agent, and tmux keybindings
hive setup --yes        # non-interactive: auto-accept all prompts
hive update             # update to latest prebuilt release + re-run setup
hive uninstall          # remove hooks and keybindings
hive uninstall --yes    # non-interactive uninstall
hive hook <event>       # process hook event from stdin (used by Claude Code hooks)
hive --debug <command>  # enable verbose logging to ~/.cache/hive/debug.log
```

Set `HIVE_NO_NOTIFY=1` to suppress desktop notifications (useful for CI / scripts).

### Tmux Keybindings

| Key | Action |
|---|---|
| `prefix+s` | Open hive popup (list view) |
| `prefix+d` | Open hive popup (detail view) |
| `Ctrl+n` | Cycle to next session |
| `Ctrl+p` | Cycle to previous session |

### Keyboard Shortcuts

#### List View

| Key | Action |
|---|---|
| `↑↓` / `j/k` | Navigate sessions |
| `Enter` | Open detail view |
| `1-9` | Switch to session by number (exits) |
| `y/z/x/w/v` | Approve permission (once) |
| `Y/Z/X/W/V` | Approve permission (always) |
| `/` | Search sessions |
| `L` | Spread/collapse iTerm2 panes |
| `M` | Toggle global mute |
| `R` | Force refresh |
| `?` | Help screen |
| `Esc` / `Q` | Quit |

#### Detail View

| Key | Action |
|---|---|
| `↑↓` / `j/k` | Navigate todos/ports |
| `Enter` | Switch to session / open port |
| `A` | Add todo |
| `D` | Delete selected todo |
| `F` | Toggle favorite |
| `!` | Toggle auto-approve |
| `M` | Toggle mute |
| `S` | Toggle skip from cycling |
| `Esc` / `Q` | Quit |

## Mobile dashboard (`hive web`)

Start a local HTTP server that exposes hive to your phone over your LAN:

```bash
hive web                            # default port 8375
hive web --port 9000                # custom port
hive web --tts-host <url>           # optional: read messages aloud via TTSQwen
hive web --dev                      # serve web.html from disk for live editing
```

The dashboard is mobile-first: tap to switch sessions, swipe a session row to skip, tap the header to see cwd / ports / flags, send text back to Claude, and approve or reject pending permission prompts from the phone. Conversations render with markdown and tool-use cards (Bash, Write, Edit, Read, Grep, Agent) you can expand.

See [CLAUDE.md](./CLAUDE.md) for the full feature list and API reference.

## Architecture

```
Claude hook fires
  → hive hook <event>        (reads stdin JSON, updates state.json)

TUI refreshes every 1s
  → reads state.json          (session status from hooks)
  → tmux list-sessions        (discover sessions, windows, panes)
  → sysinfo                   (CPU/mem per process)
  → libproc                   (listening ports, macOS only)
  → Chrome tabs               (AppleScript, macOS only)
```

No background daemon. No async runtime. No Unix sockets. Just a state file.

## Advanced

- **[Auth profiles](./docs/claude-auth-profiles.md)** — assign each project a separate Claude identity via `CLAUDE_CONFIG_DIR`. Useful for keeping work + personal accounts separate.
- **Worktree lifecycle hooks** — project-specific shell scripts (`pre-create`, `post-worktree`, `post-copy`, `post-setup`, `pre-delete`, `post-delete`) run at defined points of `hive wt new` / `hive wt delete`. See [CLAUDE.md](./CLAUDE.md) for the contract + env vars.
- **Web dashboard internals** — endpoints, HLS streaming, message extraction, dev mode: see [CLAUDE.md](./CLAUDE.md#web-dashboard-hive-web).

## Platform support

v0.1.0 is **macOS-only** (Apple Silicon and Intel). Linux is deferred — the signature integrations (port detection, Chrome tab matching, iTerm pane spread) all rely on macOS-specific APIs.

## License

MIT
