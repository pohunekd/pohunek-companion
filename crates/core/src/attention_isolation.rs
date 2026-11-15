//! Attention isolation: a session flagged with
//! `pohunek.attention.children=isolate` keeps the attention of the sessions it
//! spawned, directly or through others, from reaching the owner.
//!
//! The walk mirrors the daemon's rule: follow `spawned_by` upward from the
//! session (the session itself is skipped), the nearest flagged ancestor
//! isolates it, an ancestor missing from the host's sessions ends the walk
//! (not isolated), and a session seen twice ends it (not isolated). Only
//! `spawned_by` counts; the `manager` fallback of the sub-session tree does
//! not, because the daemon does not isolate through it.
//!
//! The daemon reports the result only on `session.list`, never on events, so
//! the companion walks the rows itself after every change instead of reading
//! that field.

use std::collections::{HashMap, HashSet};

use crate::session::SessionRow;
use crate::sub_sessions::Lineage;

/// Whether the host's daemon honours the isolation flag, as `host.inspect`
/// reported it for the current connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IsolationSupport {
    /// The daemon advertises `session_attention_isolation_supported`.
    Supported,
    /// The daemon predates the flag: it stores the key and ignores it.
    Unsupported,
    /// `host.inspect` failed; treated as unsupported, so no attention is
    /// hidden.
    Unknown,
}

impl IsolationSupport {
    pub fn is_supported(self) -> bool {
        self == Self::Supported
    }
}

/// The nearest flagged ancestor of every isolated session among one host's
/// sessions, by session id. Empty when the host does not support the flag.
pub fn isolating_ancestors<'a>(
    sessions: impl IntoIterator<Item = (&'a str, &'a Lineage)>,
    support: IsolationSupport,
) -> HashMap<String, String> {
    if !support.is_supported() {
        return HashMap::new();
    }
    let by_id: HashMap<&str, &Lineage> = sessions.into_iter().collect();
    by_id
        .iter()
        .filter_map(|(id, lineage)| {
            let ancestor = isolating_ancestor(id, lineage, &by_id)?;
            Some(((*id).to_owned(), ancestor.to_owned()))
        })
        .collect()
}

/// Sets each row's `attention_isolated_by`; `rows` are the sessions of one
/// host.
pub(crate) fn apply_isolation(rows: &mut [SessionRow], support: IsolationSupport) {
    let mut isolated = isolating_ancestors(
        rows.iter()
            .map(|row| (row.key.session_id.as_str(), &row.lineage)),
        support,
    );
    for row in rows {
        row.attention_isolated_by = isolated.remove(&row.key.session_id);
    }
}

fn isolating_ancestor<'a>(
    id: &'a str,
    lineage: &'a Lineage,
    by_id: &HashMap<&'a str, &'a Lineage>,
) -> Option<&'a str> {
    let mut visited: HashSet<&str> = HashSet::from([id]);
    let mut current = lineage.spawned_by.as_deref();
    while let Some(ancestor_id) = current {
        if !visited.insert(ancestor_id) {
            return None;
        }
        let (ancestor_id, ancestor) = by_id.get_key_value(ancestor_id)?;
        if ancestor.isolates_children {
            return Some(ancestor_id);
        }
        current = ancestor.spawned_by.as_deref();
    }
    None
}

/// How many sessions the daemon's `session.list` annotation and the walk
/// disagree on; `daemon` maps a session id to the daemon's
/// `attention_isolated_by`, absent meaning not isolated.
pub fn disagreements(daemon: &HashMap<String, String>, walk: &HashMap<String, String>) -> usize {
    let ids: HashSet<&String> = daemon.keys().chain(walk.keys()).collect();
    ids.into_iter()
        .filter(|id| daemon.get(*id) != walk.get(*id))
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::GroupSection;
    use crate::needs_you::{NEEDS_YOU_ID, with_needs_you};
    use crate::owner_state::OwnerState;
    use crate::session::SessionKey;
    use crate::sub_sessions::{SubSessionTree, fold_sections};
    use crate::tray_summary::TraySummary;
    use crate::view::View;
    use pohunek_client::protocol::{
        ATTENTION_CHILDREN_ISOLATE, ATTENTION_CHILDREN_METADATA_KEY, SessionInfo,
    };

    fn lineage(spawned_by: Option<&str>, isolates_children: bool) -> Lineage {
        Lineage {
            spawned_by: spawned_by.map(str::to_owned),
            isolates_children,
            ..Lineage::default()
        }
    }

    fn walk(sessions: &[(&str, Lineage)], support: IsolationSupport) -> Vec<(String, String)> {
        let mut out: Vec<_> =
            isolating_ancestors(sessions.iter().map(|(id, lineage)| (*id, lineage)), support)
                .into_iter()
                .collect();
        out.sort();
        out
    }

    fn pair(id: &str, by: &str) -> (String, String) {
        (id.to_owned(), by.to_owned())
    }

    #[test]
    fn a_flagged_parent_isolates_child_and_grandchild_but_not_itself() {
        let sessions = [
            ("p", lineage(None, true)),
            ("c", lineage(Some("p"), false)),
            ("g", lineage(Some("c"), false)),
            ("other", lineage(None, false)),
        ];
        assert_eq!(
            walk(&sessions, IsolationSupport::Supported),
            [pair("c", "p"), pair("g", "p")]
        );
    }

    #[test]
    fn the_nearest_flagged_ancestor_is_reported() {
        let sessions = [
            ("root", lineage(None, true)),
            ("mid", lineage(Some("root"), true)),
            ("leaf", lineage(Some("mid"), false)),
        ];
        assert_eq!(
            walk(&sessions, IsolationSupport::Supported),
            [pair("leaf", "mid"), pair("mid", "root")]
        );
    }

    #[test]
    fn a_manager_fallback_child_without_spawned_by_is_not_isolated() {
        let manager = Lineage {
            is_manager: true,
            manager: Some("k".into()),
            isolates_children: true,
            ..Lineage::default()
        };
        let worker = Lineage {
            manager: Some("k".into()),
            ..Lineage::default()
        };
        let sessions = [("m", manager), ("w", worker)];
        assert!(walk(&sessions, IsolationSupport::Supported).is_empty());
    }

    #[test]
    fn a_missing_ancestor_ends_the_walk() {
        let sessions = [
            ("p", lineage(None, true)),
            ("c", lineage(Some("gone"), false)),
        ];
        assert!(walk(&sessions, IsolationSupport::Supported).is_empty());
    }

    #[test]
    fn a_cycle_without_a_flag_isolates_nothing_and_ends() {
        let sessions = [
            ("a", lineage(Some("b"), false)),
            ("b", lineage(Some("a"), false)),
            ("self", lineage(Some("self"), true)),
        ];
        assert!(walk(&sessions, IsolationSupport::Supported).is_empty());
    }

    #[test]
    fn a_flag_on_a_cycle_isolates_the_members_it_reaches_first() {
        // The daemon's walk returns on the first flagged ancestor before it
        // would close the loop, so the flag's own node is isolated by the
        // flag through the other member.
        let sessions = [
            ("a", lineage(Some("b"), true)),
            ("b", lineage(Some("a"), false)),
        ];
        assert_eq!(
            walk(&sessions, IsolationSupport::Supported),
            [pair("b", "a")]
        );
    }

    #[test]
    fn without_the_capability_nothing_is_isolated() {
        let sessions = [("p", lineage(None, true)), ("c", lineage(Some("p"), false))];
        for support in [IsolationSupport::Unsupported, IsolationSupport::Unknown] {
            assert!(walk(&sessions, support).is_empty());
        }
    }

    #[test]
    fn only_the_exact_isolate_value_flags_a_session() {
        let info = |value: &str| -> SessionInfo {
            serde_json::from_value(serde_json::json!({
                "id": "p",
                "capabilities": {"resume": false, "fork": false},
                "agent": "claude",
                "agent_base": "claude",
                "cwd": "/tmp",
                "pid": 1,
                "runtime": {
                    "state": "live",
                    "runtime_generation": "1",
                    "worker_instance_id": "w-1",
                },
                "cols": 80,
                "rows": 24,
                "state": "running",
                "state_source": "screen",
                "created_at": "2026-01-01T00:00:00Z",
                "updated_at": "2026-01-01T00:00:00Z",
                "metadata": { ATTENTION_CHILDREN_METADATA_KEY: value },
            }))
            .expect("a minimal session decodes")
        };
        assert!(Lineage::from_info(&info(ATTENTION_CHILDREN_ISOLATE)).isolates_children);
        for other in ["Isolate", " isolate", "isolate ", "notify", ""] {
            let parent = Lineage::from_info(&info(other));
            assert!(!parent.isolates_children, "{other:?}");
            let sessions = [("p", parent), ("c", lineage(Some("p"), false))];
            assert!(walk(&sessions, IsolationSupport::Supported).is_empty());
        }
    }

    #[test]
    fn disagreements_count_sessions_either_side_marks_differently() {
        let map = |pairs: &[(&str, &str)]| -> HashMap<String, String> {
            pairs
                .iter()
                .map(|(id, by)| ((*id).to_owned(), (*by).to_owned()))
                .collect()
        };
        let daemon = map(&[("a", "p"), ("b", "p"), ("c", "q")]);
        let walk = map(&[("a", "p"), ("b", "x"), ("d", "p")]);
        assert_eq!(disagreements(&daemon, &walk), 3);
        assert_eq!(disagreements(&daemon, &daemon), 0);
    }

    fn session_row(id: &str, owner_state: OwnerState, lineage: Lineage) -> SessionRow {
        SessionRow {
            key: SessionKey {
                host: "local".into(),
                session_id: id.into(),
            },
            display_name: id.into(),
            agent: "claude".into(),
            state: "running".into(),
            owner_state,
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
            attention_isolated_by: None,
            tree: Default::default(),
        }
    }

    /// A parent (flagged or not) and its blocked child, isolation applied.
    fn blocked_child_under(flagged: bool) -> Vec<SessionRow> {
        let mut rows = vec![
            session_row("p", OwnerState::Working, lineage(None, flagged)),
            session_row("c", OwnerState::NeedsOwner, lineage(Some("p"), false)),
        ];
        apply_isolation(&mut rows, IsolationSupport::Supported);
        rows
    }

    fn needs_you_ids(sections: &[GroupSection]) -> Vec<String> {
        sections
            .iter()
            .filter(|section| section.id == NEEDS_YOU_ID)
            .flat_map(|section| section.rows.iter())
            .map(|row| row.key.session_id.clone())
            .collect()
    }

    fn group(rows: Vec<SessionRow>) -> Vec<GroupSection> {
        vec![GroupSection {
            id: "g".into(),
            name: "g".into(),
            collapsed: false,
            rows,
        }]
    }

    #[test]
    fn an_isolated_blocked_child_leaves_needs_you_and_the_tray_but_keeps_its_row() {
        let rows = blocked_child_under(true);
        assert_eq!(rows[1].attention_isolated_by.as_deref(), Some("p"));
        assert_eq!(rows[1].owner_state, OwnerState::NeedsOwner);
        for view in View::ALL {
            let sections = view.sections(group(rows.clone()), 40);
            assert!(needs_you_ids(&sections).is_empty(), "{view:?}");
            let shown: Vec<&str> = sections
                .iter()
                .flat_map(|section| section.rows.iter())
                .map(|row| row.key.session_id.as_str())
                .collect();
            assert!(shown.contains(&"c"), "{view:?} hides the isolated row");
        }
        let summary = TraySummary::from_rows(&rows, 0);
        assert_eq!(
            (summary.needs_owner, summary.idle, summary.working),
            (0, 0, 1)
        );
        assert_eq!(summary.urgent, Some(OwnerState::Working));
        assert!(summary.entries.is_empty());
    }

    #[test]
    fn an_isolated_child_counts_in_no_tray_state() {
        for state in [OwnerState::Idle, OwnerState::Working] {
            let mut rows = vec![
                session_row("p", OwnerState::Ended, lineage(None, true)),
                session_row("c", state, lineage(Some("p"), false)),
            ];
            apply_isolation(&mut rows, IsolationSupport::Supported);
            let summary = TraySummary::from_rows(&rows, 0);
            assert_eq!(
                (summary.needs_owner, summary.idle, summary.working),
                (0, 0, 0),
                "{state:?}"
            );
            assert_eq!(summary.urgent, None);
        }
    }

    #[test]
    fn a_blocked_child_of_an_unflagged_parent_stays_in_needs_you_and_the_tray() {
        let rows = blocked_child_under(false);
        assert_eq!(rows[1].attention_isolated_by, None);
        for view in View::ALL {
            let sections = view.sections(group(rows.clone()), 40);
            assert_eq!(needs_you_ids(&sections), ["c"], "{view:?}");
        }
        let summary = TraySummary::from_rows(&rows, 0);
        assert_eq!(summary.needs_owner, 1);
        assert_eq!(summary.entries[0].key.session_id, "c");
    }

    #[test]
    fn a_daemon_without_the_capability_isolates_nothing() {
        let mut rows = vec![
            session_row("p", OwnerState::Working, lineage(None, true)),
            session_row("c", OwnerState::NeedsOwner, lineage(Some("p"), false)),
        ];
        apply_isolation(&mut rows, IsolationSupport::Unsupported);
        assert_eq!(rows[1].attention_isolated_by, None);
        assert_eq!(TraySummary::from_rows(&rows, 0).needs_owner, 1);
    }

    fn marks(rows: &[SessionRow]) -> Vec<(String, usize)> {
        let tree = SubSessionTree::build(rows);
        rows.iter()
            .map(|row| (row.key.session_id.clone(), tree.needs_owner_below(&row.key)))
            .filter(|(_, count)| *count > 0)
            .collect()
    }

    #[test]
    fn a_blocked_grandchild_marks_its_parent_and_the_root() {
        let mut rows = vec![
            session_row("root", OwnerState::Working, lineage(None, false)),
            session_row("child", OwnerState::Working, lineage(Some("root"), false)),
            session_row(
                "grand",
                OwnerState::NeedsOwner,
                lineage(Some("child"), false),
            ),
            session_row(
                "other",
                OwnerState::NeedsOwner,
                lineage(Some("root"), false),
            ),
        ];
        apply_isolation(&mut rows, IsolationSupport::Supported);
        assert_eq!(
            marks(&rows),
            [("root".to_owned(), 2), ("child".to_owned(), 1)]
        );
    }

    #[test]
    fn an_isolated_blocked_child_marks_nothing() {
        let mut rows = vec![
            session_row("root", OwnerState::Working, lineage(None, false)),
            session_row("flagged", OwnerState::Working, lineage(Some("root"), true)),
            session_row("c", OwnerState::NeedsOwner, lineage(Some("flagged"), false)),
        ];
        apply_isolation(&mut rows, IsolationSupport::Supported);
        assert!(marks(&rows).is_empty());
    }

    #[test]
    fn a_fallback_linked_blocked_worker_marks_its_manager() {
        let manager = Lineage {
            is_manager: true,
            manager: Some("k".into()),
            isolates_children: true,
            ..Lineage::default()
        };
        let worker = Lineage {
            manager: Some("k".into()),
            ..Lineage::default()
        };
        let mut rows = vec![
            session_row("m", OwnerState::Working, manager),
            session_row("w", OwnerState::NeedsOwner, worker),
        ];
        apply_isolation(&mut rows, IsolationSupport::Supported);
        assert_eq!(rows[1].attention_isolated_by, None);
        assert_eq!(marks(&rows), [("m".to_owned(), 1)]);
    }

    #[test]
    fn the_mark_shows_folded_expanded_and_unfolded() {
        let rows = blocked_child_under(false);
        let tree = SubSessionTree::build(&rows);
        let parent = rows[0].key.clone();
        for (expanded, fold) in [
            (HashSet::new(), true),
            (HashSet::from([parent.clone()]), true),
            (HashSet::new(), false),
        ] {
            let sections =
                fold_sections(with_needs_you(group(rows.clone())), &tree, &expanded, fold);
            let shown = sections
                .iter()
                .flat_map(|section| section.rows.iter())
                .find(|row| row.key == parent)
                .expect("the parent is shown");
            assert_eq!(shown.tree.needs_owner_below, 1, "fold {fold}");
        }
    }
}
