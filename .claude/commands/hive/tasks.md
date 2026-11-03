---
description: Read and update this project's task board (Apple Reminders) — what's todo, doing, blocked, how stale; create, move, edit or complete tasks following the board's schema; link this conversation to one. Use when asked about tasks, the backlog, what's next or blocked, or to record follow-up work.
argument-hint: "[list | show <id> | add <title> | move <id> <column> | set <id> k=v | done <id> | schema | lint]"
allowed-tools: Bash(hive task:*)
---

This project's tasks live on its own board, backed by a Reminders list (the user also sees and
edits them on their phone, and in the browser at `/tasks/<project>` on the hive dashboard).
**Only `hive task` reads or writes it** — never the Reminders CLI or app scripting, which skip
the schema. The project is the one this directory belongs to (a worktree sees its project's
board), so don't pass `--project` unless asked about another board. No board configured → the
command says so; don't create one.

Arguments: `$ARGUMENTS` (empty → `hive task list`)

## Read

```bash
hive task list                 # open tasks by column, with priority, due date, blocked-by and
                               #   `untouched Nd` on stale ones; --column doing, --json for everything
hive task show <id>            # header fields + body (id: the 8 chars list prints)
hive task schema               # THE RULES for this board — read before writing
hive task lint                 # tasks that already break the rules (read-only)
```

## Two kinds of data on a task

- **Native Reminders fields** — title, priority (`none` `low` `medium` `high`), due date,
  completed, and the last-edited time. They are *not* in the notes header; never copy them there.
  Set them when creating (`--priority`, `--due`) or later with `hive task set <id> priority=high
  due=2026-12-01` (`due=` clears it; `title=` renames, but must keep a leading `[OPS-007]` id).
- **The notes header** — `key: value` lines, then a free-markdown body after `---`. Core
  fields: `status area epic type size est blocked-by files links repo worktree conversation`,
  plus whatever the board adds. `hive task schema` is the authority on names, values and which
  are required; don't rely on this list.

**Statuses** are the same on every board: `todo` (not started) · `doing` (being worked on now) ·
`blocked` (you want to move but something specific stands in the way — needs `blocked-by`) ·
`tracking` (watching only: nothing is owed and it resolves by an outside event — not a place for
work in flight) · `done`. A board may add columns of its own; `hive task schema` lists them.

`untouched Nd` is the time since *anyone* last edited the reminder, phone included. Any write
resets it — so don't edit a task just to look active, and don't write dates into the header
(no `updated:`; Reminders tracks that itself).

## Write — run `hive task schema` first, then follow it

```bash
hive task add [-c doing] [--field area=ops --field type=chore --field size=S] [--priority high] [--due 2026-12-01] [--body '…'] <title>
hive task move <id> <column> [--by '<what it waits on>']   # `done` completes; `blocked` needs --by
hive task set <id> size=M epic=onboarding       # header fields; `key=` removes one
hive task set <id> priority=high due=2026-12-01 # the Reminders fields, same command
hive task done <id>
hive task link <id>            # records this conversation as working on it
```

- Use only the field names and values `schema` prints. Fill what is required (`*`); leave out
  the rest — never write `none`, `-` or an empty value. `blocked-by` exists only while blocked.
- A one-off value the schema has no field for goes in an `x-` field (`--field x-reviewer=niko`):
  free text, optional, allowed on any board. Anything else undeclared is refused. If you keep
  reaching for the same one, tell the user — it belongs in the schema.
- Titles are plain; the board numbers its own ids (`[OPS-007]`) when it has them.
- Move a task when its real state changed: `doing` when you start it (and `hive task link`),
  `blocked --by …` when you hit a wall, `done` when finished and verified. Add a task for
  follow-up you discover but aren't doing now.
- Don't reshuffle, complete, retitle or rewrite tasks you haven't worked on; they're the
  user's plan. Never delete a task or drop information from one — if something doesn't fit a
  field, put it in the body.
- If a command is refused, fix the call from its message — don't work around it. If you keep
  needing a field the board lacks, tell the user; it belongs in the schema.

Reply with what you read or changed in a line or two.
