# Troubleshooting

Common issues and how to diagnose them. If none of these match, open an issue with the bug template and include `~/.cache/hive/debug.log` (generate with `hive --debug <your-command>`).

## Status in the dashboard never updates

The TUI shows sessions as idle / no status forever.

**Likely causes**

1. `hive setup` wasn't run, so Claude Code has no hook entries pointing at hive.
2. `hive setup` was run with a different `hive` binary path than the one currently on your `PATH` (you moved or reinstalled it).
3. Claude Code was already running when setup happened — it only loads hooks on startup.

**Fix**

```bash
hive setup              # idempotent; shows what's missing
```

Then restart any running `claude` sessions. The `command` field in `~/.claude/settings.json` contains the absolute path to hive, written at the moment setup ran — if you later install hive to a different location, re-run `hive setup` to refresh.

Verify hooks are live:

```bash
cat ~/.claude/settings.json | jq '.hooks'
```

You should see an entry per hook event (Stop, PreToolUse, PostToolUse, UserPromptSubmit, PermissionRequest) with the absolute path to your hive binary.

## Tmux keybindings disappear on reboot / new shell

`prefix+s`, `prefix+d`, `Ctrl+n`, `Ctrl+p` work during one session but are gone the next day.

This is expected: `hive setup` runs `tmux bind-key` against the **currently running tmux server only**. Tmux discards those bindings when the server is killed (reboot, `tmux kill-server`, idle timeout depending on your config).

**Fix** — paste the snippets that `hive setup` prints at the end of the tmux-binding step into `~/.tmux.conf`, then `tmux source-file ~/.tmux.conf`:

```tmux
bind-key s display-popup -E -w 80% -h 70% "/path/to/hive"
bind-key d display-popup -E -w 80% -h 70% "/path/to/hive --detail"
bind-key -n C-n run-shell "/path/to/hive cycle-next"
bind-key -n C-p run-shell "/path/to/hive cycle-prev"
```

Re-run `hive setup` on a live tmux server to regenerate the exact paths for your install.

## Tmux binding conflicts with another tool

Another tool already binds `prefix+s` (e.g. `tmux-sensible`) and you want hive there instead.

`hive setup` doesn't detect conflicts — it runs `bind-key` which silently overrides. If you want the opposite (keep your existing binding), either:

- Don't accept the tmux-binding prompt during `hive setup`.
- Rebind manually in `~/.tmux.conf` to a different key: copy the `display-popup …` command and bind it to `prefix+H` or whatever's free.

## `hive update` fails

```
Download failed. Check that a release exists at https://github.com/emiperez95/hive/releases/latest
```

**Likely causes**

1. No network / GitHub is down.
2. Your OS/arch doesn't have a prebuilt binary (hive currently ships mac Apple Silicon + Intel only).
3. You're behind a proxy that needs to be configured for `curl`.

**Workaround** — build from source:

```bash
cd /path/to/hive-clone
git pull
cargo install --path . --root ~/.local
hive setup
```

## `hive start` panics with "failed to initialize terminal"

You ran `hive start` (or any TUI command) in a non-interactive context — a pipe, a cron job, a script with redirected stdin.

```
hive: this command needs a terminal. Run it from an interactive shell.
```

**Fix** — only run TUI commands from interactive terminals. The non-TUI subcommands (`hive hook`, `hive project`, `hive todo`, `hive setup`, `hive uninstall`, `hive cycle-next`, etc.) work fine without a TTY.

## Notifications appear during automated tests or CI

Each `PermissionRequest` hook fire spawns a real macOS notification. If you're running a test harness or simulating hook events, this clutters your screen.

**Fix** — set `HIVE_NO_NOTIFY=1` in the environment. All notification paths (`terminal-notifier`, `osascript`, `notify-send`, tmux display-message fallback) short-circuit immediately.

```bash
HIVE_NO_NOTIFY=1 echo '{...}' | hive hook PermissionRequest
```

## `hive setup` says tmux bindings are `[ok]` but they don't work

Setup checks the running tmux server via `tmux list-keys` and looks for entries containing "hive" in the command. If your tmux server was restarted after setup, the bindings are gone but old ones may still be reported as OK in some edge cases.

**Fix** — re-run `hive setup` after starting a fresh tmux server. The status line reflects the current live state.

## Hooks don't fire in one specific project

Claude Code's `settings.json` is merged from multiple levels — user-level (`~/.claude/settings.json`, where hive writes), project-level (`.claude/settings.json`), and enterprise (`/Library/Application Support/ClaudeCode/managed-settings.json`). A project-level override can mask user-level hooks.

**Check**

```bash
cat path/to/project/.claude/settings.json 2>/dev/null
```

If a project file exists and overrides the hooks object, merge your hive hook entries into it manually (or delete the project file if you want user-level hooks to apply).

## Approval keypress (`y`, `Y`) doesn't actually approve

Approval works by sending tmux keystrokes (`1` or `2` then `Enter`) to the Claude pane. This relies on the Claude dialog being the focused widget in that pane.

**Common misses**

- A tool like `fzf` is open in front of the Claude prompt — approval keys go to fzf instead.
- The pane coordinates (session / window / pane index) changed between TUI refresh and keypress.
- You're running `claude` with a non-interactive mode that doesn't use the default dialog UI.

**Fix** — switch to the session (`Enter` from the TUI list view) and approve directly in Claude's prompt if the keystroke-injection approach doesn't work.

## Debug logging

Most non-TUI commands honor `--debug` and write to `~/.cache/hive/debug.log`. The TUI itself logs refresh cycles and state reads when you pass `--debug`.

```bash
hive --debug hook PreToolUse < payload.json
cat ~/.cache/hive/debug.log
```

If a specific subcommand doesn't produce log output, that's a gap — please file an issue.

## `~/.hive/` keeps growing

hive writes per-session state into `~/.hive/cache/`:

- `state.json` — current status for every session seen via hooks.
- `todos.txt`, `todos-done.txt` — todos keyed by tmux session name.
- `muted.txt`, `skipped.txt`, `auto-approve.txt`, `favorites.txt` — flags.
- `debug.log` — grows with `--debug` usage.

None of these grow unbounded in normal use, but if you've been bouncing between many sessions you can safely `rm -rf ~/.hive/cache/` to reset all per-session state without affecting the project registry (`~/.hive/projects.toml` stays).
