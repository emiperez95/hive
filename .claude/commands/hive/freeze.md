---
description: Freeze this conversation — close its window once this turn ends, keeping it to thaw later from the board or the TUI
argument-hint: "[why you're parking it]"
allowed-tools: Bash(hive freeze:*)
---

Freeze this conversation with `hive freeze`. Its window closes once this turn ends; the
conversation is kept and shows as a 💤 frozen card on the board and in the TUI, where
it can be thawed (resumed) later.

Arguments: `$ARGUMENTS`

## What to run

Run ONE command with Bash, once:

```bash
hive freeze -- '<note>'
```

The arguments are the **note** — why it's parked, shown on its frozen card. Pass them
after `--` as ONE single-quoted argument (escape a `'` inside it as `'\''`). No
arguments → run `hive freeze` with no note.

## Rules

- Don't ask questions, don't summarise the conversation, and don't run anything else.
- The window closes as soon as your reply ends. After the command succeeds, reply with
  ONE short line confirming it, and stop.
- If it fails, show the error. `hive freeze --help` lists the flags.
