# Session Activity Log — Recovery & Usage Metrics

## Motivation

A machine restart wipes every tmux session and, with it, every running Claude
conversation. The conversations themselves survive on disk (JSONL files in
`~/.claude*/projects/`), but two things are lost:

1. **Which** conversations were open, grouped into which tmux sessions/windows.
2. The **titles** Claude assigned them — these live only on the tmux pane title
   while running (mirrored into the window name by `sync_window_name_for_pane`).
   They are *not* persisted in the JSONL (verified: no `summary`/title lines).

So to show "here's what you had open, by title" after a reboot, hive must
**snapshot that state while sessions are alive**. Reading JSONL after the fact
recovers conversations but neither titles nor the open-set.

## Core insight: one instrumentation layer, two projections

Recovery and usage metrics are not two features — they are two **views over the
same event stream**. We instrument session *actions* once, and each event is
written two ways:

```
                        ┌─ append → activity.jsonl    (immutable history → metrics)
  event happens ────────┤
   record(event)        └─ upsert → open-windows.json  (mutable current state → recovery)
```

- The **append doc never forgets** → time-per-session, counts, history.
- The **snapshot doc always reflects now** → what to offer on startup recovery.

Same events, different retention. Build the snapshot first; metrics later is
just "add the second sink + the tmux focus hooks" — no Phase-1 code is thrown
away.

### Why unreliable closes are fine for *both* views

A crash produces **no** close event. That gap is harmless in both readings:

- Recovery: "opened, never closed, not live in tmux" = exactly a recovery candidate.
- Metrics: "opened, never closed" = cap the interval at the last known event.

So we never need close events to be *reliable* — only useful when present.

## Event vocabulary

| Event           | Source                                   | Carries                                              |
|-----------------|------------------------------------------|------------------------------------------------------|
| `window_open`   | first Claude hook for a (session_id,pane)| session, window idx/name, cwd, sid, config_dir       |
| `window_seen`   | subsequent Claude hooks                  | sid, latest title, ts (snapshot-only; not appended)  |
| `window_close`  | `SessionEnd` hook / hive-initiated kill  | session, sid                                         |
| `window_freeze` | hive freeze (`Z`)                        | session, sid                                         |
| `window_thaw`   | hive thaw                                | session, sid                                         |
| `window_skip` / `window_unskip` | hive skip toggle         | session                                              |
| `session_kill`  | hive kill-session                        | session                                              |
| `focus`         | tmux `client-session-changed` / `after-select-window` | session, window, client            |
| `attach` / `detach` | tmux `client-attached` / `client-detached`        | client                              |
| `focus_in` / `focus_out` | tmux `client-focus-in` / `client-focus-out` | client (terminal OS focus)             |

`window_seen` only bumps the snapshot (latest title + `last_seen`); it is **not**
appended to the log, to keep the log at human-paced lifecycle/focus events only.

## File schemas

### `~/.hive/cache/open-windows.json` — mutable snapshot (recovery)

Keyed by `claude_session_id`. Deliberately mirrors `FrozenEntry` so restore can
reuse the same code path.

```jsonc
{
  "windows": {
    "<claude_session_id>": {
      "session_name": "🐝 hive",
      "window_name": "Plan session activity log",  // latest title
      "window_index": "1",
      "cwd": "/Users/.../hive",
      "claude_session_id": "50dcbb18-...",
      "claude_config_dir": null,                    // auth profile, if any
      "first_seen": "2026-06-30T14:00:00Z",
      "last_seen": "2026-06-30T15:22:00Z"
    }
  }
}
```

- Upserted on every Claude hook (`window_open` / `window_seen`).
- Entry **removed** on clean close, freeze (it moves to `frozen.json`), or kill.
- Pruned by `last_seen` age (default 7 days) so a crashed entry doesn't linger
  forever.
- A frozen window lives in `frozen.json`, not here — no double-listing.

### `~/.hive/cache/activity.jsonl` — append-only log (metrics)

One compact JSON object per line, never rewritten:

```jsonl
{"ts":"2026-06-30T14:00:00Z","event":"window_open","session":"🐝 hive","window":"1","title":"","cwd":"/.../hive","sid":"50dcbb18-...","cfg":null}
{"ts":"2026-06-30T14:01:12Z","event":"focus","session":"🐝 hive","window":"1","client":"/dev/ttys003"}
{"ts":"2026-06-30T14:40:05Z","event":"focus","session":"🌳 Clear Session","window":"2","client":"/dev/ttys003"}
{"ts":"2026-06-30T15:30:00Z","event":"detach","client":"/dev/ttys003"}
{"ts":"2026-06-30T15:31:00Z","event":"window_freeze","session":"🐝 hive","sid":"50dcbb18-..."}
```

- **Concurrency**: lines are well under `PIPE_BUF` (4096B), so `O_APPEND` writes
  from concurrent hook processes are atomic on POSIX — no locking needed (same
  spirit as the existing `state.json` atomic-rename approach).
- **Rotation**: optionally roll to `activity-YYYY-MM.jsonl` to bound a single
  file; trivial at human event rates, so deferred.

## `record()` and call sites

New module `src/common/activity.rs`:

```rust
pub enum ActivityEvent { WindowOpen{..}, WindowSeen{..}, WindowClose{..},
                         Freeze{..}, Thaw{..}, Skip{..}, Unskip{..},
                         Kill{..}, Focus{..}, Attach{..}, Detach{..},
                         FocusIn{..}, FocusOut{..} }

pub fn record(ev: ActivityEvent);   // fans out to the two sinks
```

| Call site                                   | Event(s)                       | Phase |
|---------------------------------------------|--------------------------------|-------|
| `cli/hook.rs` (after `sync_window_name_for_pane`) | `window_open` / `window_seen` | 1 |
| `common/frozen.rs::freeze_window`           | `window_freeze` (remove from snapshot) | 1 |
| `common/frozen.rs::thaw_window`             | `window_thaw`                  | 2 |
| web `/api/kill-session` (+ any TUI kill)    | `session_kill` (remove from snapshot)  | 1 |
| skip toggle (TUI `S`, web flag)             | `window_skip` / `window_unskip`| 3 |
| `cli/hook.rs` new `SessionEnd` arm          | `window_close` (remove from snapshot)  | 3 |
| new `hive focus-event` cmd (tmux hooks)     | `focus`/`attach`/`detach`/`focus_in`/`focus_out` | 3 |

In Phase 1 only the **snapshot sink** is wired; the append sink + tmux focus
hooks land in Phase 3. The `record()` API is the rail that both ride.

## Recovery flow (startup)

On `hive` (default TUI) and `hive start`:

1. Load `open-windows.json`, reconcile against live tmux (`get_tmux_sessions`).
   A logged window whose session+window isn't currently live is a **ghost**.
2. **If no live sessions exist at all** (post-reboot), auto-surface the ghost
   list — *all* of them, newest `last_seen` first — as a `🕘 Last session` group,
   mirroring the existing `💤` frozen group in the search picker.
3. **Phase 1**: read-only — show title · project/cwd · "2h ago".
4. **Phase 2**: Enter on a ghost restores it. Extract the restore half of
   `thaw_window` into a shared `restore_window(entry)` (recreate tmux session +
   `claude --resume <sid>`, fall back to `claude -c`) that both frozen-thaw and
   recovery call. Restored/again-live ghosts drop out of the list.

## Metrics (Phase 3)

Fold `activity.jsonl`:

- **Sessions opened / day**, freeze & skip counts → direct from lifecycle events.
- **Active session right now** → tmux's attached-client active session/window.
- **Time per session** → sum of deltas between consecutive `focus` events,
  closed off by `detach` / `focus_out`. `client-focus-out` even catches alt-tab
  away from the terminal, shrinking the "idle while focused" blind spot.

### Honest gaps

- **Idle-but-staring** (terminal focused, not typing) is still counted; refine
  later by overlaying Claude hook activity on the focus intervals.
- **Hard sleep / crash** leaves the last interval open — cap at last known event.
- **Multiple clients** (`hive spread` attaches several at once) → two sessions
  "focused" simultaneously. Decision: track **per-client and sum** (reflects
  watching two sessions side by side).

This corrects an earlier assumption that the no-daemon design made active-time
"fuzzy": tmux fires focus events *for* us, so focus-time is accurately
measurable with **no daemon** — the tmux-native twin of hive's Claude hooks.

## Enablers

- **`SessionEnd` (and optionally `SessionStart`) Claude hooks** — not registered
  today. One arm in `cli/hook.rs` + a line in `cli/setup.rs`. Gives clean close
  signals (Phase 3).
- **tmux focus hooks** — registered by `hive setup` (verified available on tmux
  3.5a: `client-session-changed`, `after-select-window`, `client-attached`,
  `client-detached`, `client-focus-in`, `client-focus-out`), each pointing at
  `hive focus-event …`.

## Phasing

| Phase | Deliverable | Touches |
|-------|-------------|---------|
| **1** | Recovery snapshot + read-only `🕘 Last session` list on empty startup | `common/activity.rs` (snapshot sink), `cli/hook.rs`, `frozen.rs` (freeze removal), kill paths, TUI/web list group |
| **2** | One-tap restore (recreate session + `claude --resume`) | extract `restore_window`, wire Enter / web tap |
| **3** | Append log + tmux focus hooks + `SessionEnd` + metrics view | `record()` append sink, `hive focus-event`, `cli/setup.rs` hooks, `hive stats` / web metrics |

Phase 1 is the only part needed for the original problem (see what was open after
a reboot). 2 and 3 are additive and reuse Phase-1 rails.

## Status — Phase 1 (TUI) shipped

Built and installed:

- `common/activity.rs` — `OpenWindowsState` snapshot (`open-windows.json`), `record_window_seen`,
  `remove_window`, `remove_windows_for_session`, 7-day prune, atomic write, unit tests.
- `cli/hook.rs` — every hook fire upserts the window (queries the pane post-title-sync; auth
  profile from the hook's own `CLAUDE_CONFIG_DIR`). Verified end-to-end against a live pane.
- Removal on `freeze_window` (moves to `frozen.json`) and inside `kill_tmux_session` (covers
  web kill + `wt delete`).
- TUI: a `🕘 … [last session]` group in the picker + a `🕘 N from last session` header count,
  shown **only when no sessions are live** (conservative scope — avoids clutter during normal
  use, matches the "show on empty startup" ask). The picker **auto-opens** on an empty first
  refresh (the reboot case). `Del` discards a row.

Deferred:

- **Restore is read-only for now** — `Enter` on a recovery row shows the manual
  `cd <cwd> && claude --resume <sid>` command rather than running it. One-tap restore is Phase 2
  (extract `restore_window` from `thaw_window`).
- **Web dashboard** recovery section intentionally dropped — a machine restart is a
  be-at-the-machine event, so recovery lives only in the TUI. (Usage *metrics* do get a web
  view; see Phase 3a below.)

## Status — Phase 3a (append log + lifecycle + stats) shipped

Built and installed:

- `common/activity.rs` — the append-only `activity.jsonl` sink (`ActivityEntry`, `append_event`,
  `log_window_close/freeze/thaw`, `log_session_kill`, `load_activity_entries`) plus
  `compute_stats(days) → StatsSummary` shared by CLI and web.
- Lifecycle wiring: `window_open` (on first-seen sid inside `record_window_seen`),
  `window_close` (new `SessionEnd` branch in `cli/hook.rs`), `window_freeze`/`window_thaw`
  (`frozen.rs`), `session_kill` (`kill_tmux_session`). `window_seen` heartbeats are NOT
  appended (snapshot-only) to keep the log at human-paced events.
- `SessionEnd` registered by `hive setup` (added to `hook_events` + `is_hive_hook_command`).
  Requires re-running `hive setup` and restarting Claude sessions to take effect.
- `hive stats [--days N]` — counts, per-day open bar chart, recent events.
- Web: `GET /api/stats?days=N` + a `Usage` bottom-sheet (insights icon in the list header)
  rendering the same summary. Verified end-to-end against a synthetic log.

Skip/unskip logging was deferred (low value, scattered toggle sites).

## Status — Phase 3b partial (focus from hive's own commands) shipped

Instead of tmux hooks (deferred), we started with the switches hive *already controls* — no
new command, no setup changes, no tmux-hook quoting:

- `EVENT_FOCUS` (`"focus"`) + `log_focus(session, window)` in `activity.rs`.
- Session-level focus is logged in **`tmux::switch_to_session`** — the choke point every
  hive-initiated session switch routes through: CLI cycle-next/prev, cycle-free, `connect`, and
  all TUI switches (list select, picker, hint-jump, thaw). One central log, no per-call-site
  duplication.
- Window-level focus is logged by `run_window_cycle` (`window-next/prev` → session + the
  now-active window, queried after the switch). Window jumps that also change session
  (cycle-free, hint) log at session granularity via `switch_to_session`.
- `switches` count added to `StatsSummary`, shown in `hive stats` and the web Usage grid;
  events show as "switch" in the Recent feed. Verified end-to-end (window-next/prev round trip).

This partial approach was **superseded by the tmux focus hooks** (below), which capture native
switching too — the explicit `switch_to_session` / `run_window_cycle` logs were then removed to
avoid double-logging.

**Web viewing is tracked as a SEPARATE stream** (`web_view` / `web_blur`), deliberately kept
out of the tmux `focus` stream — tapping a session in the web app views it *remotely*, it
doesn't run `switch-client`, and you could be web-viewing session A on your phone while the
Mac's tmux is focused on B, so folding them together would corrupt any time-per-session figure.

- `web_view {session|null}` — the browser is now viewing a session (or the list). Fired on page
  load, tab-becomes-visible, and when the viewed session changes (`openConversation`/`showList`).
- `web_blur` — tab hidden or page closing; bounds the viewing interval so time stops counting
  when the browser loses focus or is closed. Sent via `navigator.sendBeacon` so it survives
  unload (a normal `fetch` gets killed as the page tears down).
- Endpoint: `POST /api/web-event {event, session?}` → `log_web_view`/`log_web_blur`.
- The web Usage sheet has **two tabs** (`switchStatsTab`): **Sessions** (tmux lifecycle counts,
  opens-by-day, switches, and the non-web Recent feed) and **Web** (views, distinct sessions
  seen, and the web-only Recent feed). `/api/stats` splits the feed into `recent_sessions` /
  `recent_web` and adds `web_views` / `web_sessions`. CLI `hive stats` shows both as separate
  "Recent" / "Recent (web viewing)" sections. `switches` (tmux focus) stays web-free.
- Known: mobile fires `visibilitychange` often (lock, app-switch), so the web stream is chattier
  than the tmux one — fine for bounding intervals, just noisier in the Recent feed.

## Status — Phase 3b full (tmux focus hooks + time) shipped

Driven by the coverage audit (see below). Three parts:

**1. tmux focus hooks — full focus coverage.** `hive setup` writes `~/.hive/focus-hooks.tmux`,
sources it into the running server, **and idempotently adds `source-file …/focus-hooks.tmux` to
`~/.tmux.conf`** so the hooks re-establish on every tmux/machine restart — no manual step, no
daemon (`hive uninstall` unsets the live hooks, strips the `~/.tmux.conf` line, and deletes the
file). Six hooks map to a hidden `hive event <kind> [session] [window]` command:

| tmux hook | → event | catches |
|---|---|---|
| `client-session-changed` | `focus` | any session switch (hive **and** native `prefix s`, click, external `switch-client`) |
| `after-select-window` | `focus` | any window switch (hive `window-next/prev` **and** native `prefix n`/`p`) |
| `client-attached` / `client-focus-in` | `focus` | re-attach / terminal regained OS focus |
| `client-detached` | `blur` | **iTerm closed / `prefix+d`** — stop counting |
| `client-focus-out` | `blur` | **alt-tabbed away from the terminal** — stop counting |

Quoting was the hard part: the run-shell command carries single-quoted `#{format}` args inside
a double-quoted shell string; the brace form `set-hook -g H { run-shell -b "…" }` in the sourced
file avoids escaping. Verified: emoji+space session names arrive as one arg; hooks fire and log.
The old explicit `switch_to_session`/`run_window_cycle` focus logs were **removed** (the hooks
supersede them). `focus-events on` is set so `client-focus-in/out` work.

**2. Cheap internal control actions now logged.** `skip`/`unskip` (TUI `toggle_skip` + web flag),
`spread`/`collapse` (`run_spread`/`run_collapse`). Mute/auto-approve/favorite left out (not
session activity). `wt new` still captured indirectly via `window_open`.

**3. Time-per-session, crash/sleep-robust.** `accumulate_time` pairs each `focus`/`web_view`
with the next event (closed by `blur`/`web_blur`), summing per session. Two layers of gap
handling:

- **Machine sleep is subtracted precisely** from each interval, read from the OS at report time
  — `common/machine.rs::sleep_intervals` parses `pmset -g log` (macOS; no-op stub elsewhere).
  A `Sleep` domain line opens an asleep span, the next `Wake`/`DarkWake` closes it (exact domain
  match on the tab-delimited field, so `Wake Requests`/`WakeTime` lookalikes are ignored). This
  nails the closed-lid-overnight case exactly — no daemon, no sleep hook.
- **`INTERVAL_CAP_SECS` (2h)** is now just a backstop for the residue sleep-subtraction can't
  see: a hard shutdown/crash (not logged as sleep) or an awake-but-idle-focused stretch. Raised
  from 30 min → 2h because sleep (the main huge-gap cause) is now removed precisely.

Surfaced in `hive stats` ("Active time — Xh Ym total" + "(Xh Ym of machine sleep subtracted)" +
per-session) and both web Usage tabs (bars + a subtracted-sleep note). `StatsSummary.slept_secs`
carries the window's total sleep. Unit-tested (aggregation caps, sleep subtraction, pmset parse).

## Coverage audit — external desktop events

| Event | Captured now? | How |
|---|---|---|
| Close iTerm / `prefix+d` | ✅ | `client-detached` → `blur` |
| Alt-tab away from terminal | ✅ | `client-focus-out` → `blur` |
| Native tmux switch (`prefix s`/`n`, click) | ✅ | `client-session-changed` / `after-select-window` |
| Lid close / sleep | ✅ subtracted | read from `pmset -g log` at report time; exact sleep removed from intervals |
| PC restart / hard shutdown | ⚠️ handled at read | no event; recovery snapshot preserves the open set; 2h cap bounds the last interval |
| Hard-kill Claude (SIGKILL, no `SessionEnd`) | ⚠️ | window lingers in snapshot until prune/reconcile; harmless |
| Web network drop (no `pagehide`) | ⚠️ | `web_view` left open; 2h cap bounds it at read |

Sleep is now handled **precisely** by reading the OS power log (no daemon). The remaining ⚠️ are
true **absence-of-event** cases (hard shutdown/crash/network-drop log nothing) — bounded by the
2h cap. A launchd sleep/wake agent is therefore unnecessary. Cost: `pmset -g log` takes ~1.2s,
run once per report (on-demand `hive stats` / `/api/stats`), never on the refresh path.
