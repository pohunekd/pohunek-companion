//! Sub-sessions: which session a row folds under, and the fold of the
//! displayed sections.
//!
//! A row's parent, in this order: a `work.role=manager` session is always a
//! root; else the session its `spawned_by` names on the same host; else the
//! single `work.role=manager` session of the same host whose `manager` value
//! equals the row's; else none (a root, or an orphan whose spawner is gone).
//! Only the daemon's own facts are read: `spawned_by` and session metadata.

use std::collections::{BTreeSet, HashMap, HashSet};

use pohunek_client::protocol::SessionInfo;

use crate::layout::GroupSection;
use crate::needs_you::NEEDS_YOU_ID;
use crate::session::{SessionKey, SessionRow};

/// Metadata key of a session's Pohunek Work role.
const WORK_ROLE_KEY: &str = "work.role";
/// The `work.role` of a manager session, always a root.
const MANAGER_ROLE: &str = "manager";
/// Metadata key that ties a session to its manager when `spawned_by` cannot.
const MANAGER_KEY: &str = "manager";

/// The facts of a session that place it in the sub-session tree.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Lineage {
    /// Id of the session whose request created this one.
    pub spawned_by: Option<String>,
    /// The session's `work.role` is `manager`.
    pub is_manager: bool,
    /// The session's `manager` metadata value; `None` when absent or empty.
    pub manager: Option<String>,
}

impl Lineage {
    pub(crate) fn from_info(info: &SessionInfo) -> Self {
        Self {
            spawned_by: info
                .spawned_by
                .as_ref()
                .map(|spawned| spawned.session_id.0.clone()),
            is_manager: info
                .metadata
                .get(WORK_ROLE_KEY)
                .is_some_and(|role| role.trim() == MANAGER_ROLE),
            manager: info
                .metadata
                .get(MANAGER_KEY)
                .map(|value| value.trim())
                .filter(|value| !value.is_empty())
                .map(str::to_owned),
        }
    }
}

/// Where a displayed row sits in the sub-session fold.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TreePosition {
    /// Levels below the root the row is shown under; 0 for a row in its own
    /// place.
    pub depth: u32,
    /// Number of direct sub-sessions.
    pub sub_sessions: usize,
    /// The row's sub-sessions are shown under it.
    pub expanded: bool,
    /// Sub-sessions are folded, so the row's sub-session control expands and
    /// collapses them.
    pub folding: bool,
}

/// The parent of every row that has one, over the rows of all hosts.
#[derive(Debug, Clone, Default)]
pub struct SubSessionTree {
    parents: HashMap<SessionKey, SessionKey>,
    children: HashMap<SessionKey, Vec<SessionKey>>,
    ambiguous_managers: BTreeSet<(String, String)>,
}

impl SubSessionTree {
    /// Resolves every row's parent. Parents are looked up on the row's own
    /// host only; a `spawned_by` cycle makes each of its sessions a root.
    pub fn build(rows: &[SessionRow]) -> Self {
        let keys: HashSet<&SessionKey> = rows.iter().map(|row| &row.key).collect();
        let mut managers: HashMap<(&str, &str), Vec<&SessionKey>> = HashMap::new();
        for row in rows.iter().filter(|row| row.lineage.is_manager) {
            if let Some(manager) = row.lineage.manager.as_deref() {
                managers
                    .entry((row.key.host.as_str(), manager))
                    .or_default()
                    .push(&row.key);
            }
        }
        let mut ambiguous_managers = BTreeSet::new();
        let mut parents = HashMap::new();
        for row in rows {
            if row.lineage.is_manager {
                continue;
            }
            let spawner = row.lineage.spawned_by.as_ref().map(|id| SessionKey {
                host: row.key.host.clone(),
                session_id: id.clone(),
            });
            let parent = match spawner.filter(|key| keys.contains(key)) {
                Some(key) => Some(key),
                None => row.lineage.manager.as_deref().and_then(|manager| {
                    match managers.get(&(row.key.host.as_str(), manager))?.as_slice() {
                        [only] => Some((*only).clone()),
                        _ => {
                            ambiguous_managers.insert((row.key.host.clone(), manager.to_owned()));
                            None
                        }
                    }
                }),
            };
            if let Some(parent) = parent.filter(|parent| *parent != row.key) {
                parents.insert(row.key.clone(), parent);
            }
        }
        break_cycles(&mut parents);
        let mut children: HashMap<SessionKey, Vec<SessionKey>> = HashMap::new();
        for row in rows {
            if let Some(parent) = parents.get(&row.key) {
                children
                    .entry(parent.clone())
                    .or_default()
                    .push(row.key.clone());
            }
        }
        Self {
            parents,
            children,
            ambiguous_managers,
        }
    }

    /// The session `key` folds under; `None` for a root.
    pub fn parent(&self, key: &SessionKey) -> Option<&SessionKey> {
        self.parents.get(key)
    }

    /// Direct sub-sessions of `key`, in the order of the rows `build` saw.
    pub fn children(&self, key: &SessionKey) -> &[SessionKey] {
        self.children
            .get(key)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    /// `(host, manager value)` pairs that more than one manager session of
    /// the host carries, so the `manager` fallback placed nothing under them.
    pub fn ambiguous_managers(&self) -> &BTreeSet<(String, String)> {
        &self.ambiguous_managers
    }
}

/// Removes the parent link of every session on a cycle, so each becomes a root
/// and no walk up the tree can loop.
fn break_cycles(parents: &mut HashMap<SessionKey, SessionKey>) {
    let mut settled: HashSet<SessionKey> = HashSet::new();
    let mut on_cycle: Vec<SessionKey> = Vec::new();
    let starts: Vec<SessionKey> = parents.keys().cloned().collect();
    for start in starts {
        let mut path: Vec<SessionKey> = Vec::new();
        let mut current = Some(start);
        while let Some(key) = current {
            if settled.contains(&key) {
                break;
            }
            if let Some(at) = path.iter().position(|seen| *seen == key) {
                on_cycle.extend(path[at..].iter().cloned());
                break;
            }
            current = parents.get(&key).cloned();
            path.push(key);
        }
        settled.extend(path);
    }
    for key in on_cycle {
        parents.remove(&key);
    }
}

/// Sets each row's sub-session count and, when `fold` is set, shows every
/// sub-session only under its parent: expanded parents (`expanded`) are
/// followed by their sub-sessions, indented by depth. Runs on the sections as
/// they are displayed (after the Needs-you lift and the filter): a sub-session
/// whose parent is not among the rows of the other sections (lifted into
/// Needs you, or filtered out) is shown in its own place, so no session is
/// hidden behind a parent that is not there to expand. The Needs-you section
/// is left as it is, so a sub-session that needs the owner stays lifted there
/// and is not shown a second time under its parent; its rows offer no expand
/// control. A section that only the fold emptied is dropped; a section that
/// was empty before stays.
pub fn fold_sections(
    sections: Vec<GroupSection>,
    tree: &SubSessionTree,
    expanded: &HashSet<SessionKey>,
    fold: bool,
) -> Vec<GroupSection> {
    let mut placed: HashMap<SessionKey, SessionRow> = HashMap::new();
    let mut order: HashMap<SessionKey, usize> = HashMap::new();
    for row in sections
        .iter()
        .filter(|section| section.id != NEEDS_YOU_ID)
        .flat_map(|section| section.rows.iter())
    {
        let next = order.len();
        order.entry(row.key.clone()).or_insert(next);
        placed.entry(row.key.clone()).or_insert_with(|| row.clone());
    }
    let position = |row: &SessionRow| TreePosition {
        depth: 0,
        sub_sessions: tree.children(&row.key).len(),
        expanded: false,
        folding: false,
    };
    let fold_context = Fold {
        tree,
        expanded,
        placed: &placed,
        order: &order,
    };
    sections
        .into_iter()
        .filter_map(|mut section| {
            if section.id == NEEDS_YOU_ID || !fold {
                for row in &mut section.rows {
                    row.tree = position(row);
                }
                return Some(section);
            }
            let before = section.rows.len();
            let mut rows = Vec::with_capacity(before);
            let mut visited = HashSet::new();
            for row in section.rows.into_iter().filter(|row| {
                tree.parent(&row.key)
                    .is_none_or(|parent| !placed.contains_key(parent))
            }) {
                fold_context.push(row, 0, &mut rows, &mut visited);
            }
            if before > 0 && rows.is_empty() {
                return None;
            }
            section.rows = rows;
            Some(section)
        })
        .collect()
}

/// The facts one fold pass shares while it lays out the rows.
struct Fold<'a> {
    tree: &'a SubSessionTree,
    expanded: &'a HashSet<SessionKey>,
    /// Rows of the sections other than Needs you: the rows a sub-session can
    /// be shown under, and the only sub-sessions shown.
    placed: &'a HashMap<SessionKey, SessionRow>,
    order: &'a HashMap<SessionKey, usize>,
}

impl Fold<'_> {
    /// Appends `row` and, when it is expanded, its sub-sessions below it.
    fn push(
        &self,
        mut row: SessionRow,
        depth: u32,
        out: &mut Vec<SessionRow>,
        visited: &mut HashSet<SessionKey>,
    ) {
        if !visited.insert(row.key.clone()) {
            return;
        }
        let is_expanded = self.expanded.contains(&row.key);
        let key = row.key.clone();
        row.tree = TreePosition {
            depth,
            sub_sessions: self.tree.children(&key).len(),
            expanded: is_expanded,
            folding: true,
        };
        out.push(row);
        if !is_expanded {
            return;
        }
        let mut children: Vec<&SessionKey> = self
            .tree
            .children(&key)
            .iter()
            .filter(|child| self.placed.contains_key(*child))
            .collect();
        children.sort_by_key(|child| self.order.get(*child).copied().unwrap_or(usize::MAX));
        for child in children {
            if let Some(child_row) = self.placed.get(child) {
                self.push(child_row.clone(), depth + 1, out, visited);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::needs_you::with_needs_you;
    use crate::owner_state::OwnerState;

    fn key(host: &str, id: &str) -> SessionKey {
        SessionKey {
            host: host.into(),
            session_id: id.into(),
        }
    }

    fn row_on(host: &str, id: &str, lineage: Lineage) -> SessionRow {
        SessionRow {
            key: key(host, id),
            display_name: id.into(),
            agent: "shell".into(),
            state: "running".into(),
            owner_state: OwnerState::Working,
            waiting_since: None,
            badge: None,
            project: None,
            work: None,
            attach_refusal: None,
            can_stop: false,
            can_remove: false,
            resume: crate::ResumeOffer::NotApplicable,
            runtime_detail: String::new(),
            open: crate::open::OpenTargets::default(),
            host_label: None,
            host_unreachable: false,
            details: Default::default(),
            group: None,
            group_problem: None,
            external: false,
            lineage,
            tree: TreePosition::default(),
        }
    }

    fn row(id: &str, lineage: Lineage) -> SessionRow {
        row_on("local", id, lineage)
    }

    fn spawned(by: &str) -> Lineage {
        Lineage {
            spawned_by: Some(by.into()),
            ..Lineage::default()
        }
    }

    fn manager(value: &str) -> Lineage {
        Lineage {
            is_manager: true,
            manager: Some(value.into()),
            ..Lineage::default()
        }
    }

    fn worker(value: &str) -> Lineage {
        Lineage {
            manager: Some(value.into()),
            ..Lineage::default()
        }
    }

    fn parent_of(tree: &SubSessionTree, id: &str) -> Option<String> {
        tree.parent(&key("local", id))
            .map(|parent| parent.session_id.clone())
    }

    fn section(id: &str, rows: Vec<SessionRow>) -> GroupSection {
        GroupSection {
            id: id.into(),
            name: id.into(),
            collapsed: false,
            rows,
        }
    }

    fn shown(sections: &[GroupSection]) -> Vec<(String, Vec<(String, u32)>)> {
        sections
            .iter()
            .map(|section| {
                (
                    section.id.clone(),
                    section
                        .rows
                        .iter()
                        .map(|row| (row.key.session_id.clone(), row.tree.depth))
                        .collect(),
                )
            })
            .collect()
    }

    #[test]
    fn spawned_by_names_the_parent_on_the_same_host_only() {
        let rows = vec![
            row("p", Lineage::default()),
            row("c", spawned("p")),
            row_on("remote", "r", spawned("p")),
        ];
        let tree = SubSessionTree::build(&rows);
        assert_eq!(parent_of(&tree, "c").as_deref(), Some("p"));
        assert_eq!(tree.parent(&key("remote", "r")), None);
        assert_eq!(tree.children(&key("local", "p")), [key("local", "c")]);
    }

    #[test]
    fn a_manager_is_always_a_root() {
        let rows = vec![
            row("coordinator", Lineage::default()),
            row(
                "m",
                Lineage {
                    spawned_by: Some("coordinator".into()),
                    ..manager("k")
                },
            ),
            row("other", manager("k2")),
        ];
        let tree = SubSessionTree::build(&rows);
        assert_eq!(parent_of(&tree, "m"), None);
        assert_eq!(parent_of(&tree, "other"), None);
    }

    #[test]
    fn the_manager_key_is_the_fallback_and_spawned_by_wins() {
        let rows = vec![
            row("m", manager("k")),
            row("spawner", Lineage::default()),
            row("by-key", worker("k")),
            row(
                "by-spawn",
                Lineage {
                    spawned_by: Some("spawner".into()),
                    ..worker("k")
                },
            ),
            row(
                "gone-spawner",
                Lineage {
                    spawned_by: Some("removed".into()),
                    ..worker("k")
                },
            ),
        ];
        let tree = SubSessionTree::build(&rows);
        assert_eq!(parent_of(&tree, "by-key").as_deref(), Some("m"));
        assert_eq!(parent_of(&tree, "by-spawn").as_deref(), Some("spawner"));
        assert_eq!(parent_of(&tree, "gone-spawner").as_deref(), Some("m"));
    }

    #[test]
    fn two_managers_with_one_key_place_nothing() {
        let rows = vec![
            row("m1", manager("k")),
            row("m2", manager("k")),
            row("w", worker("k")),
        ];
        let tree = SubSessionTree::build(&rows);
        assert_eq!(parent_of(&tree, "w"), None);
        assert!(
            tree.ambiguous_managers()
                .contains(&("local".to_owned(), "k".to_owned()))
        );
    }

    #[test]
    fn an_absent_spawner_leaves_an_orphan_root() {
        let rows = vec![row("o", spawned("removed"))];
        assert_eq!(parent_of(&SubSessionTree::build(&rows), "o"), None);
    }

    #[test]
    fn a_cycle_makes_every_member_a_root_and_keeps_its_tail() {
        let rows = vec![
            row("a", spawned("b")),
            row("b", spawned("a")),
            row("tail", spawned("a")),
        ];
        let tree = SubSessionTree::build(&rows);
        assert_eq!(parent_of(&tree, "a"), None);
        assert_eq!(parent_of(&tree, "b"), None);
        assert_eq!(parent_of(&tree, "tail").as_deref(), Some("a"));
        let sections = vec![section("g", rows)];
        let folded = fold_sections(sections, &tree, &HashSet::new(), true);
        assert_eq!(
            shown(&folded),
            [("g".to_owned(), vec![("a".into(), 0), ("b".into(), 0)])]
        );
    }

    #[test]
    fn folding_hides_sub_sessions_until_their_parent_is_expanded() {
        let rows = vec![
            row("p", Lineage::default()),
            row("c1", spawned("p")),
            row("c2", spawned("p")),
            row("g", spawned("c1")),
            row("solo", Lineage::default()),
        ];
        let tree = SubSessionTree::build(&rows);
        let sections = vec![
            section("a", vec![rows[0].clone(), rows[2].clone()]),
            section("b", vec![rows[1].clone(), rows[3].clone(), rows[4].clone()]),
        ];
        let folded = fold_sections(sections.clone(), &tree, &HashSet::new(), true);
        assert_eq!(
            shown(&folded),
            [
                ("a".to_owned(), vec![("p".into(), 0)]),
                ("b".to_owned(), vec![("solo".into(), 0)]),
            ]
        );
        assert_eq!(folded[0].rows[0].tree.sub_sessions, 2);
        assert!(folded[0].rows[0].tree.folding);

        let expanded: HashSet<SessionKey> = [key("local", "p"), key("local", "c1")].into();
        let unfolded = fold_sections(sections, &tree, &expanded, true);
        // Sub-sessions follow their parent into its section, in display order.
        assert_eq!(
            shown(&unfolded),
            [
                (
                    "a".to_owned(),
                    vec![
                        ("p".into(), 0),
                        ("c2".into(), 1),
                        ("c1".into(), 1),
                        ("g".into(), 2),
                    ]
                ),
                ("b".to_owned(), vec![("solo".into(), 0)]),
            ]
        );
    }

    #[test]
    fn a_section_only_the_fold_emptied_is_dropped() {
        let rows = vec![row("p", Lineage::default()), row("c", spawned("p"))];
        let tree = SubSessionTree::build(&rows);
        let sections = vec![
            section("parents", vec![rows[0].clone()]),
            section("workers", vec![rows[1].clone()]),
            section("empty", Vec::new()),
        ];
        let folded = fold_sections(sections, &tree, &HashSet::new(), true);
        let ids: Vec<&str> = folded.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["parents", "empty"]);
    }

    #[test]
    fn without_folding_every_row_stays_and_parents_keep_their_count() {
        let rows = vec![row("p", Lineage::default()), row("c", spawned("p"))];
        let tree = SubSessionTree::build(&rows);
        let sections = vec![section("g", rows)];
        let unfolded = fold_sections(sections, &tree, &HashSet::new(), false);
        assert_eq!(
            shown(&unfolded),
            [("g".to_owned(), vec![("p".into(), 0), ("c".into(), 0)])]
        );
        assert_eq!(unfolded[0].rows[0].tree.sub_sessions, 1);
        assert!(!unfolded[0].rows[0].tree.folding);
    }

    #[test]
    fn a_folded_sub_session_that_needs_the_owner_stays_in_needs_you() {
        let mut child = row("c", spawned("p"));
        child.owner_state = OwnerState::NeedsOwner;
        let rows = vec![row("p", Lineage::default()), child];
        let tree = SubSessionTree::build(&rows);
        let lifted = with_needs_you(vec![section("g", rows)]);
        let expanded: HashSet<SessionKey> = [key("local", "p")].into();
        for expanded in [HashSet::new(), expanded] {
            let folded = fold_sections(lifted.clone(), &tree, &expanded, true);
            assert_eq!(
                shown(&folded),
                [
                    (NEEDS_YOU_ID.to_owned(), vec![("c".into(), 0)]),
                    ("g".to_owned(), vec![("p".into(), 0)]),
                ]
            );
        }
    }
    #[test]
    fn a_parent_lifted_into_needs_you_leaves_its_sub_sessions_in_place() {
        let mut parent = row("p", Lineage::default());
        parent.owner_state = OwnerState::NeedsOwner;
        let rows = vec![parent, row("c", spawned("p")), row("g", spawned("c"))];
        let tree = SubSessionTree::build(&rows);
        let lifted = with_needs_you(vec![section("a", rows)]);
        let folded = fold_sections(lifted, &tree, &HashSet::new(), true);
        assert_eq!(
            shown(&folded),
            [
                (NEEDS_YOU_ID.to_owned(), vec![("p".into(), 0)]),
                ("a".to_owned(), vec![("c".into(), 0)]),
            ]
        );
        assert!(!folded[0].rows[0].tree.folding);
        assert_eq!(folded[0].rows[0].tree.sub_sessions, 1);
        assert!(folded[1].rows[0].tree.folding);
    }

    #[test]
    fn a_parent_the_filter_removed_leaves_its_sub_sessions_in_place() {
        let rows = vec![
            row("p", Lineage::default()),
            row("c", spawned("p")),
            row("other", Lineage::default()),
        ];
        let tree = SubSessionTree::build(&rows);
        // `p` was filtered out before the fold; `c` and `other` match.
        let filtered = vec![section("a", vec![rows[1].clone(), rows[2].clone()])];
        let expanded: HashSet<SessionKey> = [key("local", "p")].into();
        let folded = fold_sections(filtered, &tree, &expanded, true);
        assert_eq!(
            shown(&folded),
            [("a".to_owned(), vec![("c".into(), 0), ("other".into(), 0)])]
        );
    }
}
