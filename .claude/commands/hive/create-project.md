---
description: Register a new project with hive
---

You are an interactive wizard that helps the user register a new project with `hive project add`.

## Step 1: Core Info

Ask the user for:
1. **Project key** — short identifier (e.g., `clear-session`, `hive`). Suggest one based on the current git repo name if available.
2. **Path** — project root path. Default: current directory (`$PWD`).
3. **Emoji** — single emoji for session naming (e.g., `🐝`, `🌊`). Required.
4. **Display name** — optional human-friendly name (defaults to key).

## Step 2: Startup

Ask about startup command:
- Default: `claude -c` (Claude Code with continue flag)
- User may want a custom command or no startup command

## Step 3: Worktree Config

Ask if the user plans to use worktrees with this project. If yes:
1. **Worktrees directory** — where worktrees are created (e.g., `~/Projects/features/`)
2. **Base branch** — default branch for worktree creation (e.g., `main`, `staging`, `develop`)
3. **Package manager** — `npm`, `pnpm`, `yarn`, `bun`, or none

## Step 4: File Patterns

If worktrees are enabled, ask about files to set up in new worktrees:
1. **Copy files** — files copied into each worktree (e.g., `.env`, `.env.local`)
2. **Symlink files** — files symlinked from the main project (e.g., `node_modules`, `.next`)

Each can be specified as a list. Empty is fine.

## Step 5: Ports & Database

Ask about:
1. **Port management** — enable if the project uses ports that need to differ per worktree
   - If yes: base port (e.g., `3000`) and increment (e.g., `100`)
2. **Database management** — enable if the project uses databases per worktree
   - If yes: database name prefix (e.g., `cleardb`)

## Step 6: Hooks

Ask if the user has custom lifecycle hooks:
- If yes, ask for the hooks directory path
- If no, hive uses the default `~/.hive/projects/{key}/hooks/`

## Step 7: Confirm & Run

Show a summary of all collected values and the command that will be run. Then execute:

```bash
hive project add <key> \
  --emoji <emoji> \
  [--path <path>] \
  [--display-name <name>] \
  [--startup <cmd>] \
  [--worktrees-dir <dir>] \
  [--base-branch <branch>] \
  [--package-manager <pm>] \
  [--copy <file1> --copy <file2> ...] \
  [--symlink <file1> --symlink <file2> ...] \
  [--ports-enabled --base-port <port> --port-increment <inc>] \
  [--db-enabled --db-prefix <prefix>] \
  [--hooks-dir <dir>]
```

Only include flags for values the user provided (skip defaults/empty values).

## Step 8: Connect (Optional)

After successful registration, ask if the user wants to open a tmux session for the project:

```bash
hive connect <key>
```

## Guidelines

- Use `AskUserQuestion` for each step to collect input interactively
- Skip steps that aren't relevant (e.g., skip file patterns if no worktrees)
- Validate the project key doesn't already exist: run `hive project list` first
- Keep it conversational — don't dump all questions at once
