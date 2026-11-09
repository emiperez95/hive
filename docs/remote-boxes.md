# Remote boxes — moving a conversation between machines

Status: **implemented, phases 1-5** (2026-10-21) — see **As built** for where the code
diverges from the sketch and what is not verified. The two moves it rests on were reproduced
on this machine (see **Evidence**).

## Context for this branch (read first)

This worktree exists to build the feature below. It came out of a research thread
(2026-10-20) whose conclusions are inputs here, not open questions:

- **Why not one Claude process for everything.** In Claude Code 2.1.30x every conversation is
  its own OS process holding one credential and one `CLAUDE_CONFIG_DIR`. Subagents share a
  process but are not resumable first-class conversations. tmux costs ~4 MB; the Claude
  processes cost 150-400 MB each. So hive stays tmux-level and multi-process.
- **Why not `claude --cloud`.** It runs on Anthropic's VM: no local files, MCPs, env or hive
  visibility, and handoff is one-way (`--teleport` is cloud -> local only). Rejected because the
  whole point is keeping local context.
- **Why not containers as the movable unit.** A running container does not move with its
  process state (CRIU is Linux-only and fails on open TLS + pty). Docker Desktop idles at
  960 MB RSS on this 16 GB laptop. Containers only buy a toolchain image and isolation. The
  movable unit is files; the process is disposable, exactly as Claude's own `--bg` supervisor
  and cloud restore treat it.
- **Why "lossless" has a gate.** Background tasks, subagents, queued messages, open prompts and
  running dev servers do not survive a stop. Pack only when Claude's own status is `idle` and no
  background launch is orphaned; hive already reads both signals (`common/claude_sessions.rs`,
  `jsonl::detect_active_background_tasks`).
- **Target server.** A Mac mini with the same username, so absolute paths match and Seatbelt,
  tmux, and hive's macOS-only features (ports via libproc, Chrome JXA, terminal.rs) all work.
  Native Windows has no sandbox and no tmux; a Windows box would mean WSL2, i.e. Linux.
- **Sandbox role.** Not a resource lever (built-in Seatbelt sandbox ~0 RAM, `srt` ~55 MB/session).
  Its job here is to bound the write set so the box is complete by construction (phase 5).
- **Conventions in force.** Build/clippy/test/fmt then `cargo install --path . --root ~/.local`
  to test. Never run unscoped `tmux kill-server`; isolate with `-L` or `TMUX_TMPDIR`. Push only
  `wip/` copies of this branch (`git push origin remote-boxes:wip/remote-boxes`). Reproduce
  before planning: phases 1-2 are testable on this one machine (sshd is on, use a second
  `HIVE_HOME` + isolated tmux socket as the "remote").

Start with phase 1 (`common/boxes.rs`, `hive box pack/unpack`) and its smoke test.

## Goal

Park a long-running Claude conversation on a homeserver (a Mac mini) and pull it back later,
**without committing anything**, and without the TUI/sidebar/phone looking any different:
a conversation that lives on another machine shows in the same list, and `Enter` lands you in
it over SSH.

This is roadmap item (6), the distributed/offload tier, built on freeze/thaw.

## Premise — a conversation is files, the process is disposable

Claude Code itself treats the process as disposable: the `--bg` supervisor kills an idle
session after ~1h and rebuilds it from disk on the next reply; a cloud session whose VM
expires is restored from its transcript. So the unit that moves is never a process or a
container, it is:

| Piece | Where | Moves how |
|---|---|---|
| transcript | `~/.claude*/projects/<slug>/<uuid>.jsonl` | copied |
| memory, overlay, notes, todos | same slug + `~/.hive/cache/*` | copied |
| worktree, incl. uncommitted / staged / untracked / ignored | project worktree path | bundle + rsync (below) |
| unpushed commits | the branch | `git bundle` |
| auth profile | `~/.claude-{name}` | **not** copied; logged in once per machine |

**A sandbox is what makes "everything" enumerable.** If the server runs the session under
`srt` with `allowWrite = [worktree, slug dir, tmp]`, the set of paths that can have changed
is bounded by construction, and a box carrying those paths is complete. The built-in Bash
sandbox bounds only shell commands (file tools follow permission rules), so for a provable
box the whole-process wrapper is the one to use. Measured cost: one node process, ~55 MB RSS
per session. Optional — phase 5.

## Evidence (reproduced 2026-10-20)

1. **Worktree round trip, no commit.** Temp repo: 2 unpushed commits, 1 unstaged edit,
   1 staged file, 1 untracked file, 1 ignored artifact. Pack = `git bundle create
   origin/main..feat` + `git diff --cached --binary` + `rsync -a --exclude .git`. Worktree
   removed, branch deleted. Unpack = fetch bundle → `git worktree add` → rsync → `git apply
   --cached`. `git status --short`, `git log` and file contents identical; HEAD matched. Box
   size 36 KB.
2. **Resume is not cwd-bound.** `claude --resume <id> -p …` from a directory unrelated to
   the conversation's cwd resumed fine (6.7 s) and **appended to the original transcript**
   (35 → 71 lines). Only an empty `memory/` dir appeared under the new slug. So the transcript
   stays anchored to its original slug and lookup is global by id.
3. `sshd` is loaded on this Mac (`launchctl print system/com.openssh.sshd`), so push/pull and
   attach can be exercised against `localhost` before a second machine exists.
4. Container runtimes measured and rejected for this purpose: Docker Desktop idle here =
   960 MB RSS (VM 511 MB) with a 4 GB cap; running containers cannot be moved with their
   process state (CRIU is Linux-only and fails on open TLS + pty). Apple `container` gives
   each box its own micro-VM with reserved memory.

## The box

```
~/.hive/boxes/<uuid>/
  manifest.json      {id, project_key, worktree_key|null, branch, base, head, cwd,
                      profile, session_name, window_name, ports[], note, packed_at,
                      packed_on (hostname), with_ignored: bool}
  commits.bundle     git bundle of <base>..HEAD   (empty when nothing unpushed)
  staged.patch       git diff --cached --binary
  tree/              rsync of the working tree, --exclude .git
                     (ignored files included when with_ignored; default on when both
                      machines are the same OS/arch — a Mac mini matches this laptop)
  transcript.jsonl   + memory/ of the slug
  overlay.json       the conversation's sidecar entry (note, pinned, notes, parent)
  todos.json         the session's active + done todos
```

`base` is the project's base branch (same resolution as `worktree_health`:
`origin/HEAD` → `origin/main` → …). Nothing is committed on the user's behalf; the bundle
carries commits that already exist locally.

## Pack / unpack

**pack(conv)** — `common/boxes.rs`
1. Gate: Claude's own status (`claude_sessions`) is `idle`, no orphaned background launch
   (`detect_active_background_tasks`), no open prompt. `--force` overrides with a warning
   listing what will be lost (below).
2. `freeze_window` as today (kills the window, writes `FrozenEntry`).
3. Write the box. `git bundle`, `git diff --cached`, rsync tree, copy transcript + memory,
   dump overlay + todos, ports from the worktree's hook metadata.
4. Overlay: `location = Some("<remote>")` once pushed (phase 2), so the local recovery frame
   and Browse stop offering it as a local thaw.

**unpack(box) --thaw** — on the target
1. The project must be registered at the **same absolute path** (same username on a Mac mini
   gives this for free; otherwise a symlink). Refuse otherwise.
2. `git fetch <bundle> <branch>:<branch>` into the main checkout (creates the branch if it
   never existed there — verified).
3. Worktree: `hive wt new --from-box` → runs the project hooks (ports, env) exactly as a fresh
   worktree would, then `rsync tree/ → path`, then `git apply --cached staged.patch`.
   For the project's main checkout (no worktree) the tree rsync goes onto it directly and
   HEAD is reset to `head` only if it is an ancestor of the bundle tip.
4. Copy transcript + memory into the same slug under the box's profile dir; merge overlay
   and todos into the local caches.
5. Write a `FrozenEntry` keyed by the conversation id, then `thaw_window` — which already
   resolves the session by cwd, passes `CLAUDE_CONFIG_DIR`, and runs `claude --resume <id>`.

**What is not carried** (why the gate exists): background tasks, subagents and workflows
in flight; queued messages; an open permission prompt; running dev servers; the session's
bash cwd/env (reset to the launch dir); MCP and Remote Control links (reconnected). Identical
to what Claude's own cloud restore drops.

## Transport

```
hive remote add mini --ssh emilianoperez@mini.local --api http://mini.local:8375
hive box push <conv> mini        # pack → rsync box → ssh mini hive box unpack <id> --thaw
hive box pull <conv>             # ssh mini hive box pack <id> → rsync back → unpack --thaw
hive box list                    # local boxes + where each conversation currently lives
```

`[[remotes]]` in `~/.hive/config.toml` (`name`, `ssh`, `api`). rsync over the ssh
target (openrsync on macOS is enough: `-a --exclude`). Server prerequisites: hive installed,
the same projects registered at the same paths (`hive project export | import` — new, small),
each needed profile dir logged in once, tmux + the nested-prefix toggle in its tmux.conf.

## Federation — "no difference"

The registry gains a remote source. `gather_conversations_inner` additionally fetches
`/api/active` + `/api/conversations` from every configured remote (own thread, 1 s cache,
300 ms timeout, a down remote just contributes nothing and shows `@mini ✗` in the title bar).
Remote conversations merge in with:

- `Conversation.host: Option<String>` and `TmuxPlacement.host: Option<String>`. A remote
  placement still makes the conversation **Live** — the Lifecycle rule ("a live placement is
  the sole discriminator") holds, the placement just names a host.
- Rows render a trailing `@mini` tag; grouping stays by project (parent is resolved on the
  remote, by the same rule, and shipped in the view).
- `rank_attention` runs on the merged set so `Ctrl+g`, the sidebar and the phone all see one
  attention order across machines.
- The remote's `/api/active` already carries status, tier, age and pings; nothing is
  re-derived locally.

**Enter on a remote-live conversation** → `attach_remote`: find or create a local tmux window
in the conversation's parent session (same name as the remote window, so the local Active
view looks like the conversation is here) running

```
ssh -t <host> tmux attach -t '=<session>:<window>'
```

and switch to it; a second `Enter` just switches. Nested tmux is the one visible seam: the
server's tmux.conf gets `status off` plus the standard `F12` toggle that sets `prefix None`
and an `off` key-table on the outer tmux, so the inner one receives the prefix. (`tmux -CC`
over ssh was rejected: iTerm would open native tabs outside hive's window model.)

**Ports**: the box manifest lists them; `attach_remote` opens `ssh -L port:localhost:port`
for each, so the detail view's port list, Chrome matching and `Enter`-on-port keep working
against `localhost` as before.

**Actions on a remote row** proxy to the remote API, which already exists for the phone:
send (`/api/send`), freeze/thaw/discard (`/api/freeze` …), todos, mute, switch. Freeze of a
remote conversation produces a remote box; `pull` brings it home.

**Notifications**: the server's hook notifier would notify the server's screen. The local
side instead reuses `StateAges` + `mark_pings` on the merged set: a transition on a remote
conversation raises the local `terminal-notifier` exactly as a local one does (honouring the
three mute levels and `M`). No inbound port on the laptop.

**Phone dashboard**: `hive web` on the laptop runs the same merged gather, so `/api/active`
already includes remote conversations; `web.html` needs only the `@host` tag.

## Phases

| # | Deliverable | Test |
|---|---|---|
| 0 | spike — done, see Evidence | — |
| 1 | `hive box pack/unpack` local, `common/boxes.rs`; manifest; gate | unit: manifest roundtrip, bundle range (`base..HEAD`), staged patch, gate refuses busy/shell/bg. smoke: temp repo pack → remove worktree → unpack → identical `git status`/`log`/tree; freeze → pack → unpack → thaw resumes the id |
| 2 | `hive remote add/list`, `box push/pull` over ssh, `project export/import` | against `localhost` sshd on this machine, a second `HIVE_HOME`/`TMUX_TMPDIR` as the "remote" |
| 3 | federation read-only: remote rows in TUI, sidebar, web; `attach_remote` ssh window; port tunnels | `build_active` tests with a host-tagged placement; a down remote contributes nothing and never blocks the 1 s tick |
| 4 | proxied actions + local notifications for remote transitions; recovery frame ignores `location != local` | existing `should_notify` + `mark_pings` tests extended with a remote conversation |
| 5 | sandbox-defined boxes: thaw on the server launches `srt --settings <per-box>` whose `allowWrite` is exactly the box's path set | `srt` present on the server; box completeness check = no writes outside the set |

## Phase 1 — as built (2026-10-20)

`common/boxes.rs` + `cli/boxes.rs`: `hive box pack <conv> [--force] [--without-ignored]`,
`hive box unpack <id|path> [--thaw] [--force]`, `hive box list`. Differences from the sketch above,
found by reproducing against a temp repo first:

- The bundle is `refs/heads/<branch> --not --remotes` ("unpushed" = on no remote, as `prune.rs`
  asks), **not** `base..HEAD`: a range ending in `HEAD` carries a ref named `HEAD`, so
  `fetch bundle feat:feat` fails. No bundle at all when nothing is unpushed.
- rsync runs without `--delete`; unpack instead removes tracked files the box lacks. `--delete`
  would wipe a main checkout's ignored files when the box was packed `--without-ignored`.
- Unpack refuses (rather than overwrites) a dirty main checkout, a non-empty worktree path, a
  diverged branch, a missing profile dir, an unregistered project, and a local transcript that
  is longer than the box's (`--force` overrides only the last).
- Pack order is gate → freeze → write; if the write fails the window stays frozen (thawable).
- Unpack runs the project's `post-worktree` and `post-copy` hooks on the fresh worktree
  (before the tree is overlaid, so the box wins), and what they report becomes the registered
  worktree's metadata. There is no `wt new --from-box`; unpack does it directly.

## As built — phases 2-5

**Phase 2 — transport.** `hive remote add|list` (`[[remotes]]` appended to `config.toml` as
text, so comments and `[pricing]` survive), `hive box push|pull`, `hive project export|import`.
`common/remote.rs` runs `ssh -o BatchMode=yes` (never prompts) and `rsync -e`; `HIVE_SSH`
overrides the program, which is how the tests stand a second `$HOME` in for a second machine.
- `push` = pack → rsync the box → remote `hive box unpack --thaw` → `box mark` locally
  (overlay `location = <remote>`, local frozen entry discarded). `pull` is the reverse and
  marks the remote through `hive box mark` over ssh.
- Nothing that exists only on a side is removed: the sender's linked worktree is kept unless
  `push --remove-local` / `pull --remove-remote`. `pull --remove-remote` releases the remote
  copy once the box is on *this* disk and before unpacking (the box is the backup; and it is
  what lets two machines sharing paths in a test work).
- `reopen_conversation` refuses a conversation whose overlay has a `location`: resuming a
  shipped conversation locally would fork its transcript.

**HTTP transport (added after phase 5).** A remote with an `api` now takes boxes over HTTP —
no ssh, rsync or `hive`-on-PATH needed, so a host without rsync still works; `--via-ssh`
forces the old path, and a remote without an `api` still uses it. The server side is
`serve/boxes_api.rs`: `PUT|GET /api/box/<id>` stream a tarball, `POST …/pack|unpack|mark` run the
same code the CLI runs, each request on its own thread (the request loop is serial). `hive box
send` is the transfer half of `push`, for retrying without re-freezing.
- **No authentication, by design** (trusted LAN, like the rest of the dashboard). What stands in
  for it: `[web] accept_boxes = true` is required (default off → 403), and `boxes::unpack`
  refuses, on every transport, a `root` outside the project's checkout / worktrees dir /
  an already-registered worktree (no relative paths, no `..`), and any box id, profile or slug
  that isn't a plain name. A received tarball is extracted into a scratch dir and installed
  only if its manifest's id matches.
- Not hardened: `tar -x` is trusted to refuse `..` members (bsdtar and GNU tar both do), and
  anyone on the LAN can fill the disk by PUTting boxes to an opted-in server.
- Projects need a worktrees dir (`--worktrees-dir`) for linked-worktree boxes to be accepted.

**Phase 3 — federation (read-only).** Deviation: the peer's wire is `GET /api/registry`
(the registry's own serde `Conversation`s: live + parked, local only), not `/api/active` +
`/api/conversations` — those are lossy projections (no parent, no placement) and the peer needs
the model to merge. One poller thread per remote with an `api` (300 ms connect, 800 ms read,
1 s cadence); the gather only reads the cache, so a down remote contributes nothing and shows
`@mini ✗`. Only the long-running loops federate (`gather_conversations_stats`); one-shots such as
`box pack` use the plain gather and so can never pick a remote row.
- A remote conversation is `Live` with `Conversation.host = Some(name)`. Every local action that
  takes a placement at face value is guarded: `Conversation::is_remote()` / `flag_session()`.
- Remote sessions are named `"<session> @<host>"` in `SessionView`, build_active groups and
  every local flag set, so they cannot collide with a same-named local session.
- `Enter` → `remote::attach_remote`: a window `@mini:<name>` in the conversation's parent
  session (else a session `@mini`) running `ssh -t [-L p:localhost:p …] host tmux attach …`.
  A second `Enter` finds it. The Active view hides these windows (the `@` prefix).
- The recovery frame, `/metrics` and the attention git probe ignore remote rows.

**Phase 4 — actions.** Web: a request naming a remote session (`/api/send`, `freeze`, `todos`,
`thaw`, `discard-frozen`, `kill-session`, `switch`, and the GETs `messages`, `session-info`,
`todos`) is performed by that remote via its own API (`remote_get` / `remote_post`); `switch`
opens the local ssh window instead; `kill-session` is refused. TUI: `Enter` attaches or thaws on
the remote, `z` freezes there, `Del` on a parked row discards there; kill-window is refused.
Notifications: `RemoteNotifier` (web data thread) rings locally on a remote conversation
*entering* a blocked state, honouring the three mute levels and `M` via `should_notify`; flags
on a remote row are local files keyed by the `"<session> @<host>"` name.

**Phase 5 — sandbox.** `[sandbox] enabled / allowed_domains / extra_write` and
`box unpack|push --sandbox`. `common/sandbox.rs` derives the `srt` settings from the manifest:
`allowWrite` = the worktree (+ the repo's `.git` for a linked worktree, since commits are
written there) + the profile dir + `~/.hive` + temp + `extra_write`; network = Anthropic's
domains + `allowed_domains`. Thaw wraps `claude` as `srt --settings … -c 'claude --resume id'`
(not positional: commander would read `--resume` as its own option) and stays sandboxed across
freeze → thaw. A box packed from a sandboxed conversation records the set (`manifest.sandbox`)
and is "complete by construction"; `extra_write` entries are reported as completeness gaps.
Verified with the real `srt`: a write outside the set is refused by the kernel, and a real
`claude -p` runs under exactly the derived write set and domains.

**Added for the homeserver (Argus).**
- **Remote PATH.** Every command hive runs over ssh is prefixed with `remote::REMOTE_PATH`
  (`/opt/homebrew/bin`, `/usr/local/bin`, `~/.local/bin`, `~/.cargo/bin`): a non-interactive ssh
  shell on macOS has none of them, so `ssh -t host tmux attach` said "command not found" on a
  machine where tmux works at its own prompt. `$PATH`/`$HOME` expand on the remote shell.
- **`[sandbox] deny_read`.** Paths the conversation may not read (`~` expanded) — another
  program's database, other projects' credentials. Enforced by `srt` in the kernel (`cat`, `ls`
  and a read-only `sqlite3` open all refused; verified), unlike a `Read(...)` deny rule, which stops
  the Read tool but not `cat` through Bash. Emitted with an `allowRead` of the box's own paths so a
  deny over a parent can't lock the session out of its tree. Empty by default.

**Not done / not verified**
- Everything ssh was exercised through a shim (a second `$HOME`), not a real second machine or
  `localhost` sshd (key auth to localhost isn't set up here). Key-based ssh, a PATH where
  `hive` is missing (`--hive-path` exists for it), and the nested-tmux prefix toggle in the
  server's `tmux.conf` are documented, not automated or tested.
- Packing a *running* window (gate against live status, then freeze) is covered by unit tests
  of the gate and by the code path, but no test can produce a detectable live Claude (detection
  keys off the real binary's version-string process name).
- `Ctrl+g` (`cycle-free`) does not visit remote windows; the sidebar/web attention order is
  merged. Pings on remote rows clear when the conversation goes busy, not when you land on its
  ssh window. Remote todos are web-proxied only; the TUI detail disables them for remote rows.
  `todo-dispatch`, `conv-notes` and `conv-stats` are not proxied.
- `srt` network defaults are Anthropic-only; a sandboxed session that needs git/npm needs
  `allowed_domains`.

## Open questions

- Ignored artifacts: rsync (default on same OS/arch, measured cheap) vs rebuild via hooks.
  `--with-ignored` / `--without-ignored` on pack; manifest records which.
- `projects.toml` sync: one-shot `export/import` first; a shared file is a later concern.
- The two machines keep separate `~/.hive` caches and recovery frames — by design — but the
  overlay's `location` must be authoritative so a conversation is offered by exactly one
  side. Rule: the side holding a live placement or an un-shipped box owns it.
- Typing latency over LAN ssh is ~1 ms; over WAN consider `mosh` (not installed).
- Server sessions run unattended more often: turn the built-in sandbox on there regardless of
  phase 5 (zero cost on macOS).
