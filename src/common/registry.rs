//! Re-rooted conversation model, keyed by the Claude conversation UUID (the
//! `<uuid>.jsonl` basename). Built read-only as a shadow over the existing
//! `state.json` + a disk scan + a `conversations.json` overlay sidecar. Nothing here
//! is wired into a writer or a view yet (Increment 0).

use anyhow::{anyhow, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::common::frozen::FrozenState;
use crate::common::persistence::cache_dir;
use crate::common::projects::{expand_tilde, ProjectRegistry};
use crate::common::worktree::WorktreeState;

/// Invariant #1: identity is the Claude session UUID, never a tmux name or path.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ConversationId(pub String);

impl ConversationId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl From<&str> for ConversationId {
    fn from(s: &str) -> Self {
        ConversationId(s.to_string())
    }
}
impl std::fmt::Display for ConversationId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Invariant #3: "known but not running here" (Closed) is first-class and equals
/// "remote". This axis is ORTHOGONAL to `SessionStatus` (activity) — never overload
/// `SessionStatus`. Frozen is NOT a variant here — it is a facet of Closed (see
/// `Conversation.frozen`), so every "closed == remote" check includes frozen.
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
pub struct ConversationStatus {
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

/// The re-rooted base entity. Keyed by `ConversationId` everywhere.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Conversation {
    pub id: ConversationId,
    pub cwd: String,
    pub lifecycle: Lifecycle,
    #[serde(default)]
    pub status: Option<ConversationStatus>,
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
    /// User-assigned conversation title (from the transcript's `custom-title`).
    #[serde(default)]
    pub title: Option<String>,
    /// CLAUDE_CONFIG_DIR (auth profile) to resume under; None = default `~/.claude`.
    #[serde(default)]
    pub auth_config_dir: Option<String>,
}

impl Conversation {
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

/// The persisted overlay sidecar (NEW file `~/.hive/cache/conversations.json`; does not
/// repurpose existing state). Same shape/role as `FrozenState` — keyed by UUID.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ConversationSidecar {
    #[serde(default)]
    pub conversations: HashMap<String, ConversationOverlay>,
}

/// Only hive's overlay lives here; existence/status come from disk + state.json.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ConversationOverlay {
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

impl ConversationSidecar {
    /// New derived file `~/.hive/cache/conversations.json`. Never repurposes existing
    /// state (state.json / frozen.json / worktrees.json stay authoritative).
    fn file_path() -> Option<PathBuf> {
        cache_dir().map(|p| p.join("conversations.json"))
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

/// The in-memory joined view: every known `Conversation` keyed by its UUID.
/// Built READ-ONLY from the existing hook state (status), a disk scan
/// (existence — the source of truth for Closed/remote sessions), the live tmux
/// placements (the SOLE Live-vs-Closed discriminator), and the overlay sidecar.
/// I/O-free — all inputs are pre-loaded — so it is unit-testable without tmux.
#[derive(Debug, Clone, Default)]
pub struct ConversationRegistry {
    pub conversations: HashMap<String, Conversation>,
}

impl ConversationRegistry {
    pub fn from_shadow(
        hook: &crate::ipc::messages::HookState,
        disk_ids: &[String],
        live_placements: &HashMap<String, TmuxPlacement>,
        sidecar: &ConversationSidecar,
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

        let mut conversations = HashMap::new();
        for id in ids {
            let hook_entry = hook.sessions.get(id);
            // Single liveness rule: Live iff a live placement exists for this id.
            let placement = live_placements.get(id).cloned();
            let lifecycle = if placement.is_some() {
                Lifecycle::Live
            } else {
                Lifecycle::Closed
            };
            let status = hook_entry.map(|e| ConversationStatus {
                status: e.status.clone(),
                needs_attention: e.needs_attention,
            });
            let cwd = hook_entry.map(|e| e.cwd.clone()).unwrap_or_default();
            let last_activity = hook_entry.and_then(|e| e.last_activity.clone());
            // Overlay from the sidecar: parent (authoritative), note, pinned, archived.
            let overlay = sidecar.conversations.get(id);
            let parent = overlay.and_then(|o| o.parent.clone());
            let note = overlay.map(|o| o.note.clone()).unwrap_or_default();
            let pinned = overlay.map(|o| o.pinned).unwrap_or(false);
            let archived = overlay.map(|o| o.archived).unwrap_or(false);
            conversations.insert(
                id.to_string(),
                Conversation {
                    id: ConversationId::from(id),
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
                    title: None,
                    auth_config_dir: None,
                },
            );
        }
        ConversationRegistry { conversations }
    }

    /// Surface every frozen window from a `FrozenState`. Freeze == a pinned, noted
    /// subset of Closed. An entry that matches a known Closed conversation overlays
    /// its frozen facet; an entry we don't otherwise know — a legacy composite
    /// `session#window` key with no conversation id, or one whose transcript is
    /// gone — is added as a SYNTHETIC frozen conversation so it's still listed and
    /// thawable. Synthetic rows may be keyed by the composite key rather than a
    /// UUID; thaw goes through `frozen::thaw_window`, which handles both. A live
    /// conversation is never marked frozen (a stale entry for something now
    /// running isn't "frozen").
    pub fn apply_frozen(&mut self, frozen: &FrozenState) {
        for entry in frozen.frozen.values() {
            let key = entry.key();
            let info = FrozenInfo {
                note: entry.note.clone(),
                pinned: true,
                frozen_at: entry.frozen_at.clone(),
            };
            match self.conversations.get_mut(&key) {
                Some(c) => {
                    if c.lifecycle.is_closed_like() {
                        c.frozen = Some(info);
                    }
                }
                None => {
                    let title = (!entry.window_name.is_empty()).then(|| entry.window_name.clone());
                    self.conversations.insert(
                        key.clone(),
                        Conversation {
                            id: ConversationId(key),
                            cwd: entry.cwd.clone(),
                            lifecycle: Lifecycle::Closed,
                            status: None,
                            last_activity: Some(entry.frozen_at.clone()),
                            placement: None,
                            parent: None,
                            frozen: Some(info),
                            note: String::new(),
                            pinned: false,
                            archived: false,
                            title,
                            auth_config_dir: entry.claude_config_dir.clone(),
                        },
                    );
                }
            }
        }
    }
}

// ── Increment 3: parent resolution (host-local best-effort) + bounding + cache ──

/// Number of leading path components of `candidate` that match `cwd`, IF
/// `candidate` is a full component-wise prefix of `cwd`; else `None`. Component-
/// wise (not raw `starts_with`) so `/home/u/hive` does NOT match `/home/u/hivefoo`.
fn component_prefix_len(cwd: &Path, candidate: &Path) -> Option<usize> {
    let mut cwd_it = cwd.components();
    let mut matched = 0usize;
    for cand in candidate.components() {
        match cwd_it.next() {
            Some(w) if w == cand => matched += 1,
            _ => return None,
        }
    }
    Some(matched)
}

/// Resolve a cwd to its logical parent key (Invariant #2) by longest component-
/// wise path prefix: the deepest matching worktree wins, else the project root,
/// else `None` (a foreign/remote cwd must not false-match). Host-local best-effort.
pub fn resolve_parent(
    cwd: &str,
    worktrees: &WorktreeState,
    projects: &ProjectRegistry,
) -> Option<String> {
    let cwd_path = expand_tilde(cwd);
    // Projects first, then worktrees: on an equal-length tie `max_by_key` keeps the
    // LAST maximum, so the more-specific worktree key wins.
    let mut candidates: Vec<(usize, String)> = Vec::new();
    for (key, config) in &projects.projects {
        if let Some(len) = component_prefix_len(&cwd_path, &expand_tilde(&config.project_root)) {
            candidates.push((len, key.clone()));
        }
    }
    for entry in worktrees.worktrees.values() {
        if let Some(len) = component_prefix_len(&cwd_path, &expand_tilde(&entry.path)) {
            candidates.push((
                len,
                WorktreeState::make_key(&entry.project_key, &entry.branch),
            ));
        }
    }
    candidates
        .into_iter()
        .max_by_key(|(len, _)| *len)
        .map(|(_, key)| key)
}

/// The persisted parent is authoritative; the local resolver is a fallback only,
/// so a session's parent, once cached, is never re-derived from a shifting cwd.
pub fn effective_parent(
    session: &Conversation,
    worktrees: &WorktreeState,
    projects: &ProjectRegistry,
) -> Option<String> {
    if session.parent.is_some() {
        return session.parent.clone();
    }
    resolve_parent(&session.cwd, worktrees, projects)
}

/// Policy bounding which Closed sessions to surface (the on-disk set is unbounded).
pub struct BoundingCfg {
    pub max_age_days: i64,
}

/// Surface a Closed session iff it is not archived AND (recently active OR it has
/// a resolved parent OR it is a pinned freeze). Keeps the Closed list bounded.
pub fn should_surface_closed(
    session: &Conversation,
    now: DateTime<Utc>,
    cfg: &BoundingCfg,
) -> bool {
    if session.archived {
        return false;
    }
    let recent = match &session.last_activity {
        Some(ts) => match DateTime::parse_from_rfc3339(ts) {
            Ok(dt) => (now - dt.with_timezone(&Utc)).num_days() <= cfg.max_age_days,
            Err(_) => false,
        },
        None => false,
    };
    recent
        || session.parent.is_some()
        || session.pinned
        || session.frozen.as_ref().is_some_and(|f| f.pinned)
}

/// Cached disk scan so the ~1-1.5s refresh never does a full FS walk each tick.
pub struct ScanCache {
    pub ids: Vec<String>,
    pub scanned_at: DateTime<Utc>,
}

impl ScanCache {
    /// True while still within the rescan interval (inject `now` for testing).
    pub fn is_fresh(&self, now: DateTime<Utc>, interval_secs: i64) -> bool {
        (now - self.scanned_at).num_seconds() < interval_secs
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::messages::{HookState, SessionState, SessionStatus};

    fn sample(id: &str) -> Conversation {
        Conversation {
            id: ConversationId::from(id),
            cwd: "/home/u/hive".to_string(),
            lifecycle: Lifecycle::Live,
            status: Some(ConversationStatus {
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
            title: None,
            auth_config_dir: None,
        }
    }

    #[test]
    fn test_claude_session_id_roundtrip() {
        let id = ConversationId::from("abc-123");
        assert_eq!(id.as_str(), "abc-123");
        assert_eq!(id.to_string(), "abc-123");
        // transparent newtype serializes as a bare JSON string
        assert_eq!(serde_json::to_string(&id).unwrap(), "\"abc-123\"");
        let back: ConversationId = serde_json::from_str("\"abc-123\"").unwrap();
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
        let back: Conversation = serde_json::from_str(&json).unwrap();
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
        let back: Conversation = serde_json::from_str(&json).unwrap();
        assert_eq!(s, back);
    }

    #[test]
    fn test_mark_closed_keeps_record() {
        let mut s = sample("abc-123");
        s.mark_closed();
        assert_eq!(s.lifecycle, Lifecycle::Closed);
        assert_eq!(s.placement, None);
        // identity / cwd / history retained (keep-record contract)
        assert_eq!(s.id, ConversationId::from("abc-123"));
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
        let sc = ConversationSidecar::default();
        assert_eq!(
            serde_json::to_string(&sc).unwrap(),
            r#"{"conversations":{}}"#
        );
    }

    #[test]
    fn test_sidecar_serde_roundtrip() {
        let mut sc = ConversationSidecar::default();
        sc.conversations.insert(
            "abc-123".to_string(),
            ConversationOverlay {
                note: "wip".to_string(),
                pinned: true,
                archived: false,
                parent: Some("hive/CSD-1".to_string()),
                frozen_at: Some("2026-07-01T00:00:00Z".to_string()),
            },
        );
        let json = serde_json::to_string(&sc).unwrap();
        let back: ConversationSidecar = serde_json::from_str(&json).unwrap();
        assert_eq!(back.conversations.len(), 1);
        assert_eq!(
            back.conversations["abc-123"].parent.as_deref(),
            Some("hive/CSD-1")
        );
    }

    #[test]
    fn test_sidecar_entry_backward_compat() {
        // A legacy-shaped overlay entry with only `note`; all other fields absent.
        let legacy = r#"{"note":"just a note"}"#;
        let ov: ConversationOverlay = serde_json::from_str(legacy).unwrap();
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
        let reg = ConversationRegistry::from_shadow(
            &hook,
            &[],
            &placements,
            &ConversationSidecar::default(),
        );
        assert_eq!(reg.conversations["abc"].lifecycle, Lifecycle::Live);
        assert!(reg.conversations["abc"].placement.is_some());

        // Same hook entry, WITHOUT a placement → Closed. (the sole discriminator)
        let reg2 = ConversationRegistry::from_shadow(
            &hook,
            &[],
            &HashMap::new(),
            &ConversationSidecar::default(),
        );
        assert_eq!(reg2.conversations["abc"].lifecycle, Lifecycle::Closed);
        assert_eq!(reg2.conversations["abc"].placement, None);
    }

    #[test]
    fn test_disk_only_session_is_closed() {
        // A conversation known only from a disk scan (no hook entry, no placement)
        // is Closed with no status — this is exactly the "remote session" shape (I3).
        let hook = HookState::default();
        let disk = vec!["disk-only".to_string()];
        let reg = ConversationRegistry::from_shadow(
            &hook,
            &disk,
            &HashMap::new(),
            &ConversationSidecar::default(),
        );
        let s = &reg.conversations["disk-only"];
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
        let reg = ConversationRegistry::from_shadow(
            &hook,
            &disk,
            &HashMap::new(),
            &ConversationSidecar::default(),
        );
        assert_eq!(reg.conversations.len(), 1); // present in both → one row, no duplicate
        let s = &reg.conversations["both"];
        assert!(s.status.is_some()); // status from the hook side
        assert_eq!(s.cwd, "/x");
    }

    #[test]
    fn test_old_format_state_json_deserializes() {
        // The EXACT current on-disk schema written by the installed hook binary.
        // Safe-direction coexistence: old state.json must build the new model.
        let json = r#"{"sessions":{"abc-123":{"session_id":"abc-123","cwd":"/x","status":"Working","needs_attention":false,"last_activity":"2026-07-01T00:00:00Z","tmux_pane":"%1"}}}"#;
        let hook: HookState = serde_json::from_str(json).unwrap();
        let reg = ConversationRegistry::from_shadow(
            &hook,
            &[],
            &HashMap::new(),
            &ConversationSidecar::default(),
        );
        let s = &reg.conversations["abc-123"];
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

    // ---- Increment 2: conversations.json sidecar persistence + overlay application ----

    fn overlay(note: &str, pinned: bool, parent: Option<&str>) -> ConversationOverlay {
        ConversationOverlay {
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
        let mut sidecar = ConversationSidecar::default();
        sidecar.conversations.insert(
            "abc-123".to_string(),
            overlay("wip", true, Some("hive/CSD-1")),
        );
        let reg = ConversationRegistry::from_shadow(&hook, &disk, &HashMap::new(), &sidecar);
        let s = &reg.conversations["abc-123"];
        assert_eq!(s.note, "wip");
        assert!(s.pinned);
        assert_eq!(s.parent.as_deref(), Some("hive/CSD-1"));
    }

    #[test]
    fn test_sidecar_save_load_roundtrip() {
        let dir = std::env::temp_dir().join(format!("hive-reg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("conversations.json");
        let mut sc = ConversationSidecar::default();
        sc.conversations.insert(
            "abc-123".to_string(),
            overlay("wip", true, Some("hive/CSD-1")),
        );
        sc.save_to(&path).unwrap();

        let back = ConversationSidecar::load_from(&path);
        std::fs::remove_dir_all(&dir).ok();

        assert_eq!(back.conversations.len(), 1);
        assert_eq!(
            back.conversations["abc-123"].parent.as_deref(),
            Some("hive/CSD-1")
        );
        assert!(back.conversations["abc-123"].pinned);
    }

    #[test]
    fn test_sidecar_load_missing_is_default() {
        let path = std::env::temp_dir().join(format!("hive-missing-{}.json", std::process::id()));
        std::fs::remove_file(&path).ok();
        let sc = ConversationSidecar::load_from(&path);
        assert!(sc.conversations.is_empty());
    }

    #[test]
    fn test_sidecar_load_corrupt_is_default() {
        let dir = std::env::temp_dir().join(format!("hive-corrupt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("conversations.json");
        std::fs::write(&path, "{not valid json").unwrap();
        let sc = ConversationSidecar::load_from(&path);
        std::fs::remove_dir_all(&dir).ok();
        assert!(sc.conversations.is_empty());
    }

    #[test]
    fn test_sidecar_file_path_basename() {
        // Pins the derived-file contract: the sidecar is conversations.json, never one
        // of the authoritative files. (No-op if there is no cache dir in the env.)
        if let Some(p) = ConversationSidecar::file_path() {
            assert!(p.ends_with("conversations.json"));
            assert!(!p.ends_with("state.json"));
            assert!(!p.ends_with("frozen.json"));
        }
    }

    // ---- Increment 3: parent resolution + Closed-set bounding + scan cache ----

    fn closed(id: &str, last_activity: Option<&str>) -> Conversation {
        Conversation {
            id: ConversationId::from(id),
            cwd: "/x".to_string(),
            lifecycle: Lifecycle::Closed,
            status: None,
            last_activity: last_activity.map(|s| s.to_string()),
            placement: None,
            parent: None,
            frozen: None,
            note: String::new(),
            pinned: false,
            archived: false,
            title: None,
            auth_config_dir: None,
        }
    }

    fn project(root: &str) -> crate::common::projects::ProjectConfig {
        serde_json::from_str(&format!(r#"{{"emoji":"🐝","project_root":"{root}"}}"#)).unwrap()
    }

    fn worktree(pk: &str, branch: &str, path: &str) -> crate::common::worktree::WorktreeEntry {
        serde_json::from_str(&format!(
            r#"{{"project_key":"{pk}","branch":"{branch}","worktree_type":"worktree","path":"{path}","session_name":"s","created_at":""}}"#
        ))
        .unwrap()
    }

    fn wts_with(entries: &[(&str, &str, &str)]) -> WorktreeState {
        let mut w = WorktreeState::default();
        for (pk, br, path) in entries {
            w.worktrees
                .insert(WorktreeState::make_key(pk, br), worktree(pk, br, path));
        }
        w
    }

    fn projs_with(entries: &[(&str, &str)]) -> ProjectRegistry {
        let mut r = ProjectRegistry::default();
        for (key, root) in entries {
            r.projects.insert(key.to_string(), project(root));
        }
        r
    }

    #[test]
    fn test_resolve_parent_exact_worktree_prefix() {
        let wts = wts_with(&[("hive", "CSD-1", "/home/u/wt/CSD-1")]);
        let projs = projs_with(&[("hive", "/home/u/hive")]);
        assert_eq!(
            resolve_parent("/home/u/wt/CSD-1/src", &wts, &projs),
            Some("hive/CSD-1".to_string())
        );
    }

    #[test]
    fn test_resolve_parent_falls_back_to_project_root() {
        let projs = projs_with(&[("hive", "/home/u/hive")]);
        assert_eq!(
            resolve_parent("/home/u/hive/src", &WorktreeState::default(), &projs),
            Some("hive".to_string())
        );
    }

    #[test]
    fn test_resolve_longest_prefix_wins() {
        // Worktree lives INSIDE the project root; the deeper worktree wins.
        let wts = wts_with(&[("hive", "CSD-1", "/home/u/hive/wt/CSD-1")]);
        let projs = projs_with(&[("hive", "/home/u/hive")]);
        assert_eq!(
            resolve_parent("/home/u/hive/wt/CSD-1/src", &wts, &projs),
            Some("hive/CSD-1".to_string())
        );
    }

    #[test]
    fn test_resolve_component_boundary_no_false_prefix() {
        // /home/u/hivefoo must NOT match candidate /home/u/hive.
        let projs = projs_with(&[("hive", "/home/u/hive")]);
        assert_eq!(
            resolve_parent("/home/u/hivefoo/src", &WorktreeState::default(), &projs),
            None
        );
    }

    #[test]
    fn test_resolve_tilde_expansion() {
        let home = dirs::home_dir().expect("home dir");
        let cwd = home.join("hive").join("src");
        let projs = projs_with(&[("hive", "~/hive")]);
        assert_eq!(
            resolve_parent(&cwd.to_string_lossy(), &WorktreeState::default(), &projs),
            Some("hive".to_string())
        );
    }

    #[test]
    fn test_resolve_foreign_cwd_is_none() {
        let projs = projs_with(&[("hive", "/home/u/hive")]);
        assert_eq!(
            resolve_parent("/totally/foreign/path", &WorktreeState::default(), &projs),
            None
        );
    }

    #[test]
    fn test_persisted_parent_not_reresolved() {
        // A session with a cached parent is NOT re-derived from its cwd, even
        // though that cwd would resolve to a different local key.
        let mut s = closed("abc", None);
        s.cwd = "/home/u/hive/src".to_string();
        s.parent = Some("preset/CSD-9".to_string());
        let projs = projs_with(&[("hive", "/home/u/hive")]);
        assert_eq!(
            effective_parent(&s, &WorktreeState::default(), &projs),
            Some("preset/CSD-9".to_string())
        );
    }

    #[test]
    fn test_bounding_predicate() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-07-03T00:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let cfg = BoundingCfg { max_age_days: 7 };

        // Archived → never surfaced, even if recent.
        let mut arch = closed("a", Some("2026-07-02T00:00:00Z"));
        arch.archived = true;
        assert!(!should_surface_closed(&arch, now, &cfg));

        // Recently active → surfaced.
        assert!(should_surface_closed(
            &closed("b", Some("2026-07-02T00:00:00Z")),
            now,
            &cfg
        ));

        // Old but has a resolved parent → surfaced.
        let mut with_parent = closed("c", Some("2026-01-01T00:00:00Z"));
        with_parent.parent = Some("hive".to_string());
        assert!(should_surface_closed(&with_parent, now, &cfg));

        // Old, no parent, but a pinned freeze → surfaced.
        let mut frozen = closed("d", Some("2026-01-01T00:00:00Z"));
        frozen.frozen = Some(FrozenInfo {
            note: String::new(),
            pinned: true,
            frozen_at: "2026-01-01T00:00:00Z".to_string(),
        });
        assert!(should_surface_closed(&frozen, now, &cfg));

        // Old, no parent, not frozen → dropped.
        assert!(!should_surface_closed(
            &closed("e", Some("2026-01-01T00:00:00Z")),
            now,
            &cfg
        ));
    }

    #[test]
    fn test_scan_cache_reuses_within_interval() {
        let cache = ScanCache {
            ids: vec!["x".to_string()],
            scanned_at: chrono::DateTime::parse_from_rfc3339("2026-07-03T00:00:00Z")
                .unwrap()
                .with_timezone(&chrono::Utc),
        };
        let within = chrono::DateTime::parse_from_rfc3339("2026-07-03T00:00:05Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        assert!(cache.is_fresh(within, 30));
    }

    fn frozen_entry(id: Option<&str>, note: &str) -> crate::common::frozen::FrozenEntry {
        let idj = match id {
            Some(i) => format!("\"{i}\""),
            None => "null".to_string(),
        };
        serde_json::from_str(&format!(
            r#"{{"session_name":"s","cwd":"/x","frozen_at":"2026-07-01T00:00:00Z","claude_session_id":{idj},"note":"{note}"}}"#
        ))
        .unwrap()
    }

    #[test]
    fn test_frozen_entry_maps_to_closed_facet() {
        let mut reg = ConversationRegistry::default();
        reg.conversations
            .insert("abc".to_string(), closed("abc", None));
        let mut fs = FrozenState::default();
        fs.frozen
            .insert("abc".to_string(), frozen_entry(Some("abc"), "postpone"));

        reg.apply_frozen(&fs);

        let c = &reg.conversations["abc"];
        assert!(c.is_frozen());
        assert!(c.is_known_not_here()); // still Closed
        let f = c.frozen.as_ref().unwrap();
        assert_eq!(f.note, "postpone");
        assert!(f.pinned);
    }

    #[test]
    fn test_composite_key_frozen_surfaced_as_synthetic() {
        // A frozen entry with no conversation id (composite "session#window" key)
        // is surfaced as a synthetic frozen conversation so it's still listed.
        let mut reg = ConversationRegistry::default();
        reg.conversations
            .insert("abc".to_string(), closed("abc", None));
        let mut fs = FrozenState::default();
        fs.frozen
            .insert("s#1".to_string(), frozen_entry(None, "parked"));

        reg.apply_frozen(&fs);

        assert!(!reg.conversations["abc"].is_frozen()); // untouched
        assert_eq!(reg.conversations.len(), 2); // synthetic one added
        let synth = reg
            .conversations
            .values()
            .find(|c| c.is_frozen())
            .expect("synthetic frozen conversation");
        assert_eq!(synth.frozen.as_ref().unwrap().note, "parked");
        assert!(synth.lifecycle.is_closed_like());
    }

    #[test]
    fn test_frozen_not_applied_to_live() {
        // A stale freeze entry for a now-running conversation is ignored.
        let mut reg = ConversationRegistry::default();
        let mut live = closed("live1", None);
        live.lifecycle = Lifecycle::Live;
        reg.conversations.insert("live1".to_string(), live);
        let mut fs = FrozenState::default();
        fs.frozen
            .insert("live1".to_string(), frozen_entry(Some("live1"), "stale"));

        reg.apply_frozen(&fs);

        assert!(!reg.conversations["live1"].is_frozen());
    }

    #[test]
    fn test_scan_cache_refreshes_after_interval() {
        let cache = ScanCache {
            ids: vec!["x".to_string()],
            scanned_at: chrono::DateTime::parse_from_rfc3339("2026-07-03T00:00:00Z")
                .unwrap()
                .with_timezone(&chrono::Utc),
        };
        let after = chrono::DateTime::parse_from_rfc3339("2026-07-03T00:01:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        assert!(!cache.is_fresh(after, 30));
    }
}
