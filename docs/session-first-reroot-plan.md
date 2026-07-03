Verified against the real crate (`SessionStatus`/`SessionState` at `src/ipc/messages.rs:74/100`, `persistence::cache_dir()` `pub(crate)`, `frozen.rs` atomic-write idiom, `debug.rs` `OnceLock` flag idiom, `chrono::Utc` available). Here is the final plan.

---

# TDD Implementation Plan — Strangler-Fig Re-rooting to `ClaudeSession` (final)

**Executive summary.** We re-root hive's model on the persisted Claude conversation (`ClaudeSession`, keyed by the `<uuid>.jsonl` session id) without ever touching the always-on `hive hook` writer. Everything is built as a **read-only shadow**: a new `src/common/registry.rs` left-joins the existing `state.json` (`HookState`, live status), a bounded disk scan of `~/.claude*/projects/*.jsonl` (existence = source of truth for Closed/remote sessions), and a new overlay sidecar `sessions.json` (hive's note/pinned/archived/parent). The shadow is gated behind `HIVE_SESSION_REGISTRY` and projected into the *existing* `SessionInfo`/`SessionView` shapes, so it's a pure A/B over the same live state. **Two critic-driven simplifications make the tool unbreakable:** (1) we never invert `cleanup_stale_sessions` — `state.json` stays a small bounded live-status cache and Closed sessions come purely from the disk scan, so the hot write path is byte-identical and can't grow unbounded; (2) we never add a field to `SessionState`, so the currently-installed hook binary and the new binary read each other's `state.json` trivially (no schema change at all). The only mutation seam introduced is a single `act_on(session_id, Action)` funnel (I4/I5) that every send/switch/kill/resume routes through before the old name-based paths are deleted at cutover. Rollback at any point is `unset HIVE_SESSION_REGISTRY` or reinstall the `pre-reroot-known-good` tag.

Build gate after **every** increment:
```
cargo build && cargo clippy -- -D warnings && cargo test && cargo fmt --check
```
User verification from Inc 4 onward: `cargo install --path . --root ~/.local` then `HIVE_SESSION_REGISTRY=1 hive`.

**Before starting:** `git tag pre-reroot-known-good` on the current known-good commit; branch off `main`.

**Key design decisions folded in from review:**
- **`Lifecycle` is `{ Live, Closed }` only.** `Pending` is dropped (unreachable — no id exists before the first jsonl write). **Frozen is a facet, not a peer variant**: a frozen session is `Closed` with `frozen: Some(FrozenInfo{ pinned: true, .. })`. This makes "Frozen ⊂ Closed" true *at the type level* — every "known-but-not-running-here / == remote" check (`is_closed_like()`) automatically includes frozen sessions.
- **The hook writer is never modified.** `parent` is resolved at *read* time and cached only into the new `sessions.json` sidecar. There is no additive field on `SessionState`, so the "blast radius" writer increment is deleted entirely.
- **`cleanup_stale_sessions(600)` stays.** `state.json` remains bounded; Closed history is derived from disk, never accumulated in the hot-path file.
- **One liveness rule, pinned once:** a session is `Live` iff a live `TmuxPlacement` is supplied for its id (from detected instances / open-windows). Otherwise `Closed`. `last_activity` recency only governs *whether a Closed session is surfaced* (bounding), never Live-vs-Closed.
- **The Closed set is bounded and the disk scan is cached** (mtime/interval), never a full FS walk every 1-1.5 s refresh.

---

## Increment 0 — New module + types + pure unit tests (ZERO installed surface)

**(a) Goal.** Create `src/common/registry.rs` with the full `ClaudeSession` model, its serde behavior, and lifecycle transitions. No reads of `state.json`, no wiring, no CLI. Register `pub mod registry;` in `src/common/mod.rs`.

**(b) Test-first.** In-module `#[cfg(test)] mod tests` (mirrors `frozen.rs`/`instances.rs` style):
- `test_claude_session_id_roundtrip` — transparent newtype: `ClaudeSessionId::from("uuid")` → `as_str()`/`to_string()` == the UUID; serde field roundtrips as a bare JSON string.
- `test_lifecycle_serde_both_variants` — `Live`/`Closed` roundtrip as bare strings.
- `test_lifecycle_helpers` — `Live.is_actionable_here()`, `Closed.is_closed_like()`; the inverse are false.
- `test_claude_session_serde_roundtrip` — full session with `Some(placement)`, `Some(parent)`, `Some(frozen)` roundtrips.
- `test_claude_session_minimal_serde` — `placement/parent/frozen == None` roundtrips (Closed/remote shape; I3).
- `test_mark_closed_keeps_record` — `mark_closed()` sets `Closed`, clears `placement`, **retains** `id`/`cwd`/`last_activity` (pins the keep-record contract).
- `test_mark_live_sets_placement` — `mark_live(p)` → `Live`, `placement == Some(p)`, `frozen == None`.
- `test_freeze_is_closed_facet` — `freeze("note")` → `lifecycle == Closed`, `is_frozen()`, `frozen.pinned`, and `is_closed_like()` true (Frozen ⊂ Closed at the type level).
- `test_thaw_restores_live` — `thaw()` → `Live`, `frozen == None`.
- `test_sidecar_default_empty` / `test_sidecar_serde_roundtrip` — `SessionSidecar::default()` → `{"sessions":{}}`; populated map roundtrips.
- `test_sidecar_entry_backward_compat` — raw legacy-shaped entry JSON with only `note` (missing `pinned`/`archived`/`parent`/`frozen_at`) deserializes with all missing fields at `Default` (the `#[serde(default)]` proof).

**(c) Implementation.** See **First increment — ready to run** below for the exact code.

**(d) Backward-compat & flag state.** No flag. Old binary 100% unaffected (file unreferenced). `SessionStatusState` reuses the existing `SessionStatus` enum — **no new variant is added to `SessionStatus`** (an externally-tagged enum; a new variant would make old binaries fail to parse and `HookState::load()` drops the entire map to `Default`).

**(e) Rollback.** Delete the file + the `mod` line.

---

## Increment 1 — Disk-scan existence + `state.json` coexistence, single liveness rule (READ-ONLY, pure)

**(a) Goal.** Build the model read-only from (1) `HookState` for status, (2) a bounded jsonl disk scan for existence, (3) a `live_placements: &HashMap<String, TmuxPlacement>` map that is the *sole* Live-vs-Closed discriminator. Seams S1, S8; invariants I1, I3.

**(b) Test-first** (pure, in-memory — no real `~/.hive`, no tmux):
- `test_live_iff_placement_present` — **pins the single rule.** A hook entry whose id IS in `live_placements` → `Live` with that placement; the SAME entry with an empty `live_placements` → `Closed`, `placement == None`.
- `test_disk_only_session_is_closed` — a disk-scanned id absent from `HookState` and `live_placements` → `Closed`, `status == None` (I3: closed == remote).
- `test_hook_and_disk_left_join` — id in both → one merged session (no dup), status from hook, existence from disk.
- `test_old_format_state_json_deserializes` — the **safe-direction coexistence proof.** Embed the exact current schema literal `r#"{"sessions":{"abc-123":{"session_id":"abc-123","cwd":"/x","status":"Working","needs_attention":false,"last_activity":"2026-07-01T00:00:00Z","tmux_pane":"%1"}}}"#`, `from_str::<HookState>()`, feed to `from_shadow`, assert the `ClaudeSession` is correct.
- `test_new_state_json_still_reads_under_hookstate` — the **dangerous-direction proof.** Round-trip a `HookState` through `to_string` then `from_str` and assert all sessions survive; assert (grep-style doc-test or explicit deserialize of an over-populated JSON with an extra unknown key) that `HookState`/`SessionState`/`SessionStatus` carry **no `deny_unknown_fields`** — the guard that protects the old binary's all-or-nothing `load()`.
- `test_scan_returns_session_ids` — temp-dir fixture with `<uuid1>.jsonl`, `<uuid2>.jsonl`, and a non-`.jsonl` file → only the two UUIDs come back (path-injected scan, temp-dir isolation).

**(c) Implementation.**
- `jsonl.rs`: add path-injectable global enumerators (none exists today):
  ```rust
  pub fn scan_session_ids_in(projects_dirs: &[PathBuf]) -> Vec<String> // *.jsonl basenames
  pub fn scan_all_session_ids() -> Vec<String>                          // over ~/.claude*/projects/*
  ```
- `registry.rs`:
  ```rust
  pub struct SessionRegistry { pub sessions: HashMap<String, ClaudeSession> }
  impl SessionRegistry {
      pub fn from_shadow(
          hook: &crate::ipc::messages::HookState,
          disk_ids: &[String],
          live_placements: &HashMap<String, TmuxPlacement>,
          sidecar: &SessionSidecar,
      ) -> Self { /* left-join; Live iff id in live_placements, else Closed */ }
  }
  ```
  `from_shadow` is I/O-free (all inputs pre-loaded) → unit-testable without tmux/sysinfo. `sidecar`/`live_placements` are threaded now; the placement map stays empty until Inc 4/6 feed it.

**(d) Backward-compat & flag state.** No view wired, no flag consumed. Old binary keeps writing `state.json`; new code only reads it. **No `SessionState` field added** — `state.json` shape is unchanged, so both directions of the coexistence test hold trivially and forever.

**(e) Rollback.** Revert `registry.rs` + `jsonl.rs` scan fns.

---

## Increment 2 — Sidecar persistence (`sessions.json`) roundtrip + parent write-back

**(a) Goal.** Persist/load `SessionSidecar` to a NEW file `~/.hive/cache/sessions.json`, cloning the `FrozenState` idiom (`frozen.rs:123-126`). Support opportunistic parent caching write-back (a new-file write only — never the hook path).

**(b) Test-first:**
- `test_sidecar_save_load_roundtrip` — path-injected `save_to`/`load_from` to a temp dir (`std::env::temp_dir().join(format!("hive-reg-{}", process::id()))`, `remove_dir_all` after — the `test_hook_state_save_load_roundtrip` pattern).
- `test_sidecar_load_missing_is_default` — non-existent path → `default()`.
- `test_sidecar_load_corrupt_is_default` — garbage → `default()`, never panics.
- `test_from_shadow_applies_overlay` — sidecar `{"abc-123":{note:"wip",pinned:true,parent:"hive/CSD-1"}}` overlays note/pinned/parent onto a disk-only `abc-123`.
- `test_sidecar_file_path_basename` — `SessionSidecar::file_path()` ends with `sessions.json` and equals neither `state.json` nor `frozen.json` (pins the derived-file contract even though disk IO stays a thin wrapper).

**(c) Implementation** (copy `frozen.rs:74-140`):
```rust
impl SessionSidecar {
    fn file_path() -> Option<PathBuf> {
        crate::common::persistence::cache_dir().map(|p| p.join("sessions.json"))
    }
    pub fn load() -> Self { /* file_path → from_str → default on any error */ }
    pub fn load_from(path: &Path) -> Self { /* injectable */ }
    pub fn save(&self) -> anyhow::Result<()> { /* to_string_pretty → .json.tmp → rename */ }
    pub fn save_to(&self, path: &Path) -> anyhow::Result<()> { /* injectable */ }
}
```

**(d) Backward-compat & flag state.** `sessions.json` is brand-new and derived; `state.json`/`worktrees.json`/`frozen.json` remain authoritative. Absent file defaults cleanly. No flag consumed. (`cache_dir()` is `pub(crate)` — in-crate use is fine.)

**(e) Rollback.** Delete `sessions.json` on disk (harmless) + revert the impl.

---

## Increment 3 — Parent resolution (local-only best-effort) + Closed-set bounding + scan caching

**(a) Goal.** Replace name-based correlation (S4/S5) with logical `cwd → (project, branch)` resolution and pin the bounding/caching policy for the Closed set. Invariant I2.

**(b) Test-first** (pure — in-memory `WorktreeState` + `ProjectRegistry`):
- `test_resolve_parent_exact_worktree_prefix` — cwd under a worktree path → that worktree's `make_key`.
- `test_resolve_parent_falls_back_to_project_root` — cwd under a project root (no worktree match) → project key.
- `test_resolve_longest_prefix_wins` — nested candidates → deeper match.
- `test_resolve_component_boundary_no_false_prefix` — cwd `/home/u/hivefoo` must NOT match candidate `/home/u/hive` (component-wise, not raw `starts_with`).
- `test_resolve_tilde_expansion` — `project_root = "~/hive"` matches an absolute cwd after `expand_tilde`.
- `test_resolve_foreign_cwd_is_none` — a cwd with no local prefix → `None` (must NOT false-match a remote/foreign path).
- `test_persisted_parent_not_reresolved` — a session whose sidecar overlay already has `parent` is NOT re-resolved by cwd (persisted parent is authoritative; local resolver is fallback only).
- `test_bounding_predicate` — `should_surface_closed(session, now, cfg)` returns true iff `!archived && (last_activity within N days OR parent.is_some() OR frozen.pinned)`; assert each arm.
- `test_scan_cache_reuses_within_interval` / `test_scan_cache_refreshes_after_interval` — a `ScanCache { ids, scanned_at }` returns cached ids inside the interval and rescans after (inject a clock).

**(c) Implementation** in `registry.rs`:
```rust
pub fn resolve_parent(
    cwd: &str,
    worktrees: &crate::common::worktree::WorktreeState,
    projects: &crate::common::projects::ProjectRegistry,
) -> Option<String> // longest component-wise prefix → make_key(project,branch) or project key
```
Uses `expand_tilde` (`projects.rs:138`) + `WorktreeState::make_key` (`worktree.rs:37`), normalizes via `Path::components()`. **Resolution mechanism is host-local best-effort**; a persisted sidecar `parent` short-circuits it. Add `should_surface_closed(...)` and a cached-scan wrapper (interval- or mtime-gated). Do NOT touch `find_worktree_by_session_name`/`find_by_session_name` yet.

**(d) Backward-compat & flag state.** Pure additive fns, unused by production until Inc 4. When a parent is resolved for a live session, cache it into `sessions.json` (new file only). No flag consumed.

**(e) Rollback.** Delete the fns + tests.

---

## Increment 4 — Read-only shadow behind `HIVE_SESSION_REGISTRY`, via a PURE status mapper + shared enrichment

**(a) Goal.** Flag-gate an alternate view at the two consumer seams. Crucially, the registry re-roots **only identity + status + placement**; CPU/mem/ports/chrome/todos/flags stay in the *existing shared enrichment code*, consumed identically by both paths (no second enrichment pipeline to drift). Seams S9, S10; invariants I1, I5.

**(b) Test-first:**
- Flag reader: `parse_registry_flag(var: Option<&str>) -> bool` using the `HIVE_NO_NOTIFY` idiom (`is_some_and(|v| !v.is_empty() && v != "0")`), backed by `OnceLock<bool>` like `debug.rs:9`. Tests `test_flag_default_off`/`_env_on`/`_zero_off`/`_empty_off` drive the pure parser with `None`/`Some("1")`/`Some("0")`/`Some("")`.
- **Pure status mapper** (the behavior worth pinning — no sysinfo/ports):
  ```rust
  pub fn map_instance_status(entry: &ClaudeSession, instance: Option<&ClaudeInstance>)
      -> (crate::common::types::ClaudeStatus, Option<TmuxPlacement>)
  ```
  - `test_map_status_live_from_instance` — Live entry + instance → mapped `ClaudeStatus` (via `convert_hook_status`) + placement.
  - `test_map_status_closed_no_instance` — Closed entry, no instance → a closed/remote `ClaudeStatus` + `placement == None`.
- **Pure overlay guard** (no disk fixture):
  ```rust
  pub fn apply_workflow_overlay(base: ClaudeStatus, summary: Option<String>, cwd_shared: bool) -> ClaudeStatus
  ```
  - `test_workflow_overlay_only_when_idle` — overlay applied iff `base == Waiting && !cwd_shared && summary.is_some()`; every other combination returns `base` unchanged (reproduces the 3-site guard; guards against the b61220e "borrow sibling status" regression).
- Field-parity: `test_projection_field_parity` — on a fixture (registry + instances + a canned enrichment struct), assert the projected `SessionView` carries `session_id` on each window (I5) and that identity/status/placement fields equal what the legacy mapper produces for the same input.
- CLI smoke (`tests/cli_smoke.rs`, temp `$HOME`): `test_registry_flag_does_not_crash` — run a read-only command with `.env("HIVE_SESSION_REGISTRY","1")`, assert exit 0.

**(c) Implementation.**
- `is_registry_shadow_enabled()` in `registry.rs`.
- Restructure so enrichment (sysinfo/ports/chrome/todos/flags) is a shared step applied *after* identity/placement/status assembly. Branch in `gather_sessions` (`tui/app.rs`) and `gather_session_data` (`serve/server.rs`): `if is_registry_shadow_enabled() { /* registry supplies identity+status+placement via map_instance_status; reuse existing enrichment */ } else { /* byte-identical existing code */ }`. The registry supplies Closed/Frozen rows with no matching instance. Feed `detect_claude_instances` + `HookIndex` (S3) placements into `from_shadow`'s `live_placements`.
- Enumerate every `SessionInfo`/`SessionView` field and its source in the increment's dev notes; the field-parity test enforces it.

**(d) Backward-compat & flag state.** Flag **OFF by default** → zero behavior change, all existing tests pass unchanged. Flag ON → new *view only*, still no writes. Auto-approve masking stays where it lives (web `build_window_view`; TUI `apply_refresh`) — the projection must not double-mask; the parity test catches divergence, treated as a cutover blocker.

**(e) Rollback.** `unset HIVE_SESSION_REGISTRY` or reinstall the pre-tag binary; code rollback = revert the two branch points.

---

## Increment 5 — The single action seam `act_on` (I4 second half) + reopen == thaw

**(a) Goal.** Introduce ONE mutation funnel so that *every* "act on it" — Send, Switch, Kill, Resume — resolves `ClaudeSession.placement → local tmux target` in a single function. This is where the distributed tier later adds *one* forwarding branch instead of a hundred edits. Reopen/thaw == `ensure_tmux_session + claude --resume <id>` becomes the `Resume` arm. Seams S1; invariants I3, I4. **No writer/behavior change to `state.json`** — `cleanup_stale_sessions(600)` stays exactly as is; Closed is derived read-side.

**(b) Test-first** (pure — resolve against an in-memory registry, no real tmux):
- `test_act_on_resolves_by_session_id` — `act_on(&reg, id, Action::Switch)` resolves the target through `placement`, NOT a tmux name passed in; returns a resolved `TmuxTarget`/session_name.
- `test_act_on_send_resolves_placement` / `_kill_resolves_placement` — Send/Kill resolve the same way.
- `test_reopen_builds_resume_startup` — `reopen_startup_cmd(&ClaudeSessionId) -> String` == `"claude --resume <uuid>"` (matches `thaw_window`).
- `test_reopen_uses_recorded_cwd` — Resume targets `ClaudeSession.cwd`.
- `test_act_on_closed_without_placement_uses_reopen` — a Closed session (placement None) + `Switch`/`Resume` routes to `ensure_tmux_session(..., "claude --resume <id>")` (I4: single seam handles both live-switch and reopen).

**(c) Implementation** in `registry.rs`:
```rust
pub enum Action { Send(String), Switch, Kill, Resume }
pub fn act_on(reg: &SessionRegistry, id: &ClaudeSessionId, action: Action) -> anyhow::Result<ActResult>
```
Resolves `placement` → local tmux ops today (`tmux send-keys`/`switch-client`/`kill`, or `ensure_tmux_session` (`projects.rs:260`) for Resume/reopen — the SAME primitive `thaw_window` uses, I4). One documented seam; distributed forwarding is later a single branch keyed on a per-session locality flag. Route the flagged TUI switch/send/kill and the web POST handlers through `act_on` (accept `session_id`; accept name transitionally). Migrating the web mutation endpoints (`/api/send`, `/api/toggle-flag`, `/api/todos`, `/api/kill-session`, `/api/connect`) to accept `session_id` as primary key satisfies I5's *mutation* surface, not just reads.

**(d) Backward-compat & flag state.** All routing changes live behind the flag / accept the legacy name key transitionally. The hook writer is untouched. `state.json` still bounded by the unchanged `cleanup_stale_sessions`.

**(e) Rollback.** Flag off; `act_on` + endpoint session_id support are inert when the flag is off and name-keying still works.

---

## Increment 6 — Fold `frozen.json` + `open-windows.json` into registry reads (Frozen = Closed facet)

**(a) Goal.** Make Freeze/Thaw read through the registry, unifying `frozen.json` (S6) and `open-windows.json` (S7) into the `ClaudeSession` lifecycle. Still flag-gated.

**(b) Test-first:**
- `test_frozen_entry_maps_to_closed_facet` — `from_shadow` (now also taking `&FrozenState`) maps a session-id-keyed `FrozenEntry` to `Closed` + `frozen: Some(FrozenInfo{note,pinned:true,frozen_at})`; `is_frozen()` true, `is_closed_like()` true.
- `test_composite_key_frozen_excluded` — a `FrozenEntry` with `claude_session_id == None` (composite `session#window` key, `frozen.rs:54-59`) is **excluded** from the registry (kept only in the legacy frozen view until it ages out) and does NOT synthesize a fake/None id that could collide with a real UUID (I1).
- `test_open_windows_seed_live_placement` — an `OpenWindowsState` entry (session-id-keyed) supplies a `TmuxPlacement` (→ Live) when no live instance is currently detected (bridges detached-but-known).
- `test_frozen_satisfies_known_not_here` — a frozen session satisfies `is_closed_like()` (I3) and has `placement == None`.

**(c) Implementation.** Extend `from_shadow` to accept `&FrozenState` (`frozen.rs`) and `&OpenWindowsState` (`activity.rs:86`) — both already session-id-keyed. Map `FrozenEntry`→`Closed`+`FrozenInfo`; open-windows→`live_placements` seed. Freeze/thaw *actions* still route through existing `freeze_window`/`thaw_window` writes for now (and through `act_on` for the tmux side); registry only *reads*. Composite-key entries handled by an explicit `Option<ClaudeSessionId>` branch that excludes them.

**(d) Backward-compat & flag state.** `frozen.json`/`open-windows.json` remain authoritative write targets; registry reads only. Flag-gated.

**(e) Rollback.** Flag off; revert `from_shadow` param additions.

---

## Increment 7 — Cutover + cleanup (delete dead correlation code)

**(a) Goal.** Flip the flag default ON (after soak), make `sessions.json` authoritative for the hive overlay, and delete dead name-based correlation code. **The hook writer's `cleanup_stale_sessions(600)` is left as-is** — `state.json` stays a bounded live-status cache; Closed history lives on disk, never in the hot-path file.

**(b) Test-first:**
- `test_registry_flag_default_on` — with env unset, `is_registry_shadow_enabled()` true.
- `test_renamed_window_still_resolves` — the behavioral regression that proves deleting S4/S5 is safe: a session whose tmux window was renamed still resolves to its parent by `session_id` + cwd (NOT by tmux name). (Do **not** assert call-graph routing — behavior only.)
- Full suite green with the default ON.

**(c) Implementation.**
- Flip the flag default. Registry owns `sessions.json` (overlay) writes; freeze/thaw metadata migrates from `frozen.json` into the sidecar's `frozen` facet.
- **Delete dead correlation code:** `find_worktree_by_session_name` (`worktree.rs:185`), `ProjectRegistry::find_by_session_name`/name-scan `has_project` (`projects.rs:193,200`), `migrate_session_names`, `HookIndex.by_cwd`/`cwd_shared` fallback (S3), and the cwd-keyed `hook_sessions` map in `gather_sessions`. Remove the legacy branch in both gather functions (registry projection becomes the only path).
- Keep writing `frozen.json`/`open-windows.json` for ONE more release (downgrade safety), then retire.
- **Do NOT invert `cleanup_stale_sessions`.** There is no `transition_stale_sessions` — Closed is a read-side derivation from the disk scan + `live_placements`, which is redundant with any writer-side retention. This removes the unbounded-growth risk on the always-on writer entirely.

**(d) Backward-compat & flag state.** Post-release the flag is a no-op. During the one-release soak, `frozen.json`/`open-windows.json` are still written so a downgrade works; `state.json` shape was never changed, so downgrade is always safe.

**(e) Rollback.** Re-add the flag default OFF and the retained old code path from git history / the pre-cutover tag, or reinstall the pre-cutover binary.

---

## Serde additive strategy (concrete)

1. **Never add a variant to `SessionStatus`/`HookEvent`** for lifecycle — externally-tagged enums reject unknown variants and `HookState::load()` drops the ENTIRE map to `Default` on any parse error (verified `messages.rs`). Lifecycle lives on the NEW `ClaudeSession`/`Lifecycle` type, on a separate axis.
2. **Never add a field to `SessionState`.** `parent` is resolved at read time and cached only into the new `sessions.json`. `state.json`'s shape is therefore unchanged forever → old and new binaries read each other's `state.json` with zero risk. (This is a stronger guarantee than `#[serde(default)]` additive fields.)
3. **All new persistence goes to `sessions.json`** — never mutate `state.json`/`frozen.json`/`worktrees.json` shapes until cutover.
4. **New fields on the new sidecar structs use `#[serde(default)]`;** bools use `#[serde(default, skip_serializing_if = "std::ops::Not::not")]` (the `ProjectConfig::archived` recipe).
5. **Guard test** (`ipc/messages.rs`): assert `HookState`/`SessionState`/`SessionStatus` carry no `deny_unknown_fields` and that a `HookState` with an injected extra unknown key still deserializes with all sessions intact — this pins the true blast radius (`load()`'s all-or-nothing default) against a future contributor.

---

### First increment — ready to run

Create **`/Users/emilianoperez/Projects/00-Personal/hive/src/common/registry.rs`** and add `pub mod registry;` to `/Users/emilianoperez/Projects/00-Personal/hive/src/common/mod.rs` (alphabetically after `ports`).

```rust
// src/common/registry.rs
//! Re-rooted session model, keyed by the Claude conversation UUID (the
//! `<uuid>.jsonl` basename). Built read-only as a shadow over the existing
//! `state.json` + a disk scan + a `sessions.json` overlay sidecar. Nothing here
//! is wired into a writer or a view yet (Increment 0).

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Invariant #1: identity is the Claude session UUID, never a tmux name or path.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ClaudeSessionId(pub String);

impl ClaudeSessionId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl From<&str> for ClaudeSessionId {
    fn from(s: &str) -> Self {
        ClaudeSessionId(s.to_string())
    }
}
impl std::fmt::Display for ClaudeSessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Invariant #3: "known but not running here" (Closed) is first-class and equals
/// "remote". This axis is ORTHOGONAL to `SessionStatus` (activity) — never overload
/// `SessionStatus`. Frozen is NOT a variant here — it is a facet of Closed (see
/// `ClaudeSession.frozen`), so every "closed == remote" check includes frozen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Lifecycle {
    Live,
    Closed,
}

impl Lifecycle {
    /// True only when the conversation is running on THIS host right now.
    pub fn is_actionable_here(&self) -> bool {
        matches!(self, Lifecycle::Live)
    }
    /// True for every "known but not running here" state (== remote). Frozen
    /// sessions are Closed, so they satisfy this too.
    pub fn is_closed_like(&self) -> bool {
        matches!(self, Lifecycle::Closed)
    }
}

/// Invariant #4: "where it runs" — ephemeral, host-local, NULLABLE. None ⇒
/// closed/detached/remote.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TmuxPlacement {
    pub session_name: String,
    #[serde(default)]
    pub window_index: String,
    #[serde(default)]
    pub window_name: String,
    #[serde(default)]
    pub pane_id: Option<String>,
}

/// Activity status overlay — wraps the existing hook status enum on a separate
/// axis from `Lifecycle` (never add lifecycle variants to `SessionStatus`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionStatusState {
    pub status: crate::ipc::messages::SessionStatus,
    pub needs_attention: bool,
}

/// The hive overlay for a frozen window (== pinned+noted subset of Closed).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FrozenInfo {
    #[serde(default)]
    pub note: String,
    #[serde(default)]
    pub pinned: bool,
    /// RFC3339
    pub frozen_at: String,
}

/// The re-rooted base entity. Keyed by `ClaudeSessionId` everywhere.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClaudeSession {
    pub id: ClaudeSessionId,
    pub cwd: String,
    pub lifecycle: Lifecycle,
    #[serde(default)]
    pub status: Option<SessionStatusState>,
    /// RFC3339; feeds `Project.last_session = max(..)` and Closed-set bounding.
    #[serde(default)]
    pub last_activity: Option<String>,
    /// Invariant #4 — placement is optional.
    #[serde(default)]
    pub placement: Option<TmuxPlacement>,
    /// Invariant #2 — logical parent key = `WorktreeState::make_key("{project}","{branch}")`,
    /// NOT a path and NOT a tmux name.
    #[serde(default)]
    pub parent: Option<String>,
    /// Frozen facet: `Some` ⇒ this Closed session is a pinned/noted freeze.
    #[serde(default)]
    pub frozen: Option<FrozenInfo>,
}

impl ClaudeSession {
    pub fn is_frozen(&self) -> bool {
        self.frozen.is_some()
    }
    /// True for closed/remote/frozen — "known but not running here".
    pub fn is_known_not_here(&self) -> bool {
        self.lifecycle.is_closed_like()
    }
    pub fn mark_closed(&mut self) {
        self.lifecycle = Lifecycle::Closed;
        self.placement = None; // "where it runs" is gone; identity/cwd/history retained
    }
    pub fn mark_live(&mut self, placement: TmuxPlacement) {
        self.lifecycle = Lifecycle::Live;
        self.placement = Some(placement);
        self.frozen = None;
    }
    /// Freeze == a pinned/noted Closed session (Frozen ⊂ Closed).
    pub fn freeze(&mut self, note: &str) {
        self.lifecycle = Lifecycle::Closed;
        self.placement = None;
        self.frozen = Some(FrozenInfo {
            note: note.to_string(),
            pinned: true,
            frozen_at: chrono::Utc::now().to_rfc3339(),
        });
    }
    pub fn thaw(&mut self) {
        self.lifecycle = Lifecycle::Live;
        self.frozen = None;
    }
}

/// The persisted overlay sidecar (NEW file `~/.hive/cache/sessions.json`; does not
/// repurpose existing state). Same shape/role as `FrozenState` — keyed by UUID.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionSidecar {
    #[serde(default)]
    pub sessions: HashMap<String, SessionOverlay>,
}

/// Only hive's overlay lives here; existence/status come from disk + state.json.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionOverlay {
    #[serde(default)]
    pub note: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pinned: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub archived: bool,
    /// logical parent key (project/branch), resolved once and cached here
    #[serde(default)]
    pub parent: Option<String>,
    #[serde(default)]
    pub frozen_at: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::messages::SessionStatus;

    fn sample(id: &str) -> ClaudeSession {
        ClaudeSession {
            id: ClaudeSessionId::from(id),
            cwd: "/home/u/hive".to_string(),
            lifecycle: Lifecycle::Live,
            status: Some(SessionStatusState {
                status: SessionStatus::Working,
                needs_attention: false,
            }),
            last_activity: Some("2026-07-01T00:00:00Z".to_string()),
            placement: Some(TmuxPlacement {
                session_name: "🐝 hive".to_string(),
                window_index: "0".to_string(),
                window_name: "claude".to_string(),
                pane_id: Some("%1".to_string()),
            }),
            parent: Some("hive".to_string()),
            frozen: None,
        }
    }

    #[test]
    fn test_claude_session_id_roundtrip() {
        let id = ClaudeSessionId::from("abc-123");
        assert_eq!(id.as_str(), "abc-123");
        assert_eq!(id.to_string(), "abc-123");
        // transparent newtype serializes as a bare JSON string
        assert_eq!(serde_json::to_string(&id).unwrap(), "\"abc-123\"");
        let back: ClaudeSessionId = serde_json::from_str("\"abc-123\"").unwrap();
        assert_eq!(back, id);
    }

    #[test]
    fn test_lifecycle_serde_both_variants() {
        for lc in [Lifecycle::Live, Lifecycle::Closed] {
            let s = serde_json::to_string(&lc).unwrap();
            let back: Lifecycle = serde_json::from_str(&s).unwrap();
            assert_eq!(lc, back);
        }
    }

    #[test]
    fn test_lifecycle_helpers() {
        assert!(Lifecycle::Live.is_actionable_here());
        assert!(!Lifecycle::Live.is_closed_like());
        assert!(Lifecycle::Closed.is_closed_like());
        assert!(!Lifecycle::Closed.is_actionable_here());
    }

    #[test]
    fn test_claude_session_serde_roundtrip() {
        let s = sample("abc-123");
        let json = serde_json::to_string(&s).unwrap();
        let back: ClaudeSession = serde_json::from_str(&json).unwrap();
        assert_eq!(s, back);
    }

    #[test]
    fn test_claude_session_minimal_serde() {
        let mut s = sample("closed-1");
        s.status = None;
        s.placement = None;
        s.parent = None;
        s.frozen = None;
        s.lifecycle = Lifecycle::Closed;
        let json = serde_json::to_string(&s).unwrap();
        let back: ClaudeSession = serde_json::from_str(&json).unwrap();
        assert_eq!(s, back);
    }

    #[test]
    fn test_mark_closed_keeps_record() {
        let mut s = sample("abc-123");
        s.mark_closed();
        assert_eq!(s.lifecycle, Lifecycle::Closed);
        assert_eq!(s.placement, None);
        // identity / cwd / history retained (keep-record contract)
        assert_eq!(s.id, ClaudeSessionId::from("abc-123"));
        assert_eq!(s.cwd, "/home/u/hive");
        assert_eq!(s.last_activity.as_deref(), Some("2026-07-01T00:00:00Z"));
    }

    #[test]
    fn test_mark_live_sets_placement() {
        let mut s = sample("abc-123");
        s.mark_closed();
        let p = TmuxPlacement {
            session_name: "🐝 hive".to_string(),
            window_index: "1".to_string(),
            window_name: "claude".to_string(),
            pane_id: None,
        };
        s.mark_live(p.clone());
        assert_eq!(s.lifecycle, Lifecycle::Live);
        assert_eq!(s.placement, Some(p));
        assert_eq!(s.frozen, None);
    }

    #[test]
    fn test_freeze_is_closed_facet() {
        let mut s = sample("abc-123");
        s.freeze("postpone the refactor");
        assert_eq!(s.lifecycle, Lifecycle::Closed); // Frozen ⊂ Closed
        assert!(s.is_frozen());
        assert!(s.is_known_not_here());
        assert_eq!(s.placement, None);
        let f = s.frozen.unwrap();
        assert!(f.pinned);
        assert_eq!(f.note, "postpone the refactor");
        assert!(!f.frozen_at.is_empty());
    }

    #[test]
    fn test_thaw_restores_live() {
        let mut s = sample("abc-123");
        s.freeze("later");
        s.thaw();
        assert_eq!(s.lifecycle, Lifecycle::Live);
        assert_eq!(s.frozen, None);
    }

    #[test]
    fn test_sidecar_default_empty() {
        let sc = SessionSidecar::default();
        assert_eq!(serde_json::to_string(&sc).unwrap(), r#"{"sessions":{}}"#);
    }

    #[test]
    fn test_sidecar_serde_roundtrip() {
        let mut sc = SessionSidecar::default();
        sc.sessions.insert(
            "abc-123".to_string(),
            SessionOverlay {
                note: "wip".to_string(),
                pinned: true,
                archived: false,
                parent: Some("hive/CSD-1".to_string()),
                frozen_at: Some("2026-07-01T00:00:00Z".to_string()),
            },
        );
        let json = serde_json::to_string(&sc).unwrap();
        let back: SessionSidecar = serde_json::from_str(&json).unwrap();
        assert_eq!(back.sessions.len(), 1);
        assert_eq!(back.sessions["abc-123"].parent.as_deref(), Some("hive/CSD-1"));
    }

    #[test]
    fn test_sidecar_entry_backward_compat() {
        // A legacy-shaped overlay entry with only `note`; all other fields absent.
        let legacy = r#"{"note":"just a note"}"#;
        let ov: SessionOverlay = serde_json::from_str(legacy).unwrap();
        assert_eq!(ov.note, "just a note");
        assert!(!ov.pinned); // #[serde(default)]
        assert!(!ov.archived);
        assert_eq!(ov.parent, None);
        assert_eq!(ov.frozen_at, None);
    }
}
```

Build gate:
```
cargo build && cargo clippy -- -D warnings && cargo test && cargo fmt --check
```

---

### Seam & invariant coverage

**10 identity/correlation seams:**

| # | Seam | File / signature | Increment |
|---|------|------------------|-----------|
| S1 | `HookState.sessions` keyed by `session_id` UUID | `ipc/messages.rs` `HookState` | Inc 1 (read); cleanup left as-is (Inc 7 note) |
| S2 | out-of-band `tmux_pane` write + `record_window_seen` | `cli/hook.rs:158-194` | **Untouched** (no writer change; `parent` cached read-side → `sessions.json`) |
| S3 | `HookIndex::resolve_pane` + `by_cwd`/`cwd_shared` fallback | `common/instances.rs:69-115` | Inc 4 (pane-precise reused as `live_placements`); Inc 7 (cwd fallback deleted) |
| S4 | `find_worktree_by_session_name` | `common/worktree.rs:185` | Inc 3 (replaced by `resolve_parent`); Inc 7 (deleted) |
| S5 | `ProjectRegistry::find_by_session_name` / name-scan `has_project` | `common/projects.rs:193,200` | Inc 3 (replaced); Inc 7 (deleted) |
| S6 | `FrozenEntry::key()` (session_id else `session#window`) | `common/frozen.rs:54-59` | Inc 6 (session-id entries folded to Closed facet; composite-key excluded) |
| S7 | `OpenWindowsState` keyed by `claude_session_id` | `common/activity.rs:86` | Inc 6 (seeds `live_placements`) |
| S8 | jsonl `<uuid>.jsonl` basename == session_id | `common/jsonl.rs` `find_jsonl_by_session_id` | Inc 1 (existence scan), Inc 3 (cached) |
| S9 | `gather_sessions` cwd-keyed `hook_sessions`; `SessionInfo` keyed by tmux name | `tui/app.rs` | Inc 4 (flagged projection); Inc 7 (cwd map deleted) |
| S10 | `gather_session_data` / `SessionView` session-id-keyed | `serve/server.rs:25` | Inc 4 (projection preserves session_id key, I5); Inc 5 (mutation endpoints session-id-keyed) |

**5 distributed invariants:**

| Inv | Statement | Established / preserved in |
|-----|-----------|----------------------------|
| I1 | Identity = `session_id` UUID, never tmux name / abs path | Inc 0 (`ClaudeSessionId` newtype), Inc 1, Inc 6 (no fake ids for composite entries) |
| I2 | Project/Worktree identity = logical `(key, branch)`, not path | Inc 0 (`parent` stored as `make_key`), Inc 3 (local-only resolver + persisted-parent authoritative) |
| I3 | "Known but not actionable here" (closed == remote) is first-class | Inc 0 (`Lifecycle::Closed`, Frozen as facet), Inc 1, Inc 6 |
| I4 | "Where it runs" = nullable `TmuxPlacement`; "act on it" through ONE seam | Inc 0 (placement type), **Inc 5 (`act_on` funnels Send/Switch/Kill/Resume + reopen)** |
| I5 | Web API session model clean + session-id-keyed (inter-host protocol) | Inc 4 (read projection keeps session_id key), Inc 5 (mutation endpoints accept session_id) |

**Sequencing rationale:** Shadow (Inc 0-4, read-only, flag OFF) → soak → action seam + frozen fold (Inc 5-6, flagged) → cutover + delete dead code (Inc 7). The always-on `hive hook` writer and `state.json`'s schema are **never** modified, so every increment is a pure read over live state and rollback is always `unset HIVE_SESSION_REGISTRY` or reinstall `pre-reroot-known-good`.

Primary new file: **`/Users/emilianoperez/Projects/00-Personal/hive/src/common/registry.rs`**. Later touched: `src/common/jsonl.rs` (cached global scan), `src/common/mod.rs` (module registration), `src/ipc/messages.rs` (guard test only — no field added), `src/tui/app.rs` + `src/serve/server.rs` (flagged projection + mutation routing), `src/serve/web.rs` (session-id-keyed POST), `tests/cli_smoke.rs` (flag smoke).