# hive

Interactive Claude Code session dashboard for tmux.

Monitor and manage multiple parallel Claude Code sessions from a single TUI. See which sessions need permission, approve them with a keypress, and switch between sessions instantly.

## Features

- **Session overview** — all tmux sessions with Claude activity, CPU/memory usage
- **Permission approval** — approve Bash, Write, Edit permissions with single keypresses
- **Detail view** — per-session todos, listening ports, Chrome tab matching, process tree
- **Search** — fuzzy search across active sessions and registered projects
- **Project registry** — manage projects with emoji identifiers, startup commands, and config
- **Worktree management** — create/delete git worktrees with tmux sessions and lifecycle hooks
- **iTerm2 pane spread** — split into N panes, each auto-attaching to a session
- **Notifications** — native macOS/Linux notifications when sessions need attention
- **Hook-based status** — real-time Claude status via Claude Code hooks
- **Port detection** — discovers listening TCP ports per session (macOS via libproc)
- **Chrome integration** — matches localhost Chrome tabs to session ports (macOS)

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
- Optionally bind `prefix+s` (list view) and `prefix+d` (detail view) in tmux

Running `hive setup` again shows what's already installed and only offers to add what's missing.

To remove everything:

```bash
hive uninstall
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
hive project import     # import from sesh.toml
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
hive update             # update to latest version from GitHub + re-run setup
hive uninstall          # remove hooks and keybindings
hive hook <event>       # process hook event from stdin (used by Claude Code hooks)
```

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

## Platform Support

| Feature | macOS | Linux |
|---|---|---|
| TUI dashboard | yes | yes |
| Hook status | yes | yes |
| Notifications | osascript/terminal-notifier | notify-send |
| Port detection | libproc | stub (empty) |
| Chrome tabs | AppleScript | stub (empty) |
| iTerm2 panes | AppleScript | stub (empty) |

## Adding Terminal Support

The iTerm2 pane spread/collapse feature lives in `src/common/iterm.rs` and is guarded behind `#[cfg(target_os = "macos")]`. To add support for another terminal emulator:

1. **Create a new module** (e.g., `src/common/wezterm.rs` or `src/common/kitty.rs`) implementing:
   - `get_pane_count() -> usize` — return the number of panes/splits in the current tab/window
   - `spread_panes(n: usize) -> bool` — open `n` new panes, each running `hive start` with the current PATH
   - `collapse_panes() -> bool` — close all panes except the current one

2. **Register the module** in `src/common/mod.rs`.

3. **Wire it up** in `src/main.rs`: `run_spread()` and `run_collapse()` currently call `crate::common::iterm::*`. Add detection logic or a config flag to select the right backend. The TUI key handler for `L` in `run_tui()` uses `get_iterm_pane_count()` to decide between spread and collapse.

Each terminal has different automation APIs:
- **iTerm2**: AppleScript (`tell application "iTerm2"`)
- **WezTerm**: CLI (`wezterm cli split-pane`) or Lua scripting
- **Kitty**: Remote control protocol (`kitten @ launch`, `kitten @ close-window`)
- **Alacritty/tmux-only**: No terminal splits — could fall back to tmux splits instead

Key consideration: new panes need the full PATH to find tmux. iTerm2 panes get minimal environment, so hive passes `env PATH='...'` explicitly. Other terminals may or may not have this issue.

## License

MIT
