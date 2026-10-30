---
description: Close this conversation's window once this turn ends — the transcript stays on disk and it can be resumed later
allowed-tools: Bash(hive close:*)
---

Close this conversation's window with `hive close`. It happens once this turn ends; the
transcript stays on disk, so it can be resumed later from the TUI.

## What to run

Run `hive close` with Bash, once. It takes no arguments.

## Rules

- Don't ask questions and don't run anything else first.
- The window closes as soon as your reply ends. After the command succeeds, reply with
  ONE short line confirming it, and stop.
- If it fails, show the error.
