# Task schemas (draft — enforcement OFF)

Every board speaks one **core**; a board adds fields, narrows a core field's values, and says
what it requires (`src/common/task_schema.rs`, `[projects.<key>.tasks]`). `strict = true`
turns on enforcement for `hive task add / set / move`; until then `hive task lint` only
reports. Edits made in the Reminders app are never blocked — lint is how drift shows.

## Core (canonical header order)

| field | kind | meaning |
|---|---|---|
| `status` * | todo · doing · blocked · tracking · done (+ a board's extras) | board column. doing = being worked now; blocked = needs `blocked-by`; tracking = watching only, nothing owed; done completes the reminder |
| `area` | text (narrowed per board) | coarse category of the work |
| `epic` | text | named stream of related tasks |
| `type` | chore · sync · review · research · feature · design | kind of work |
| `size` | XS · S · M · L · XL | relative effort |
| `est` | 30m · 2h · 1d | time estimate |
| `blocked-by` | text | only while status is `blocked` — absent otherwise, never `none` |
| `files` | comma-separated repo paths | what the task reads or produces |
| `links` | comma-separated URLs | |
| `repo` | URL | |
| `worktree` | path | |
| `conversation` | UUID | set by `hive task link` |

Cross-field rules: `blocked` ⇔ `blocked-by` present; `done` ⇔ reminder completed.

## What each existing list becomes (nothing dropped — renamed, split or grouped)

| Thesis today | → |
|---|---|
| `status` todo/blocked/done/**doing** | same (the board now has all four columns) |
| `area` (experiments, tooling, literatura, admin) | `area`, closed set |
| `epic` | `epic` |
| `blocked-by: none` (33×) | field removed; kept only on blocked tasks |
| `artefact` | `files` — **split**: the one `https://claude.ai/artifact/…` goes to `links` |
| `est` | `est` |
| `log` (yes/no) | Thesis-only field `log` (bool) |
| title tag `[TUT]` `[F6]` … | stays in the title (free text; derivable from area/epic) |

| AWS Projects today | → |
|---|---|
| `project` (symposium, ops, roadmap, link) | **`area`** (closed set; also picks the title id `[SYMP-nnn]`) |
| `area` (onboarding, skills-loader, …) | **`epic`** — same idea as Thesis's `epic` |
| `type`, `size`, `status` | same (`tracking` and `blocked` are now generic columns on every board) |
| `materials` | `files` |
| `repo`, `links`, `worktree` | same (`repo: … (private)` → drop the suffix, it isn't a URL) |
| `parent` (1×) | AWS-only field `parent` |
| `## Goal` body | unchanged (body is free markdown) |

`area` and `epic` swap meaning on AWS — the single reason for `renames` being applied together.
Completed reminders whose `status` still says todo/doing (9 on Thesis, 3 on AWS) are set to
`done`.

## Rollout

1. Review this; adjust fields/values.
2. Put the schema in `projects.toml` with `strict = false`; run `hive task lint` until the
   report is what you expect.
3. Migrate existing reminders (rename/split/regroup as above) — a separate, explicit step.
4. Remove `renames`, set `strict = true`.
