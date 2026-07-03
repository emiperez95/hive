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
}
