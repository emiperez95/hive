---
description: Move this conversation on the hive board — todo, planning, in-progress, blocked (with a reason) or done; optionally freeze it
argument-hint: "<todo|planning|in-progress|blocked|done> [--freeze] [reason]"
allowed-tools: Bash(hive stage:*)
---

Move this conversation's card on the hive board (`/board`) with `hive stage`.

Arguments: `$ARGUMENTS`

## What to run

Build ONE command from the arguments and run it with Bash, once:

```bash
hive stage <stage> [--freeze] -- '<reason>'
```

- `<stage>` is the first argument: `todo`, `planning`, `in-progress`, `blocked` or `done`.
- `--freeze` — also freeze this window once this turn ends (the card stays in its column
  as a 💤 frozen card to thaw later). Only if the arguments ask for it.
- Everything else is the **reason** — kept only for `blocked`. Pass it after `--` as ONE
  single-quoted argument (escape a `'` inside it as `'\''`). No reason → omit `-- …`.

## Rules

- Don't ask questions and don't run anything else first.
- After it succeeds, reply with its output in a line or two and stop. With `--freeze`,
  that reply is the last thing this window shows — keep it short.
- If it fails, show the error. `hive stage --help` lists the flags.
