# Worktree Lifecycle Hooks

hive's worktree commands (`hive wt new`, `hive wt delete`) can invoke project-specific shell scripts at defined points. Use hooks to allocate ports, set up databases, copy env files, or tear down resources when a worktree is deleted.

## Where hooks live

By default:

```
~/.hive/projects/{project_key}/hooks/
```

Override per-project with the `hooks_dir` field in `~/.hive/projects.toml` (or `--hooks-dir` on `hive project add`). Each hook is a plain shell script named `<event>.sh` and made executable (`chmod +x`).

Missing hooks are no-ops — only the scripts you write are invoked.

## Events

| Event | When it fires | Typical use case |
|---|---|---|
| `pre-create` | Before `git worktree add` runs | Validation, pre-checks |
| `post-worktree` | After `git worktree add` succeeds | Port allocation, resource reservation |
| `post-copy` | After file copy/symlink + memory seed | Database setup, env config, dependency install |
| `post-setup` | After tmux session is created and registered | Final setup (e.g. send a message to Claude) |
| `pre-delete` | Before any teardown starts | Database drop, port release, shared-state cleanup |
| `post-delete` | After worktree, session, and registry are cleaned up | Final housekeeping |

A hook that exits non-zero aborts the current `hive wt` command and surfaces the stderr to the user.

## Environment variables

Every hook receives:

| Variable | Description |
|---|---|
| `HIVE_PROJECT_KEY` | The project key from `projects.toml` (e.g. `my-app`). |
| `HIVE_BRANCH` | The branch name passed to `hive wt new` / `delete`. |
| `HIVE_WORKTREE_PATH` | Absolute path to the worktree on disk. |
| `HIVE_PROJECT_ROOT` | Absolute path to the main project repo. |
| `HIVE_SESSION_NAME` | The tmux session name hive will use. |
| `HIVE_WORKTREE_TYPE` | Worktree type (default `worktree`, or whatever `--type` specified). |
| `HIVE_METADATA` | JSON string: all metadata accumulated by prior hooks this run. Empty object on `pre-create`. |
| `HIVE_METADATA_FILE` | Absolute path a hook can write to, to contribute new metadata keys. |

Within the script:

```bash
#!/usr/bin/env bash
set -euo pipefail

echo "Creating worktree for $HIVE_PROJECT_KEY/$HIVE_BRANCH at $HIVE_WORKTREE_PATH"
```

## Metadata protocol

Hooks contribute state to the worktree by writing a JSON object to `$HIVE_METADATA_FILE`. hive merges this into the worktree's metadata and passes the accumulated JSON to subsequent hooks via `$HIVE_METADATA`.

Example — `post-worktree.sh` allocates a port and records it:

```bash
#!/usr/bin/env bash
set -euo pipefail

# Read prior metadata (none on the first hook of the run).
PREV=$(cat "$HIVE_METADATA")

# Allocate a port based on the branch hash.
PORT=$((3000 + $(printf '%s' "$HIVE_BRANCH" | cksum | awk '{print $1 % 100}')))

# Start the dev server using that port, etc.

# Record it for future hooks + the registry.
cat > "$HIVE_METADATA_FILE" <<EOF
{
  "port": $PORT,
  "started_at": "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
}
EOF
```

Then `post-copy.sh` or `post-setup.sh` can read:

```bash
PORT=$(printf '%s' "$HIVE_METADATA" | jq -r '.port')
```

### Special key: `session_name`

If a hook writes `session_name` into `$HIVE_METADATA_FILE`, hive uses that value as the tmux session name instead of the default (`{emoji} [{project_key}] {branch}`). This lets you slot into custom naming conventions.

All metadata — including `session_name` and any custom keys — is stored in `worktrees.json` and passed through on every subsequent hook invocation for the same worktree.

## Delete-time cleanup

`pre-delete` and `post-delete` hooks receive the same env vars as the create-time hooks. Use them to release resources created during `post-worktree`, `post-copy`, or `post-setup`:

```bash
#!/usr/bin/env bash
# pre-delete.sh
set -euo pipefail

PORT=$(printf '%s' "$HIVE_METADATA" | jq -r '.port // empty')
if [ -n "$PORT" ]; then
  # Stop whatever is listening on $PORT.
  lsof -tiTCP:$PORT | xargs -r kill || true
fi
```

A failing `pre-delete` aborts the deletion; `post-delete` runs after the worktree is already gone, so a failure there is logged but doesn't roll anything back.

## Example: full setup for a Node.js project

```
~/.hive/projects/my-app/hooks/
├── post-worktree.sh   # allocate port, copy .env.example → .env.local
├── post-copy.sh       # pnpm install, prisma migrate
├── pre-delete.sh      # stop dev server, drop test database
```

```bash
# post-worktree.sh
#!/usr/bin/env bash
set -euo pipefail

PORT=$((3000 + RANDOM % 100))
cp "$HIVE_PROJECT_ROOT/.env.example" "$HIVE_WORKTREE_PATH/.env.local"
sed -i '' "s/PORT=3000/PORT=$PORT/" "$HIVE_WORKTREE_PATH/.env.local"

cat > "$HIVE_METADATA_FILE" <<EOF
{ "port": $PORT }
EOF
```

```bash
# post-copy.sh
#!/usr/bin/env bash
set -euo pipefail

cd "$HIVE_WORKTREE_PATH"
pnpm install --prefer-offline
pnpm prisma migrate deploy
```

```bash
# pre-delete.sh
#!/usr/bin/env bash
set -euo pipefail

PORT=$(printf '%s' "$HIVE_METADATA" | jq -r '.port // empty')
[ -n "$PORT" ] && lsof -tiTCP:$PORT | xargs -r kill 2>/dev/null || true
```

## Debugging hooks

- Run `hive wt new` with `--debug` to see hook stdout/stderr streamed.
- Check `~/.cache/hive/debug.log` for the full command line hive invoked.
- Test a hook in isolation by exporting the env vars yourself:

```bash
HIVE_PROJECT_KEY=test \
HIVE_BRANCH=my-feature \
HIVE_WORKTREE_PATH=/tmp/test-wt \
HIVE_PROJECT_ROOT=/tmp/test-repo \
HIVE_SESSION_NAME="🧪 [test] my-feature" \
HIVE_WORKTREE_TYPE=worktree \
HIVE_METADATA='{}' \
HIVE_METADATA_FILE=/tmp/hive-test-metadata.json \
  ~/.hive/projects/test/hooks/post-worktree.sh
```
