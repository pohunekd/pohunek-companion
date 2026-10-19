//! The sidebar views: how the sessions are sectioned. My groups is the owner's
//! editable layout; the others are read-only derivations of it.

use crate::layout::GroupSection;
use crate::needs_you::with_needs_you;
use crate::owner_state::OwnerState;

/// Which sectioning the sidebar shows. Adding a view is one variant, one entry
/// in [`View::ALL`], and one arm in each match below.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum View {
    #[default]
    MyGroups,
    ByState,
}

impl View {
    /// Every view in selector order.
    pub const ALL: [View; 2] = [View::MyGroups, View::ByState];

    /// Value stored in `layout.toml`.
    pub fn id(self) -> &'static str {
        match self {
            Self::MyGroups => "my-groups",
            Self::ByState => "by-state",
        }
    }

    /// Selector label.
    pub fn label(self) -> &'static str {
        match self {
            Self::MyGroups => "My groups",
            Self::ByState => "By state",
        }
    }

    pub fn parse(id: &str) -> Option<View> {
        Self::ALL.into_iter().find(|view| view.id() == id)
    }

    /// Position in [`View::ALL`].
    pub fn index(self) -> usize {
        Self::ALL
            .iter()
            .position(|view| *view == self)
            .unwrap_or_default()
    }

    /// Whether the owner can rearrange sessions in this view.
    pub fn is_editable(self) -> bool {
        matches!(self, Self::MyGroups)
    }

    /// Sections for display, from the owner's arranged groups. The Needs-you
    /// lift applies in every view.
    pub fn sections(self, arranged: Vec<GroupSection>) -> Vec<GroupSection> {
        match self {
            Self::MyGroups => with_needs_you(arranged),
            Self::ByState => by_state(arranged),
        }
    }
}

/// The sections of the By state view, in display order after Needs you.
const STATE_SECTIONS: [(OwnerState, &str); 4] = [
    (OwnerState::Working, "working"),
    (OwnerState::Idle, "idle"),
    (OwnerState::Ended, "ended"),
    (OwnerState::NoAgentState, "no-agent-state"),
];

/// Regroups the arranged sessions by owner state: Needs you, Working, Idle,
/// Ended, No agent state. Inside a section sessions keep their My groups order
/// (groups in order, then items in order); empty sections are omitted.
pub fn by_state(arranged: Vec<GroupSection>) -> Vec<GroupSection> {
    let all = GroupSection {
        id: String::new(),
        name: String::new(),
        rows: arranged.into_iter().flat_map(|s| s.rows).collect(),
    };
    let mut lifted = with_needs_you(vec![all]);
    let rest = lifted.pop().map(|s| s.rows).unwrap_or_default();
    let mut out = lifted;
    for (state, id) in STATE_SECTIONS {
        let rows: Vec<_> = rest
            .iter()
            .filter(|row| row.owner_state == state)
            .cloned()
            .collect();
        if !rows.is_empty() {
            out.push(GroupSection {
                id: id.to_owned(),
                name: state.label().to_owned(),
                rows,
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::needs_you::NEEDS_YOU_ID;
    use crate::session::{SessionKey, SessionRow};

    fn row(id: &str, owner_state: OwnerState) -> SessionRow {
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
        }
    }

    fn group(id: &str, rows: Vec<SessionRow>) -> GroupSection {
        GroupSection {
            id: id.into(),
            name: id.into(),
            rows,
        }
    }

    fn ids(section: &GroupSection) -> Vec<&str> {
        section
            .rows
            .iter()
            .map(|r| r.key.session_id.as_str())
            .collect()
    }

    #[test]
    fn by_state_places_every_session_once_in_state_order_keeping_group_order() {
        use OwnerState::*;
        let out = by_state(vec![
            group(
                "a",
                vec![row("1", Idle), row("2", NoAgentState), row("3", Working)],
            ),
            group(
                "b",
                vec![row("4", NeedsOwner), row("5", Ended), row("6", Idle)],
            ),
            group("c", vec![row("7", Working)]),
        ]);
        let sections: Vec<(&str, Vec<&str>)> =
            out.iter().map(|s| (s.id.as_str(), ids(s))).collect();
        assert_eq!(
            sections,
            [
                (NEEDS_YOU_ID, vec!["4"]),
                ("working", vec!["3", "7"]),
                ("idle", vec!["1", "6"]),
                ("ended", vec!["5"]),
                ("no-agent-state", vec!["2"]),
            ]
        );
    }

    #[test]
    fn by_state_omits_empty_sections() {
        let out = by_state(vec![group("a", vec![row("1", OwnerState::Idle)])]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, "idle");
        assert!(by_state(Vec::new()).is_empty());
    }

    #[test]
    fn view_ids_round_trip_and_unknown_ids_do_not_parse() {
        for view in View::ALL {
            assert_eq!(View::parse(view.id()), Some(view));
            assert_eq!(View::ALL[view.index()], view);
        }
        assert_eq!(View::parse("by-project"), None);
    }
}
