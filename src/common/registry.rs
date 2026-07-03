//! Re-rooted session model, keyed by the Claude conversation UUID (the
//! `<uuid>.jsonl` basename). Built read-only as a shadow over the existing
//! `state.json` + a disk scan + a `sessions.json` overlay sidecar. Nothing here
//! is wired into a writer or a view yet (Increment 0).

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::common::persistence::cache_dir;

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
    /// Hive overlay (from the sidecar): free-text note.
    #[serde(default)]
    pub note: String,
    /// Hive overlay: user-pinned (surfaced regardless of recency bounding).
    #[serde(default)]
    pub pinned: bool,
    /// Hive overlay: hidden from default listings.
    #[serde(default)]
    pub archived: bool,
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

impl SessionSidecar {
    /// New derived file `~/.hive/cache/sessions.json`. Never repurposes existing
    /// state (state.json / frozen.json / worktrees.json stay authoritative).
    fn file_path() -> Option<PathBuf> {
        cache_dir().map(|p| p.join("sessions.json"))
    }

    /// Load the overlay from disk. Returns an empty sidecar on any error.
    pub fn load() -> Self {
        match Self::file_path() {
            Some(path) => Self::load_from(&path),
            None => Self::default(),
        }
    }

    /// Injectable load (for tests): missing or corrupt → default, never panics.
    pub fn load_from(path: &Path) -> Self {
        let Ok(content) = std::fs::read_to_string(path) else {
            return Self::default();
        };
        serde_json::from_str(&content).unwrap_or_default()
    }

    /// Save atomically (write .tmp, rename), cloning the `FrozenState` idiom.
    pub fn save(&self) -> Result<()> {
        let path = Self::file_path().ok_or_else(|| anyhow!("Cannot determine cache directory"))?;
        self.save_to(&path)
    }

    /// Injectable save (for tests).
    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let content = serde_json::to_string_pretty(self)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, &content)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }
}

/// The in-memory joined view: every known `ClaudeSession` keyed by its UUID.
/// Built READ-ONLY from the existing hook state (status), a disk scan
/// (existence — the source of truth for Closed/remote sessions), the live tmux
/// placements (the SOLE Live-vs-Closed discriminator), and the overlay sidecar.
/// I/O-free — all inputs are pre-loaded — so it is unit-testable without tmux.
#[derive(Debug, Clone, Default)]
pub struct SessionRegistry {
    pub sessions: HashMap<String, ClaudeSession>,
}

impl SessionRegistry {
    pub fn from_shadow(
        hook: &crate::ipc::messages::HookState,
        disk_ids: &[String],
        live_placements: &HashMap<String, TmuxPlacement>,
        sidecar: &SessionSidecar,
    ) -> Self {
        use std::collections::BTreeSet;
        // Union of every id we know about: hook status entries + on-disk transcripts.
        let mut ids: BTreeSet<&str> = BTreeSet::new();
        for k in hook.sessions.keys() {
            ids.insert(k.as_str());
        }
        for d in disk_ids {
            ids.insert(d.as_str());
        }

        let mut sessions = HashMap::new();
        for id in ids {
            let hook_entry = hook.sessions.get(id);
            // Single liveness rule: Live iff a live placement exists for this id.
            let placement = live_placements.get(id).cloned();
            let lifecycle = if placement.is_some() {
                Lifecycle::Live
            } else {
                Lifecycle::Closed
            };
            let status = hook_entry.map(|e| SessionStatusState {
                status: e.status.clone(),
                needs_attention: e.needs_attention,
            });
            let cwd = hook_entry.map(|e| e.cwd.clone()).unwrap_or_default();
            let last_activity = hook_entry.and_then(|e| e.last_activity.clone());
            // Overlay from the sidecar: parent (authoritative), note, pinned, archived.
            let overlay = sidecar.sessions.get(id);
            let parent = overlay.and_then(|o| o.parent.clone());
            let note = overlay.map(|o| o.note.clone()).unwrap_or_default();
            let pinned = overlay.map(|o| o.pinned).unwrap_or(false);
            let archived = overlay.map(|o| o.archived).unwrap_or(false);
            sessions.insert(
                id.to_string(),
                ClaudeSession {
                    id: ClaudeSessionId::from(id),
                    cwd,
                    lifecycle,
                    status,
                    last_activity,
                    placement,
                    parent,
                    frozen: None,
                    note,
                    pinned,
                    archived,
                },
            );
        }
        SessionRegistry { sessions }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::messages::{HookState, SessionState, SessionStatus};

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
            note: String::new(),
            pinned: false,
            archived: false,
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
        assert_eq!(
            back.sessions["abc-123"].parent.as_deref(),
            Some("hive/CSD-1")
        );
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

    // ---- Increment 1: from_shadow left-join + single liveness rule + coexistence ----

    fn state_entry(id: &str, cwd: &str, status: SessionStatus) -> SessionState {
        SessionState {
            session_id: id.to_string(),
            cwd: cwd.to_string(),
            status,
            needs_attention: false,
            last_activity: None,
            tmux_pane: None,
        }
    }

    fn placement(name: &str) -> TmuxPlacement {
        TmuxPlacement {
            session_name: name.to_string(),
            window_index: "0".to_string(),
            window_name: "claude".to_string(),
            pane_id: Some("%1".to_string()),
        }
    }

    #[test]
    fn test_live_iff_placement_present() {
        let mut hook = HookState::default();
        hook.sessions.insert(
            "abc".to_string(),
            state_entry("abc", "/x", SessionStatus::Working),
        );

        // Same hook entry, WITH a live placement → Live.
        let mut placements = HashMap::new();
        placements.insert("abc".to_string(), placement("🐝 hive"));
        let reg = SessionRegistry::from_shadow(&hook, &[], &placements, &SessionSidecar::default());
        assert_eq!(reg.sessions["abc"].lifecycle, Lifecycle::Live);
        assert!(reg.sessions["abc"].placement.is_some());

        // Same hook entry, WITHOUT a placement → Closed. (the sole discriminator)
        let reg2 =
            SessionRegistry::from_shadow(&hook, &[], &HashMap::new(), &SessionSidecar::default());
        assert_eq!(reg2.sessions["abc"].lifecycle, Lifecycle::Closed);
        assert_eq!(reg2.sessions["abc"].placement, None);
    }

    #[test]
    fn test_disk_only_session_is_closed() {
        // A conversation known only from a disk scan (no hook entry, no placement)
        // is Closed with no status — this is exactly the "remote session" shape (I3).
        let hook = HookState::default();
        let disk = vec!["disk-only".to_string()];
        let reg =
            SessionRegistry::from_shadow(&hook, &disk, &HashMap::new(), &SessionSidecar::default());
        let s = &reg.sessions["disk-only"];
        assert_eq!(s.lifecycle, Lifecycle::Closed);
        assert!(s.status.is_none());
        assert_eq!(s.placement, None);
    }

    #[test]
    fn test_hook_and_disk_left_join() {
        let mut hook = HookState::default();
        hook.sessions.insert(
            "both".to_string(),
            state_entry("both", "/x", SessionStatus::Waiting),
        );
        let disk = vec!["both".to_string()];
        let reg =
            SessionRegistry::from_shadow(&hook, &disk, &HashMap::new(), &SessionSidecar::default());
        assert_eq!(reg.sessions.len(), 1); // present in both → one row, no duplicate
        let s = &reg.sessions["both"];
        assert!(s.status.is_some()); // status from the hook side
        assert_eq!(s.cwd, "/x");
    }

    #[test]
    fn test_old_format_state_json_deserializes() {
        // The EXACT current on-disk schema written by the installed hook binary.
        // Safe-direction coexistence: old state.json must build the new model.
        let json = r#"{"sessions":{"abc-123":{"session_id":"abc-123","cwd":"/x","status":"Working","needs_attention":false,"last_activity":"2026-07-01T00:00:00Z","tmux_pane":"%1"}}}"#;
        let hook: HookState = serde_json::from_str(json).unwrap();
        let reg =
            SessionRegistry::from_shadow(&hook, &[], &HashMap::new(), &SessionSidecar::default());
        let s = &reg.sessions["abc-123"];
        assert_eq!(s.cwd, "/x");
        assert_eq!(s.lifecycle, Lifecycle::Closed); // no live placement supplied
        assert_eq!(s.status.as_ref().unwrap().status, SessionStatus::Working);
        assert_eq!(s.last_activity.as_deref(), Some("2026-07-01T00:00:00Z"));
    }

    #[test]
    fn test_new_state_json_still_reads_under_hookstate() {
        // Dangerous-direction coexistence: prove SessionState carries no
        // deny_unknown_fields, so an UNKNOWN future field is ignored and an older
        // installed binary never chokes on a newer state.json (HookState::load is
        // all-or-nothing — one parse error would drop the ENTIRE map to default).
        let json = r#"{"sessions":{"abc-123":{"session_id":"abc-123","cwd":"/x","status":"Working","needs_attention":false,"last_activity":null,"tmux_pane":null,"future_unknown_field":"whatever"}}}"#;
        let hook: HookState = serde_json::from_str(json).unwrap();
        assert!(hook.sessions.contains_key("abc-123"));
        assert_eq!(hook.sessions["abc-123"].cwd, "/x");
    }

    // ---- Increment 2: sessions.json sidecar persistence + overlay application ----

    fn overlay(note: &str, pinned: bool, parent: Option<&str>) -> SessionOverlay {
        SessionOverlay {
            note: note.to_string(),
            pinned,
            archived: false,
            parent: parent.map(|s| s.to_string()),
            frozen_at: None,
        }
    }

    #[test]
    fn test_from_shadow_applies_overlay() {
        // A disk-only session gets its note/pinned/parent from the sidecar overlay.
        let hook = HookState::default();
        let disk = vec!["abc-123".to_string()];
        let mut sidecar = SessionSidecar::default();
        sidecar.sessions.insert(
            "abc-123".to_string(),
            overlay("wip", true, Some("hive/CSD-1")),
        );
        let reg = SessionRegistry::from_shadow(&hook, &disk, &HashMap::new(), &sidecar);
        let s = &reg.sessions["abc-123"];
        assert_eq!(s.note, "wip");
        assert!(s.pinned);
        assert_eq!(s.parent.as_deref(), Some("hive/CSD-1"));
    }

    #[test]
    fn test_sidecar_save_load_roundtrip() {
        let dir = std::env::temp_dir().join(format!("hive-reg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sessions.json");
        let mut sc = SessionSidecar::default();
        sc.sessions.insert(
            "abc-123".to_string(),
            overlay("wip", true, Some("hive/CSD-1")),
        );
        sc.save_to(&path).unwrap();

        let back = SessionSidecar::load_from(&path);
        std::fs::remove_dir_all(&dir).ok();

        assert_eq!(back.sessions.len(), 1);
        assert_eq!(
            back.sessions["abc-123"].parent.as_deref(),
            Some("hive/CSD-1")
        );
        assert!(back.sessions["abc-123"].pinned);
    }

    #[test]
    fn test_sidecar_load_missing_is_default() {
        let path = std::env::temp_dir().join(format!("hive-missing-{}.json", std::process::id()));
        std::fs::remove_file(&path).ok();
        let sc = SessionSidecar::load_from(&path);
        assert!(sc.sessions.is_empty());
    }

    #[test]
    fn test_sidecar_load_corrupt_is_default() {
        let dir = std::env::temp_dir().join(format!("hive-corrupt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sessions.json");
        std::fs::write(&path, "{not valid json").unwrap();
        let sc = SessionSidecar::load_from(&path);
        std::fs::remove_dir_all(&dir).ok();
        assert!(sc.sessions.is_empty());
    }

    #[test]
    fn test_sidecar_file_path_basename() {
        // Pins the derived-file contract: the sidecar is sessions.json, never one
        // of the authoritative files. (No-op if there is no cache dir in the env.)
        if let Some(p) = SessionSidecar::file_path() {
            assert!(p.ends_with("sessions.json"));
            assert!(!p.ends_with("state.json"));
            assert!(!p.ends_with("frozen.json"));
        }
    }
}
