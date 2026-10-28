---
description: Split this conversation in two — a fork carries the same history on in another window (beside this one, a new worktree, or another project)
argument-hint: "[--wt <branch> | --to <project[/branch]>] [--switch] [what the fork should do]"
allowed-tools: Bash(hive fork:*)
---

Split this conversation with `hive fork`. The fork gets this whole conversation as its
history and runs in its own tmux window; this conversation stays exactly where it is.

Arguments: `$ARGUMENTS`

## What to run

Build ONE command from the arguments and run it with Bash, once:

```bash
hive fork [--wt <branch> [--base <branch>] | --to <key>] [--switch] -- '<directive>'
```

- `--wt <branch>` — fork into a NEW worktree of this project, branched off the branch
  checked out here. `--to <key>` — fork into a registered project (`hive`) or worktree
  (`hive/feature-x`). Neither — the fork opens beside this conversation, same directory.
- `--switch` — move the tmux client to the fork. Without it you stay here.
- Everything in the arguments that isn't one of those flags is the **directive**: what
  the fork should work on. Pass it after `--` as ONE single-quoted argument (escape a `'`
  inside it as `'\''`). No directive → omit the `--` part; the fork will wait.

## Rules

- Don't ask questions and don't run anything else first — the flags are explicit.
- The directive is the FORK's job, not yours. After the command succeeds, reply with
  its output in two or three lines (fork id, where it runs) and stop. Do not start on
  the directive here.
- If it fails, show the error. `hive fork --help` lists the flags.
