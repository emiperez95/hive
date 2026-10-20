//! What the sidebar lists, as plain data — no terminal, no I/O.
//!
//! [`conv_rows`] flattens `/api/active` into one row per conversation, and
//! [`build_items`] turns rows + frozen + projects into the flat item list for
//! the current view. Both mirror the web sidebar (`sbConvRows`, `sbNest`,
//! `sbProjectsHtml`, `renderSidebarList`) so the two surfaces never disagree
//! about what is where.

use std::collections::{HashMap, HashSet};

use super::api::{FrozenItem, ProjectItem};
use crate::ipc::messages::SessionStatus;
use crate::serve::web_types::SessionView;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum View {
    #[default]
    Live,
    Attention,
    Projects,
}

/// Where a session lives: its project, and the worktree branch when it is one.
#[derive(Debug, Clone, PartialEq)]
pub struct Place {
    pub key: String,
    pub label: String,
    pub emoji: String,
    pub branch: Option<String>,
}

impl Place {
    /// `emoji label · branch`, the second line of an attention row.
    pub fn line(&self) -> String {
        let head = [self.emoji.as_str(), self.label.as_str()]
            .iter()
            .filter(|s| !s.is_empty())
            .copied()
            .collect::<Vec<_>>()
            .join(" ");
        match &self.branch {
            Some(b) => format!("{head} · {b}"),
            None => head,
        }
    }
}

/// One conversation row (or a windowless session's single row).
#[derive(Debug, Clone, PartialEq)]
pub struct Conv {
    pub session: String,
    /// `None` for a session with no windows — switching goes to the session.
    pub window_index: Option<String>,
    pub session_id: Option<String>,
    pub pane_id: Option<String>,
    pub title: String,
    pub place: Place,
    pub tier: u8,
    pub rank: Option<u32>,
    pub status: Option<SessionStatus>,
    pub age: String,
    pub unseen: bool,
    pub skipped: bool,
}

impl Conv {
    pub fn blocks(&self) -> bool {
        self.status.as_ref().is_some_and(|s| s.blocks_human())
    }
}

/// The project registry, indexed the two ways a session name is resolved.
#[derive(Debug, Default)]
pub struct Projects {
    pub list: Vec<ProjectItem>,
    by_key: HashMap<String, usize>,
    by_session: HashMap<String, usize>,
}

impl Projects {
    pub fn new(list: Vec<ProjectItem>) -> Self {
        let by_key = list
            .iter()
            .enumerate()
            .map(|(i, p)| (p.key.clone(), i))
            .collect();
        let by_session = list
            .iter()
            .enumerate()
            .map(|(i, p)| (p.session_name.clone(), i))
            .collect();
        Self {
            list,
            by_key,
            by_session,
        }
    }

    fn key(&self, k: &str) -> Option<&ProjectItem> {
        self.by_key.get(k).map(|&i| &self.list[i])
    }

    fn session(&self, s: &str) -> Option<&ProjectItem> {
        self.by_session.get(s).map(|&i| &self.list[i])
    }
}

/// Where a session lives, from its name (see "Session Naming" in CLAUDE.md):
/// `📊 [avateen] live-avatar` → project avateen, worktree live-avatar;
/// `📊 Avateen` → project avateen. A worktree of an unregistered project still
/// groups, labelled by its key.
pub fn place_of(session: &str, projects: &Projects) -> Place {
    if let (Some(open), Some(close)) = (session.find('['), session.find(']')) {
        if open < close {
            let key = &session[open + 1..close];
            let branch = session[close + 1..].trim();
            let p = projects.key(key);
            let emoji = p
                .map(|p| p.emoji.clone())
                .filter(|e| !e.is_empty())
                .unwrap_or_else(|| session[..open].trim().to_string());
            return Place {
                key: key.to_string(),
                label: p.map_or_else(|| key.to_string(), |p| p.display_name.clone()),
                emoji,
                branch: Some(if branch.is_empty() {
                    "(worktree)".into()
                } else {
                    branch.into()
                }),
            };
        }
    }
    if let Some(p) = projects.session(session) {
        return Place {
            key: p.key.clone(),
            label: p.display_name.clone(),
            emoji: p.emoji.clone(),
            branch: None,
        };
    }
    let (emoji, rest) = split_emoji(session);
    Place {
        key: session.to_string(),
        label: if rest.is_empty() { session } else { rest }.to_string(),
        emoji: emoji.to_string(),
        branch: None,
    }
}

/// A leading non-ASCII word (the session emoji) and the rest.
pub fn split_emoji(name: &str) -> (&str, &str) {
    match name.split_once(' ') {
        Some((head, rest)) if !head.is_empty() && !head.is_ascii() => (head, rest.trim()),
        _ => ("", name),
    }
}

/// One row per CONVERSATION, in the server's session order — a session hosting
/// three contributes three rows. A session with no windows contributes one.
pub fn conv_rows(
    sessions: &[SessionView],
    projects: &Projects,
    now: chrono::DateTime<chrono::Utc>,
) -> Vec<Conv> {
    let age_since = |iso: Option<&str>| {
        iso.and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
            .map(|t| dur((now - t.with_timezone(&chrono::Utc)).num_seconds().max(0) as u64))
            .unwrap_or_default()
    };
    let mut out = Vec::new();
    for s in sessions {
        let place = place_of(&s.name, projects);
        if s.windows.is_empty() {
            out.push(Conv {
                session: s.name.clone(),
                window_index: None,
                session_id: None,
                pane_id: None,
                title: split_emoji(&s.name).1.to_string(),
                place,
                tier: 2,
                rank: None,
                status: s.status.clone(),
                age: age_since(s.last_activity.as_deref()),
                unseen: false,
                skipped: s.skipped,
            });
            continue;
        }
        for w in &s.windows {
            let named = !w.window_name.is_empty()
                && !w
                    .window_name
                    .chars()
                    .all(|c| c.is_ascii_digit() || c == '.');
            // Working and blocked rows show how long they've held the state —
            // their last_activity is stamped by every tool call, so always "now".
            let held =
                (w.attention == 0 || w.attention == 3) && w.status != Some(SessionStatus::Waiting);
            out.push(Conv {
                session: s.name.clone(),
                window_index: Some(w.window_index.clone()),
                session_id: w.session_id.clone(),
                pane_id: Some(w.pane_id.clone()),
                title: if named {
                    w.window_name.clone()
                } else {
                    format!("Window {}", w.window_index)
                },
                place: place.clone(),
                tier: w.attention,
                rank: w.attention_rank,
                status: w.status.clone(),
                age: if held {
                    w.state_secs.map(|s| dur(u64::from(s))).unwrap_or_default()
                } else {
                    age_since(w.last_activity.as_deref())
                },
                unseen: w.unseen,
                skipped: s.skipped,
            });
        }
    }
    out
}

pub fn dur(s: u64) -> String {
    match s {
        0..=59 => "now".into(),
        60..=3599 => format!("{}m", s / 60),
        3600..=86399 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86400),
    }
}

/// The web sidebar's state words, so both surfaces read the same.
pub fn state_word(st: &Option<SessionStatus>) -> &'static str {
    match st {
        None => "—",
        Some(SessionStatus::Waiting) => "idle",
        Some(SessionStatus::Working) | Some(SessionStatus::Unknown) => "work",
        Some(SessionStatus::QuestionAsked) => "ask",
        Some(SessionStatus::PlanReview) => "plan",
        Some(SessionStatus::NeedsPermission { .. }) => "perm",
        Some(SessionStatus::EditApproval { .. }) => "edit",
        Some(SessionStatus::RunningWorkflow { .. }) => "flow",
        Some(SessionStatus::NeedsInput { .. }) => "needs",
    }
}

/// The two folded sections below every list view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Sect {
    Skipped,
    Frozen,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Item {
    /// Dim one-line text (counts, "no conversations").
    Note(String),
    Blank,
    /// An attention tier heading.
    Tier {
        tier: u8,
        n: usize,
    },
    /// A session / project heading carrying that project's mute.
    Group {
        emoji: String,
        label: String,
        key: String,
    },
    /// A worktree heading inside a nested group.
    Worktree(String),
    Section {
        sect: Sect,
        n: usize,
        open: bool,
    },
    ProjectHead {
        key: String,
        emoji: String,
        label: String,
        archived: bool,
        count: usize,
        open: bool,
    },
    /// Index into the conv rows. `flat` = the two-line attention row.
    Conv {
        idx: usize,
        flat: bool,
        indent: u8,
    },
    Frozen {
        idx: usize,
        indent: u8,
    },
}

impl Item {
    /// Can the cursor rest here (and Enter act on it)?
    pub fn selectable(&self) -> bool {
        matches!(
            self,
            Item::Conv { .. }
                | Item::Frozen { .. }
                | Item::Section { .. }
                | Item::ProjectHead { .. }
        )
    }

    /// A key that survives a rebuild, so the cursor stays on the same thing
    /// when the list under it is refreshed.
    pub fn key(&self, convs: &[Conv], frozen: &[FrozenItem]) -> Option<String> {
        match self {
            Item::Conv { idx, .. } => convs.get(*idx).map(|c| {
                format!(
                    "c:{}:{}",
                    c.session,
                    c.window_index.as_deref().unwrap_or("")
                )
            }),
            Item::Frozen { idx, .. } => frozen.get(*idx).map(|f| format!("f:{}", f.key)),
            Item::Section { sect, .. } => Some(format!("s:{sect:?}")),
            Item::ProjectHead { key, .. } => Some(format!("p:{key}")),
            _ => None,
        }
    }
}

/// Fold state the user controls, remembered across launches.
pub struct ListState<'a> {
    pub view: View,
    pub skipped_open: bool,
    pub frozen_open: bool,
    pub show_archived: bool,
    pub expanded: &'a HashSet<String>,
}

const TIERS: [u8; 4] = [0, 1, 2, 3];

pub fn build_items(
    st: &ListState,
    convs: &[Conv],
    frozen: &[FrozenItem],
    projects: &Projects,
) -> Vec<Item> {
    let mut items = Vec::new();
    match st.view {
        View::Projects => {
            projects_view(&mut items, st, convs, frozen, projects);
            return items;
        }
        View::Attention => {
            let mut ranked: Vec<usize> = (0..convs.len())
                .filter(|&i| !convs[i].skipped && convs[i].rank.is_some())
                .collect();
            ranked.sort_by_key(|&i| convs[i].rank);
            if ranked.is_empty() {
                items.push(Item::Note("No live conversations".into()));
            }
            // Every tier renders open: idle is the list of windows free to take,
            // half of what this view is for.
            for t in TIERS {
                let mine: Vec<usize> = ranked
                    .iter()
                    .copied()
                    .filter(|&i| convs[i].tier == t)
                    .collect();
                if mine.is_empty() {
                    continue;
                }
                items.push(Item::Blank);
                items.push(Item::Tier {
                    tier: t,
                    n: mine.len(),
                });
                items.extend(mine.into_iter().map(|idx| Item::Conv {
                    idx,
                    flat: true,
                    indent: 0,
                }));
            }
        }
        View::Live => {
            let live: Vec<usize> = (0..convs.len()).filter(|&i| !convs[i].skipped).collect();
            if live.is_empty() {
                items.push(Item::Note("No live conversations".into()));
            }
            let mut last: Option<&str> = None;
            for i in live {
                let c = &convs[i];
                if last != Some(c.session.as_str()) {
                    // A blank line between sessions: rows are tinted cards, and
                    // two groups' cards touching read as one block.
                    if last.is_some() {
                        items.push(Item::Blank);
                    }
                    let (emoji, rest) = split_emoji(&c.session);
                    items.push(Item::Group {
                        emoji: emoji.to_string(),
                        label: if rest.is_empty() { &c.session } else { rest }.to_string(),
                        key: c.place.key.clone(),
                    });
                    last = Some(&c.session);
                }
                items.push(Item::Conv {
                    idx: i,
                    flat: false,
                    indent: 1,
                });
            }
        }
    }

    // Skipped and Frozen sit below in both live views — set aside, not ranked.
    let skipped: Vec<usize> = (0..convs.len()).filter(|&i| convs[i].skipped).collect();
    if !skipped.is_empty() {
        let n = skipped
            .iter()
            .map(|&i| &convs[i].session)
            .collect::<HashSet<_>>()
            .len();
        items.push(Item::Blank);
        items.push(Item::Section {
            sect: Sect::Skipped,
            n,
            open: st.skipped_open,
        });
        if st.skipped_open {
            let entries = skipped
                .iter()
                .map(|&i| {
                    (
                        convs[i].place.clone(),
                        Item::Conv {
                            idx: i,
                            flat: false,
                            indent: 2,
                        },
                    )
                })
                .collect();
            nested(&mut items, entries);
        }
    }
    if !frozen.is_empty() {
        items.push(Item::Blank);
        items.push(Item::Section {
            sect: Sect::Frozen,
            n: frozen.len(),
            open: st.frozen_open,
        });
        if st.frozen_open {
            let entries = frozen
                .iter()
                .enumerate()
                .map(|(i, f)| {
                    (
                        place_of(&f.session_name, projects),
                        Item::Frozen { idx: i, indent: 2 },
                    )
                })
                .collect();
            nested(&mut items, entries);
        }
    }
    items
}

/// Entries bucketed by project, then worktree — source order kept at every level.
fn nested(items: &mut Vec<Item>, entries: Vec<(Place, Item)>) {
    for g in bucket(entries) {
        items.push(Item::Group {
            emoji: g.place.emoji.clone(),
            label: g.place.label.clone(),
            key: g.place.key.clone(),
        });
        push_body(items, g);
    }
}

struct Bucket {
    place: Place,
    own: Vec<Item>,
    worktrees: Vec<(String, Vec<Item>)>,
}

impl Bucket {
    fn count(&self) -> usize {
        self.own.len() + self.worktrees.iter().map(|(_, v)| v.len()).sum::<usize>()
    }
}

fn bucket(entries: Vec<(Place, Item)>) -> Vec<Bucket> {
    let mut out: Vec<Bucket> = Vec::new();
    for (place, item) in entries {
        let i = match out.iter().position(|b| b.place.key == place.key) {
            Some(i) => i,
            None => {
                out.push(Bucket {
                    place: Place {
                        branch: None,
                        ..place.clone()
                    },
                    own: Vec::new(),
                    worktrees: Vec::new(),
                });
                out.len() - 1
            }
        };
        let b = &mut out[i];
        match place.branch {
            None => b.own.push(item),
            Some(branch) => match b.worktrees.iter_mut().find(|(n, _)| *n == branch) {
                Some((_, v)) => v.push(item),
                None => b.worktrees.push((branch, vec![item])),
            },
        }
    }
    out
}

fn push_body(items: &mut Vec<Item>, b: Bucket) {
    items.extend(b.own);
    for (branch, v) in b.worktrees {
        items.push(Item::Worktree(branch));
        items.extend(v);
    }
}

/// Every registered project, project → worktree → conversation, one project
/// open at a time. Projects with something in them come first in list order,
/// the rest alphabetically. Archived ones hide unless shown — but never one
/// with conversations: you can't hide what's running.
fn projects_view(
    items: &mut Vec<Item>,
    st: &ListState,
    convs: &[Conv],
    frozen: &[FrozenItem],
    projects: &Projects,
) {
    let mut entries: Vec<(Place, Item)> = convs
        .iter()
        .enumerate()
        .map(|(i, c)| {
            (
                c.place.clone(),
                Item::Conv {
                    idx: i,
                    flat: false,
                    indent: 2,
                },
            )
        })
        .collect();
    entries.extend(frozen.iter().enumerate().map(|(i, f)| {
        (
            place_of(&f.session_name, projects),
            Item::Frozen { idx: i, indent: 2 },
        )
    }));
    let buckets = bucket(entries);
    let archived: HashSet<&str> = projects
        .list
        .iter()
        .filter(|p| p.archived)
        .map(|p| p.key.as_str())
        .collect();
    let seen: HashSet<String> = buckets.iter().map(|b| b.place.key.clone()).collect();
    let mut empties: Vec<&ProjectItem> = projects
        .list
        .iter()
        .filter(|p| !seen.contains(&p.key) && (st.show_archived || !p.archived))
        .collect();
    empties.sort_by_key(|p| p.display_name.to_lowercase());

    let total = buckets.len() + empties.len();
    let hidden = if st.show_archived {
        0
    } else {
        archived.iter().filter(|k| !seen.contains(**k)).count()
    };
    items.push(Item::Note(format!(
        "{total} project{}{}",
        if total == 1 { "" } else { "s" },
        if hidden > 0 {
            format!(" · {hidden} archived hidden")
        } else {
            String::new()
        }
    )));

    for b in buckets {
        let key = b.place.key.clone();
        let open = st.expanded.contains(&key);
        items.push(Item::ProjectHead {
            key: key.clone(),
            emoji: b.place.emoji.clone(),
            label: b.place.label.clone(),
            archived: archived.contains(key.as_str()),
            count: b.count(),
            open,
        });
        if open {
            push_body(items, b);
        }
    }
    for p in empties {
        let open = st.expanded.contains(&p.key);
        items.push(Item::ProjectHead {
            key: p.key.clone(),
            emoji: p.emoji.clone(),
            label: p.display_name.clone(),
            archived: p.archived,
            count: 0,
            open,
        });
        if open {
            items.push(Item::Note("    No conversations".into()));
        }
    }
}

/// `1.2M`, `340k` — four token classes summed, for a column of figures.
pub fn fmt_tokens(n: u64) -> String {
    match n {
        0 => "0".into(),
        n if n >= 1_000_000_000 => format!("{:.1}B", n as f64 / 1e9),
        n if n >= 1_000_000 => format!("{:.1}M", n as f64 / 1e6),
        n if n >= 1_000 => format!("{}k", (n as f64 / 1e3).round() as u64),
        n => n.to_string(),
    }
}

pub fn fmt_cost(v: f64) -> String {
    if v >= 100.0 {
        format!("${}", v.round() as u64)
    } else {
        format!("${v:.2}")
    }
}

/// Strip the vendor prefix so the column stays narrow.
pub fn short_model(m: &str) -> &str {
    m.strip_prefix("claude-").unwrap_or(m)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::serve::web_types::WindowView;

    fn win(
        idx: &str,
        name: &str,
        tier: u8,
        rank: Option<u32>,
        status: SessionStatus,
    ) -> WindowView {
        serde_json::from_value(serde_json::json!({
            "pane_id": format!("%{idx}"), "window_index": idx, "window_name": name,
            "status": status, "cpu": 0.0, "mem_kb": 0, "ports": [], "cwd": null,
            "last_activity": "2026-10-19T10:00:00+00:00", "pane": null,
            "attention": tier, "attention_rank": rank, "state_secs": 120,
        }))
        .unwrap()
    }

    fn session(name: &str, skipped: bool, windows: Vec<WindowView>) -> SessionView {
        let mut v: SessionView = serde_json::from_value(serde_json::json!({
            "name": name, "status": null, "cpu": 0.0, "mem_kb": 0, "ports": [],
            "processes": [], "cwd": null, "last_activity": null, "attached": false,
            "pane": null, "windows": [], "skipped": skipped,
        }))
        .unwrap();
        v.windows = windows;
        v
    }

    fn project(key: &str, name: &str, archived: bool) -> ProjectItem {
        ProjectItem {
            key: key.into(),
            emoji: "📁".into(),
            display_name: name.into(),
            session_name: format!("📁 {name}"),
            archived,
        }
    }

    fn frozen(session: &str, note: &str) -> FrozenItem {
        FrozenItem {
            key: note.into(),
            session_name: session.into(),
            window_label: "w".into(),
            note: note.into(),
            relative: "4d ago".into(),
        }
    }

    fn now() -> chrono::DateTime<chrono::Utc> {
        "2026-10-19T10:30:00Z".parse().unwrap()
    }

    fn titles(items: &[Item], convs: &[Conv]) -> Vec<String> {
        items
            .iter()
            .filter_map(|i| match i {
                Item::Conv { idx, .. } => Some(convs[*idx].title.clone()),
                _ => None,
            })
            .collect()
    }

    fn state<'a>(view: View, expanded: &'a HashSet<String>) -> ListState<'a> {
        ListState {
            view,
            skipped_open: true,
            frozen_open: true,
            show_archived: false,
            expanded,
        }
    }

    #[test]
    fn attention_follows_the_server_rank_across_sessions() {
        let s = vec![
            session(
                "🐝 hive",
                false,
                vec![win("1", "a", 2, Some(2), SessionStatus::Waiting)],
            ),
            session(
                "📁 other",
                false,
                vec![
                    win("1", "b", 0, Some(0), SessionStatus::QuestionAsked),
                    win("2", "c", 3, Some(3), SessionStatus::Working),
                ],
            ),
            session(
                "🌳 x",
                false,
                vec![win("1", "d", 1, Some(1), SessionStatus::Waiting)],
            ),
        ];
        let convs = conv_rows(&s, &Projects::default(), now());
        let ex = HashSet::new();
        let items = build_items(
            &state(View::Attention, &ex),
            &convs,
            &[],
            &Projects::default(),
        );
        assert_eq!(titles(&items, &convs), ["b", "d", "a", "c"]);
        let tiers: Vec<u8> = items
            .iter()
            .filter_map(|i| match i {
                Item::Tier { tier, .. } => Some(*tier),
                _ => None,
            })
            .collect();
        assert_eq!(tiers, [0, 1, 2, 3]);
    }

    #[test]
    fn live_groups_by_session_in_server_order() {
        let s = vec![
            session(
                "🐝 hive",
                false,
                vec![
                    win("1", "a", 2, Some(1), SessionStatus::Waiting),
                    win("2", "b", 2, Some(2), SessionStatus::Waiting),
                ],
            ),
            session(
                "📁 other",
                false,
                vec![win("1", "c", 2, Some(0), SessionStatus::Waiting)],
            ),
        ];
        let convs = conv_rows(&s, &Projects::default(), now());
        let ex = HashSet::new();
        let items = build_items(&state(View::Live, &ex), &convs, &[], &Projects::default());
        assert_eq!(titles(&items, &convs), ["a", "b", "c"]);
        assert!(matches!(&items[0], Item::Group { label, .. } if label == "hive"));
    }

    // Skipped windows have no rank, so the attention view leaves them to the
    // Skipped section rather than dropping them.
    #[test]
    fn skipped_and_frozen_fold_below_and_nest_by_project_then_worktree() {
        let s = vec![
            session(
                "🐝 hive",
                false,
                vec![win("1", "live", 2, Some(0), SessionStatus::Waiting)],
            ),
            session(
                "📊 [ava] wt-a",
                true,
                vec![win("1", "s1", 2, None, SessionStatus::Waiting)],
            ),
            session(
                "📊 [ava] wt-b",
                true,
                vec![win("1", "s2", 2, None, SessionStatus::Waiting)],
            ),
        ];
        let convs = conv_rows(&s, &Projects::default(), now());
        let fz = vec![frozen("📊 [ava] wt-a", "parked")];
        let ex = HashSet::new();
        let items = build_items(
            &state(View::Attention, &ex),
            &convs,
            &fz,
            &Projects::default(),
        );
        assert_eq!(titles(&items, &convs), ["live", "s1", "s2"]);
        assert!(items.contains(&Item::Section {
            sect: Sect::Skipped,
            n: 2,
            open: true
        }));
        assert!(items.contains(&Item::Worktree("wt-a".into())));
        assert!(items.contains(&Item::Worktree("wt-b".into())));
        // One project group for both skipped worktrees.
        let groups = items
            .iter()
            .filter(|i| matches!(i, Item::Group { key, .. } if key == "ava"))
            .count();
        assert_eq!(groups, 2, "one under Skipped, one under Frozen");

        let closed = ListState {
            skipped_open: false,
            frozen_open: false,
            ..state(View::Attention, &ex)
        };
        let items = build_items(&closed, &convs, &fz, &Projects::default());
        assert_eq!(titles(&items, &convs), ["live"]);
        assert!(!items.iter().any(|i| matches!(i, Item::Frozen { .. })));
    }

    #[test]
    fn projects_view_puts_busy_projects_first_and_hides_idle_archived() {
        let projects = Projects::new(vec![
            project("zed", "Zed", false),
            project("alpha", "Alpha", false),
            project("old", "Old", true),
            project("busy-old", "BusyOld", true),
        ]);
        let s = vec![session(
            "📁 BusyOld",
            false,
            vec![win("1", "a", 2, Some(0), SessionStatus::Waiting)],
        )];
        let convs = conv_rows(&s, &projects, now());
        let mut ex = HashSet::new();
        let heads = |items: &[Item]| -> Vec<String> {
            items
                .iter()
                .filter_map(|i| match i {
                    Item::ProjectHead { key, .. } => Some(key.clone()),
                    _ => None,
                })
                .collect()
        };
        let items = build_items(&state(View::Projects, &ex), &convs, &[], &projects);
        assert_eq!(heads(&items), ["busy-old", "alpha", "zed"]);
        assert_eq!(
            items[0],
            Item::Note("3 projects · 1 archived hidden".into())
        );
        assert!(titles(&items, &convs).is_empty(), "collapsed until opened");

        ex.insert("busy-old".to_string());
        let shown = ListState {
            show_archived: true,
            ..state(View::Projects, &ex)
        };
        let items = build_items(&shown, &convs, &[], &projects);
        assert_eq!(heads(&items), ["busy-old", "alpha", "old", "zed"]);
        assert_eq!(titles(&items, &convs), ["a"]);
    }

    #[test]
    fn age_is_held_time_for_working_and_blocked_else_last_activity() {
        let s = vec![session(
            "s",
            false,
            vec![
                win("1", "busy", 3, Some(0), SessionStatus::Working),
                win("2", "idle", 2, Some(1), SessionStatus::Waiting),
            ],
        )];
        let convs = conv_rows(&s, &Projects::default(), now());
        assert_eq!(convs[0].age, "2m");
        assert_eq!(convs[1].age, "30m");
    }

    #[test]
    fn windowless_session_is_one_row_targeting_the_session() {
        let convs = conv_rows(
            &[session("📁 bare", false, vec![])],
            &Projects::default(),
            now(),
        );
        assert_eq!(convs.len(), 1);
        assert_eq!(convs[0].window_index, None);
        assert_eq!(convs[0].title, "bare");
    }

    #[test]
    fn numeric_window_names_get_a_readable_title() {
        let convs = conv_rows(
            &[session(
                "s",
                false,
                vec![win("3", "2.1.45", 2, Some(0), SessionStatus::Waiting)],
            )],
            &Projects::default(),
            now(),
        );
        assert_eq!(convs[0].title, "Window 3");
    }

    #[test]
    fn place_resolves_worktrees_projects_and_strangers() {
        let projects = Projects::new(vec![project("ava", "Avateen", false)]);
        let p = place_of("📊 [ava] live-avatar", &projects);
        assert_eq!(
            (p.key.as_str(), p.label.as_str(), p.branch.as_deref()),
            ("ava", "Avateen", Some("live-avatar"))
        );
        assert_eq!(
            place_of("📊 [ava]", &projects).branch.as_deref(),
            Some("(worktree)")
        );
        assert_eq!(place_of("📁 Avateen", &projects).key, "ava");
        let stranger = place_of("🐝 hive", &projects);
        assert_eq!(
            (stranger.label.as_str(), stranger.emoji.as_str()),
            ("hive", "🐝")
        );
        assert_eq!(p.line(), "📁 Avateen · live-avatar");
    }

    #[test]
    fn item_keys_survive_a_rebuild() {
        let convs = conv_rows(
            &[session(
                "s",
                false,
                vec![win("2", "a", 2, Some(0), SessionStatus::Waiting)],
            )],
            &Projects::default(),
            now(),
        );
        let item = Item::Conv {
            idx: 0,
            flat: false,
            indent: 0,
        };
        assert_eq!(item.key(&convs, &[]).as_deref(), Some("c:s:2"));
        assert!(Item::Blank.key(&convs, &[]).is_none());
        assert!(!Item::Note(String::new()).selectable());
    }

    #[test]
    fn formatting_matches_the_web_pane() {
        assert_eq!(fmt_tokens(0), "0");
        assert_eq!(fmt_tokens(999), "999");
        assert_eq!(fmt_tokens(340_400), "340k");
        assert_eq!(fmt_tokens(1_260_000), "1.3M");
        assert_eq!(fmt_cost(3.456), "$3.46");
        assert_eq!(fmt_cost(1234.4), "$1234");
        assert_eq!(short_model("claude-opus-5"), "opus-5");
        assert_eq!(
            state_word(&Some(SessionStatus::NeedsInput { reason: None })),
            "needs"
        );
        assert_eq!(dur(3600), "1h");
    }
}
