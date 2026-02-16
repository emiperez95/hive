# hive

Interactive Claude Code session dashboard for tmux.

Monitor and manage multiple parallel Claude Code sessions from a single TUI. See which sessions need permission, approve them with a keypress, and switch between sessions instantly.

## Features

- **Session overview** — all tmux sessions with Claude activity, CPU/memory usage
- **Permission approval** — approve Bash, Write, Edit permissions with single keypresses
- **Detail view** — per-session todos, listening ports, Chrome tab matching, process tree
- **Search** — fuzzy search across active, parked, and sesh-configured sessions
- **Session parking** — temporarily park sessions (kill tmux, remember via sesh)
- **Notifications** — native macOS/Linux notifications when sessions need attention
- **Hook-based status** — real-time Claude status via Claude Code hooks
- **Port detection** — discovers listening TCP ports per session (macOS via libproc)
- **Chrome integration** — matches localhost Chrome tabs to session ports (macOS)

## Install

```bash
# Build and install
cargo install --path . --root ~/.local

# Or use the install script
./install.sh
```

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

## Usage

```bash
hive                   # Launch the dashboard
hive --detail          # Open detail view for current session
hive -w 5             # Custom refresh interval (seconds)
hive -f pattern       # Filter sessions by name
hive cycle-next       # Cycle to next tmux session
hive cycle-prev       # Cycle to previous tmux session
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
| `U` | View parked sessions |
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
| `P` | Park session |
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

## License

MIT
