//! Every session's todos, regrouped by the PROJECT that owns the session.
//!
//! Todos are stored per tmux session (`todos.txt`, `session\ttext`), which is the
//! right key for adding one from inside a window but the wrong one for reading them
//! back: a project's work is spread over its own session and one per worktree. The
//! owner of a session is resolved by NAME — a project's derived session name, or a
//! worktree's registered one — so this needs no live tmux and no registry gather.
//! A session nothing claims (renamed by hand, a project since removed) still shows,
//! as its own group: a todo you wrote down must never vanish from the overview.

use std::collections::HashMap;

use super::projects::ProjectRegistry;
use super::worktree::WorktreeState;

/// Who owns a session: the project key, plus the branch when it's a worktree's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionOwner {
    pub project: String,
    pub branch: Option<String>,
}

/// One todo, with enough to act on it (`index` is its 0-based slot in the session's list).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TodoRow {
    pub session: String,
    pub branch: Option<String>,
    pub index: usize,
    pub text: String,
}

/// The todos of one project (`project: Some`) or of one unclaimed session (`None`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TodoGroup {
    pub project: Option<String>,
    pub rows: Vec<TodoRow>,
}

impl TodoGroup {
    /// The session an unclaimed group stands for (all its rows share it).
    pub fn session(&self) -> Option<&str> {
        self.rows.first().map(|r| r.session.as_str())
    }
}

/// Session name → owner, for every registered project and worktree.
pub fn session_owners(
    projects: &ProjectRegistry,
    worktrees: &WorktreeState,
) -> HashMap<String, SessionOwner> {
    let mut owners: HashMap<String, SessionOwner> = HashMap::new();
    for (key, config) in &projects.projects {
        owners.insert(
            ProjectRegistry::session_name(key, config),
            SessionOwner {
                project: key.clone(),
                branch: None,
            },
        );
    }
    // Inserted second: a worktree's registered name is the more specific claim.
    for e in worktrees.worktrees.values() {
        owners.insert(
            e.session_name.clone(),
            SessionOwner {
                project: e.project_key.clone(),
                branch: Some(e.branch.clone()),
            },
        );
    }
    owners
}

/// A worktree session hive named (`{emoji} [{project}] {branch}`) whose worktree is no
/// longer registered: the tag still says which project it was, so claim it for that
/// project when the project itself is still registered.
fn owner_from_name(session: &str, owners: &HashMap<String, SessionOwner>) -> Option<SessionOwner> {
    let (_, rest) = session.split_once('[')?;
    let (project, branch) = rest.split_once(']')?;
    let branch = branch.trim();
    let known = owners.values().any(|o| o.project == project);
    (known && !branch.is_empty()).then(|| SessionOwner {
        project: project.to_string(),
        branch: Some(branch.to_string()),
    })
}

/// Regroup per-session todos by owning project. Groups are ordered by `label` (case-
/// insensitive) with unclaimed sessions last; within a project, its own session comes
/// first and then its worktrees by branch; each session keeps its stored order.
pub fn group_todos(
    todos: &HashMap<String, Vec<String>>,
    owners: &HashMap<String, SessionOwner>,
    label: impl Fn(&str) -> String,
) -> Vec<TodoGroup> {
    let mut by_project: HashMap<String, Vec<TodoRow>> = HashMap::new();
    let mut unclaimed: Vec<TodoGroup> = Vec::new();
    for (session, items) in todos {
        if items.is_empty() {
            continue;
        }
        let owner = owners
            .get(session)
            .cloned()
            .or_else(|| owner_from_name(session, owners));
        let owner = owner.as_ref();
        let rows = items.iter().enumerate().map(|(index, text)| TodoRow {
            session: session.clone(),
            branch: owner.and_then(|o| o.branch.clone()),
            index,
            text: text.clone(),
        });
        match owner {
            Some(o) => by_project
                .entry(o.project.clone())
                .or_default()
                .extend(rows),
            None => unclaimed.push(TodoGroup {
                project: None,
                rows: rows.collect(),
            }),
        }
    }

    let mut groups: Vec<TodoGroup> = by_project
        .into_iter()
        .map(|(project, mut rows)| {
            // `None` (the project's own session) sorts before any branch.
            rows.sort_by(|a, b| {
                a.branch
                    .as_deref()
                    .map(str::to_lowercase)
                    .cmp(&b.branch.as_deref().map(str::to_lowercase))
                    .then_with(|| a.session.cmp(&b.session))
                    .then_with(|| a.index.cmp(&b.index))
            });
            TodoGroup {
                project: Some(project),
                rows,
            }
        })
        .collect();
    groups.sort_by_cached_key(|g| label(g.project.as_deref().unwrap_or_default()).to_lowercase());
    unclaimed.sort_by_cached_key(|g| g.session().unwrap_or_default().to_lowercase());
    groups.extend(unclaimed);
    groups
}

/// [`group_todos`] over the on-disk active todos and the current registries,
/// labelled by each project's display name.
pub fn load_grouped_todos(projects: &ProjectRegistry) -> Vec<TodoGroup> {
    let owners = session_owners(projects, &WorktreeState::load());
    let todos = super::persistence::load_session_todos();
    group_todos(&todos, &owners, |key| project_display_name(projects, key))
}

/// Forget the todos (active AND done) of sessions that are going away — a removed
/// project's session and its worktrees', or a deleted worktree's. Todos are keyed by
/// session name, so nothing else would ever clear them: they would sit in the todos
/// screen as `(no project)` forever. Returns the ACTIVE todos dropped, so the caller
/// can print them (a todo you wrote down shouldn't disappear without a trace).
pub fn drop_session_todos(sessions: &[String]) -> Vec<(String, String)> {
    use super::persistence::{
        load_completed_todos, load_session_todos, save_completed_todos, save_session_todos,
    };
    let mut active = load_session_todos();
    let dropped = remove_sessions(&mut active, sessions);
    if !dropped.is_empty() {
        save_session_todos(&active);
    }
    let mut done = load_completed_todos();
    if !remove_sessions(&mut done, sessions).is_empty() {
        save_completed_todos(&done);
    }
    dropped
}

/// Pure core of [`drop_session_todos`]: remove `sessions` from `todos`, returning
/// what was removed as `(session, text)` in session order.
fn remove_sessions(
    todos: &mut HashMap<String, Vec<String>>,
    sessions: &[String],
) -> Vec<(String, String)> {
    let mut dropped = Vec::new();
    for s in sessions {
        if let Some(items) = todos.remove(s) {
            dropped.extend(items.into_iter().map(|t| (s.clone(), t)));
        }
    }
    dropped
}

/// A project's display name (falls back to its key).
pub fn project_display_name(projects: &ProjectRegistry, key: &str) -> String {
    projects
        .projects
        .get(key)
        .and_then(|c| c.display_name.clone())
        .unwrap_or_else(|| key.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owner(project: &str, branch: Option<&str>) -> SessionOwner {
        SessionOwner {
            project: project.into(),
            branch: branch.map(Into::into),
        }
    }

    fn todos(pairs: &[(&str, &[&str])]) -> HashMap<String, Vec<String>> {
        pairs
            .iter()
            .map(|(s, t)| (s.to_string(), t.iter().map(|x| x.to_string()).collect()))
            .collect()
    }

    #[test]
    fn worktree_sessions_fold_into_their_project() {
        let owners: HashMap<_, _> = [
            ("🐝 hive".to_string(), owner("hive", None)),
            (
                "🐝 [hive] mobile".to_string(),
                owner("hive", Some("mobile")),
            ),
        ]
        .into();
        let t = todos(&[("🐝 [hive] mobile", &["m1"]), ("🐝 hive", &["h1", "h2"])]);
        let g = group_todos(&t, &owners, |k| k.to_string());
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].project.as_deref(), Some("hive"));
        // The project's own session first, then its worktrees; stored order kept.
        let texts: Vec<_> = g[0].rows.iter().map(|r| r.text.as_str()).collect();
        assert_eq!(texts, ["h1", "h2", "m1"]);
        assert_eq!(g[0].rows[1].index, 1);
        assert_eq!(g[0].rows[2].branch.as_deref(), Some("mobile"));
    }

    #[test]
    fn unclaimed_sessions_still_show_after_projects() {
        let owners: HashMap<_, _> = [("🌐 Network".to_string(), owner("network", None))].into();
        let t = todos(&[("📁 gone", &["orphan"]), ("🌐 Network", &["n"])]);
        let g = group_todos(&t, &owners, |k| k.to_string());
        assert_eq!(g.len(), 2);
        assert_eq!(g[0].project.as_deref(), Some("network"));
        assert_eq!(g[1].project, None);
        assert_eq!(g[1].session(), Some("📁 gone"));
    }

    #[test]
    fn an_unregistered_worktree_session_is_claimed_by_its_tag() {
        let owners: HashMap<_, _> = [("🐝 hive".to_string(), owner("hive", None))].into();
        let t = todos(&[("🐝 [hive] mobile", &["m"]), ("📊 [avateen] x", &["a"])]);
        let g = group_todos(&t, &owners, |k| k.to_string());
        assert_eq!(g[0].project.as_deref(), Some("hive"));
        assert_eq!(g[0].rows[0].branch.as_deref(), Some("mobile"));
        // A tag naming no registered project stays unclaimed rather than inventing one.
        assert_eq!(g[1].project, None);
    }

    #[test]
    fn removing_sessions_drops_only_theirs() {
        let mut t = todos(&[("a", &["1", "2"]), ("b", &["3"]), ("c", &["4"])]);
        let dropped = remove_sessions(&mut t, &["a".into(), "c".into(), "gone".into()]);
        assert_eq!(
            dropped,
            [
                ("a".into(), "1".into()),
                ("a".into(), "2".into()),
                ("c".into(), "4".into())
            ]
        );
        assert_eq!(t.keys().collect::<Vec<_>>(), ["b"]);
    }

    #[test]
    fn groups_sort_by_label_and_empty_lists_drop() {
        let owners: HashMap<_, _> = [
            ("a".to_string(), owner("zkey", None)),
            ("b".to_string(), owner("akey", None)),
        ]
        .into();
        let t = todos(&[("a", &["x"]), ("b", &["y"]), ("c", &[])]);
        // Labels invert the key order: sorting follows the label, not the key.
        let g = group_todos(&t, &owners, |k| {
            if k == "zkey" { "Alpha" } else { "beta" }.into()
        });
        let keys: Vec<_> = g.iter().map(|g| g.project.as_deref().unwrap()).collect();
        assert_eq!(keys, ["zkey", "akey"]);
    }

    #[test]
    fn a_worktree_claim_beats_a_colliding_project_name() {
        let mut projects = ProjectRegistry::default();
        let cfg = super::super::projects::ProjectConfig {
            emoji: "📊".into(),
            ..Default::default()
        };
        projects.projects.insert("avateen".into(), cfg);
        let mut wts = WorktreeState::default();
        let e = super::super::worktree::WorktreeEntry {
            project_key: "other".into(),
            branch: "b".into(),
            worktree_type: "worktree".into(),
            path: "/y".into(),
            session_name: "📊 avateen".into(),
            metadata: serde_json::Value::Null,
            created_at: String::new(),
        };
        wts.worktrees.insert("other/b".into(), e);
        let owners = session_owners(&projects, &wts);
        assert_eq!(owners["📊 avateen"], owner("other", Some("b")));
    }
}
