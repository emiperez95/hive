# Changelog

All notable changes to hive are recorded here. Format loosely follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **`prefix + a` / `hive --project-detail`** — open the current window's *project* detail
  straight from tmux, alongside `prefix + s` (list) and `prefix + d` (conversation detail). The
  project resolves from the current window's conversation, or — when the window isn't running
  Claude — from the pane's cwd. A worktree resolves up to its project. `hive setup` registers
  and reports the binding; `hive uninstall` already removed it.
- **Frozen conversations are sectioned in the project detail** under a `💤 Frozen (n)` header,
  with `Closed (n)` marking where the parked ones end.
- **`GET /metrics` on `hive web`** — Prometheus text exposition, so hive's view of the work can
  be scraped into the same dashboard as Claude Code's own OpenTelemetry export. Claude's
  telemetry reports what a session *spent*; it structurally cannot report how many sessions
  exist, which project they belong to, whether they're live or merely resumable, or whether
  they're stopped waiting on a human. Series cover window concurrency, conversations by
  lifecycle / project / status / auth profile, conversations blocked awaiting a human decision,
  worktree debt (dead worktrees per project), per-project active time and CPU/memory, todos, and
  lifecycle counters.

  A scrape endpoint rather than an OTLP push on purpose: `opentelemetry-otlp`'s gRPC transport
  pulls in tonic and an async runtime, and hive is synchronous by design. The registry
  aggregates are built by the web data thread, which already gathers once a second — a scrape
  never triggers a gather of its own.

  A ready-to-run collector + Prometheus + Grafana stack lives outside this repo, in
  `claude-logging/otel-stack/`.

### Changed

- **Project detail lists worktrees *after* conversations**, and omits the section entirely when
  the project has none. The cursor order follows the drawn order: todos → conversations →
  worktrees.

### Fixed

- **A `cd` inside a session blanked its conversation view.** The web dashboard showed an empty
  page for a live session whose agent had moved into a subdirectory. Two different things are
  called "cwd" and hive conflated them: the directory Claude was *launched* in, which fixes the
  transcript's path for the life of the conversation, and the agent's *current* shell directory,
  which every `cd` moves and which Claude reports in its hook payloads. hive stored the second and
  reconstructed the transcript path from it, so once they diverged both the exact `<id>.jsonl`
  check and the recency fallback searched a directory the transcript was never in.
  `resolve_jsonl_path` now falls back to the conversation id, which never drifts:
  `find_jsonl_by_session_id_anywhere` scans every `~/.claude*/projects/*/`, one `is_file()` per
  slug dir and only on the miss path. The same resolver backs `get_claude_status_from_jsonl_for`,
  so the TUI's transcript tail had the identical blind spot.

  A known id that resolves to nothing now returns `None` instead of the newest transcript for the
  cwd. That fallback was worse than the blank page it was covering: asked for an id that exists
  nowhere, it answered with an unrelated session's transcript and nothing marked the substitution.
- **An archived project could hide a live conversation.** Archiving is meant to be a display
  preference, but `build_browse` dropped the whole group from the unfiltered Browse list — so a
  conversation started in an archived project was invisible, live rows and all. Two independent
  guards now cover it: `projects::activate_project` clears the flag wherever work starts (new
  conversation in a project or worktree, resume/thaw — so the web's `/api/resume` is covered too —
  `connect_worktree`, `hive connect`, `hive wt new`), and `build_browse` never hides an archived
  project that has a live conversation. The second is what catches a bare `claude` run in a tmux
  window, which no unarchive-on-start hook can observe; its worktree rows come back with it.
- **Resuming a conversation opened a window that never started Claude.** Enter on a closed
  conversation added the tmux window and switched to it, then left it sitting at a bare shell.
  The exact-match sweep (`=name` targets) had routed `send-keys` through `tmux::exact` too —
  but `send-keys` takes a *pane* target, and `=name` is a session, so tmux answered `can't find
  pane: =name` and the `claude --resume <id>` was never typed. The failure was silent because
  the send was already best-effort.

  The same line broke every path that opens a window and types into it: thaw (`z`), new
  conversation (`n`), new task, `hive wt new`'s startup command, and **every session hive
  creates** via `ensure_tmux_session` — projects with a startup command came up at a shell.
  All six now use `tmux::exact_active_pane` (`=name:`), which keeps the `=` exactness (verified
  it still refuses to prefix-match a longer live session) while resolving to the current
  window's active pane — the window `new-window` just created.
- **Frozen conversations were effectively invisible in the project detail.** They sorted below
  every plain closed conversation, so the one you froze in order to come back to it fell past
  the 5-row page cap — on the very screen whose title bar counts it. They now sort directly
  under the live rows.
- **README documented the retired classic TUI.** The keyboard-shortcut tables still listed
  single-keypress permission approval (never ported to the conversation-first TUI — see
  `docs/permission-approve-reject.md`) and flags that no longer exist (`hive -w`). Rewritten
  against the shipped key map: list, project detail, conversation detail.
- **Every usage-stats request re-read the whole macOS power log.** `compute_stats` calls
  `machine::sleep_intervals` to subtract machine-sleep from focus time, which shells out to
  `pmset -g log` — a ~1.8s call rendering ~45k lines. That cost was already being paid on every
  web `/api/stats` load, and a scraped `/metrics` would have made it continuous. The raw output
  is now memoized for 5 minutes (per process, successful reads only), taking `/metrics` to
  ~25ms. Sleep history only grows at the tail and old spans never change, so the staleness costs
  at most five minutes of accuracy on the newest interval. One-shot CLI runs (`hive stats`) see a
  cold cache and behave exactly as before.
- **`.githooks/pre-commit` announced version bumps it never made.** It wrote the version with
  sed's `0,/re/` address, a GNU extension that BSD sed (macOS) accepts, exits 0 on, and ignores
  — so `Cargo.toml` sat at `0.1.0` while every commit reported a bump. Now uses awk and verifies
  the write, failing the commit if the version didn't land. Also swapped `cargo generate-lockfile`
  for `cargo update --workspace`, so unrelated third-party upgrades stop being swept into
  whatever commit is in flight.

## [0.1.0] — First public release

First tagged public release. macOS-only (Apple Silicon + Intel). Linux is deferred.

### Added

- **Interactive TUI** for monitoring multiple parallel Claude Code sessions across tmux.
- **Permission approval** via single-keypress (`y/z/x/w/v` once, `Y/Z/X/W/V` always).
- **Detail view** per session: todos, listening ports, Chrome tab matches, process tree.
- **Project registry** (`hive project add/list/remove/import`) stored in `~/.hive/projects.toml`.
- **Worktree lifecycle** (`hive wt new/delete/list`) with six project-level hook points.
- **Mobile web dashboard** (`hive web`) — phone-friendly UI with markdown conversation view, tool-use cards, message sending, permission approval, HLS-streamed TTS playback, per-session info modal.
- **Chrome tab matching** and **iTerm2 pane spread** (`hive spread N` / `hive collapse`).
- **Per-project auth profiles** via `CLAUDE_CONFIG_DIR` — multiple Claude identities on one machine.
- **Prebuilt binary install path**: `install.sh` downloads the latest tarball from GitHub Releases, no Rust toolchain required.
- **`hive update`** fetches the latest release, replaces the running executable, and re-runs `hive setup`.
- **`hive setup`** registers hooks in `~/.claude/settings.json`, installs the `janus-wt-portal` agent and `create-project` slash command, and offers to bind tmux keys (`prefix+s`, `prefix+d`, `Ctrl+n`, `Ctrl+p`).
- **Safety**: atomic writes + `.bak` backups for `settings.json`; malformed JSON moved to `.bak.malformed.<ts>`; `--yes` flag for scripted setup/uninstall; `HIVE_NO_NOTIFY=1` suppresses notifications.
- **Quick-start walkthrough** in the README, issue + PR templates, GitHub repo description + topics.

### Behavior notes

- Tmux bindings applied by `hive setup` are scoped to the live tmux server. Setup prints the `bind-key …` snippets — paste them into `~/.tmux.conf` to persist.
- Hooks in `~/.claude/settings.json` use the absolute path to the `hive` binary at the time setup was run. If you move the binary, re-run `hive setup` to refresh the paths.
- State lives in `~/.hive/` (projects, cache, todos). Uninstall does not remove this directory — `rm -rf ~/.hive/` for a full wipe.

[Unreleased]: https://github.com/emiperez95/hive/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/emiperez95/hive/releases/tag/v0.1.0
