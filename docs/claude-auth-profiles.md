# Claude Auth Profiles

Run different Claude Code identities per project (e.g. personal vs company) without logging in/out.

## How it works

Claude Code honors the `CLAUDE_CONFIG_DIR` env var, redirecting its entire config root (`~/.claude/`) to a different directory. Hive passes this env var to tmux sessions via `tmux new-session -e`, so the shell (and any `claude` process inside it) automatically uses the right identity.

Credentials are stored in macOS Keychain with per-config-dir hashed keys — full isolation out of the box.

```
~/.claude/                         # default (personal)
~/.claude-work/                    # "work" profile → CLAUDE_CONFIG_DIR=~/.claude-work
~/.claude-freelance/               # "freelance" profile, etc.
```

Each profile dir has its own credentials, conversation history, and project state. Shared resources (agents, commands, hooks, skills, plugins, settings) are symlinked back to `~/.claude/` so you maintain a single source of truth.

## Creating a new profile

### 1. Create the directory and symlinks

```bash
PROFILE_NAME="work"  # change this
mkdir -p ~/.claude-$PROFILE_NAME

# Symlink shared resources back to ~/.claude/
for item in agents commands hooks skills plugins CLAUDE.md settings.json settings.local.json statusline-command.sh output-styles; do
  [ -e ~/.claude/"$item" ] && ln -s ~/.claude/"$item" ~/.claude-$PROFILE_NAME/"$item"
done
```

### 2. Log in with the new account

```bash
CLAUDE_CONFIG_DIR=~/.claude-$PROFILE_NAME claude auth login
```

Complete the OAuth flow in the browser. Verify:

```bash
CLAUDE_CONFIG_DIR=~/.claude-$PROFILE_NAME claude auth status
# Should show the new account email/org
```

### 3. Sync user-level MCPs

MCPs live in `~/.claude.json` (not `~/.claude/`), which is per-profile. Sync them:

```bash
mcp_json=$(jq '.mcpServers' ~/.claude.json)
jq --argjson mcps "$mcp_json" '.mcpServers = $mcps' ~/.claude-$PROFILE_NAME/.claude.json > /tmp/claude-json-tmp \
  && mv /tmp/claude-json-tmp ~/.claude-$PROFILE_NAME/.claude.json
```

### 4. (Optional) Add a shell alias

Add to `~/.zshrc` for convenient standalone use:

```bash
claude-work() { CLAUDE_CONFIG_DIR="$HOME/.claude-work" claude "$@"; }
```

## Assigning a profile to a hive project

Add `auth_profile = "<name>"` to the project in `~/.hive/projects.toml`:

```toml
[projects.company-app]
emoji = "🏢"
project_root = "~/Projects/company/app"
startup_command = "claude -c"
auth_profile = "work"
```

All sessions created by `hive connect` or `hive wt new` for this project will automatically use the profile's credentials. Worktrees inherit the parent project's `auth_profile`.

## Migrating existing conversations

When switching a project to a new profile, existing conversation history stays in the old profile dir. To migrate:

```bash
# Move all conversation dirs for a project path pattern
mkdir -p ~/.claude-work/projects
for dir in ~/.claude/projects/-Users-*-your-project-pattern-*; do
  [ -d "$dir" ] && mv "$dir" ~/.claude-work/projects/
done
```

Hive's JSONL reader searches all `~/.claude*/projects/` dirs automatically, so conversations are found regardless of which profile dir they're in. But migrating keeps things tidy.

**Important:** if sessions are still running during migration, they may recreate conversation files in the old location. Kill running sessions first, or accept the small split.

## Maintenance

### Re-syncing shared items

When you add new agents, commands, hooks, skills, or plugins to `~/.claude/`, they're automatically available in all profiles (via symlinks). But if you add a new **category** (e.g. a new top-level dir in `~/.claude/`), relink:

```bash
PROFILE_NAME="work"
item="new-directory"
ln -sf ~/.claude/"$item" ~/.claude-$PROFILE_NAME/"$item"
```

### Re-syncing MCPs

After adding/removing MCPs (via `claude mcp add` or editing `~/.claude.json`), re-run the sync command from step 3 above.

### Checking auth status

```bash
# All profiles at a glance
for dir in ~/.claude ~/.claude-*/; do
  name=$(basename "$dir")
  email=$(CLAUDE_CONFIG_DIR="$dir" claude auth status 2>/dev/null | jq -r '.email // "not logged in"')
  echo "$name → $email"
done
```

### Re-authenticating

If a profile's OAuth session expires:

```bash
CLAUDE_CONFIG_DIR=~/.claude-work claude auth login
```

## What's shared vs isolated

| Shared (symlinked) | Isolated (per-profile) |
|--------------------|----------------------|
| `agents/` | `.anthropic/` (credentials) |
| `commands/` | `projects/` (conversation history, auto-memory) |
| `hooks/` | `sessions/` |
| `skills/` | `history.jsonl` |
| `plugins/` | `todos/`, `tasks/`, `plans/` |
| `CLAUDE.md` | `.claude.json` (MCPs, project trust, usage stats) |
| `settings.json` | macOS Keychain entry (auto-isolated) |
| `settings.local.json` | |
| `statusline-command.sh` | |
| `output-styles/` | |

## How hive injects the profile

When `auth_profile` is set on a project:

1. `ProjectConfig::tmux_env()` returns `[("CLAUDE_CONFIG_DIR", "~/.claude-{name}")]`
2. `ensure_tmux_session()` passes it as `tmux new-session -e CLAUDE_CONFIG_DIR=~/.claude-{name}`
3. The initial shell inherits the env var
4. `startup_command` (e.g. `claude -c`) runs inside that shell, picks up the identity
5. Worktree sessions (`hive wt new`) use the same mechanism

`tmux new-session -e` is used instead of `tmux set-environment` because `-e` sets the env for the **initial shell** (which then passes it to child processes like `claude`), while `set-environment` only affects future panes/windows.

## Future: `hive auth` subcommand

The manual steps above could be automated via a `hive auth` CLI:

```
hive auth add <name>          # Create profile + symlinks + login + sync MCPs
hive auth remove <name>       # Delete profile (refuses if referenced by projects)
hive auth list                # Show all profiles with email/org
hive auth status [name]       # Auth status for a specific profile
hive auth relink [name]       # Re-create symlinks (after new shared items)
hive auth sync-mcps [name]    # Sync MCPs from personal profile
```

Not yet implemented — the manual process is straightforward enough for now.
