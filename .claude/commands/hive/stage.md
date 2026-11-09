---
description: Move this conversation on the hive board — now, waiting (with a reason, freezes it), later (freezes it) or done (closes it)
argument-hint: "<now|waiting|later|done> [reason]"
allowed-tools: Bash(hive stage:*)
---

Move this conversation's card on the hive board (`/board`) with `hive stage`.

Arguments: `$ARGUMENTS`

## What to run

Build ONE command from the arguments and run it with Bash, once:

```bash
hive stage <stage> -- '<reason>'
```

- `<stage>` is the first argument: `now`, `waiting`, `later` or `done`.
- `waiting` and `later` freeze this window once this turn ends (the card stays in its
  column as a 💤 frozen card to thaw later). `done` closes it once this turn ends. `now`
  thaws a frozen conversation (only reachable with `--id`, since a frozen one can't run this).
- Everything else is the **reason** — kept only for `waiting`. Pass it after `--` as ONE
  single-quoted argument (escape a `'` inside it as `'\''`). No reason → omit `-- …`.

## Rules

- Don't ask questions and don't run anything else first.
- After it succeeds, reply with its output in a line or two and stop. For `waiting`,
  `later` and `done`, that reply is the last thing this window shows — keep it short.
- If it fails, show the error. `hive stage --help` lists the flags.
