//! The "Needs you" section: sessions that need the owner, lifted out of their
//! groups for display only.

use crate::layout::GroupSection;
use crate::owner_state::OwnerState;
use crate::tray_summary::needs_owner_order;

/// Id and display name of the leading section.
pub const NEEDS_YOU_ID: &str = "needs-you";
const NEEDS_YOU_NAME: &str = "Needs you";

/// Moves every session that needs the owner out of its section into a leading
/// "Needs you" section, ordered like the tray menu. The section is omitted when
/// nothing needs the owner. Works on any ordered sections, so every view can
/// use it; the layout store is never touched, so a lifted session returns to
/// its place once it no longer needs the owner.
pub fn with_needs_you(sections: Vec<GroupSection>) -> Vec<GroupSection> {
    let mut lifted = Vec::new();
    let mut rest = Vec::with_capacity(sections.len() + 1);
    for mut section in sections {
        let (needing, staying) = section
            .rows
            .into_iter()
            .partition(|row| row.owner_state == OwnerState::NeedsOwner);
        lifted.extend(needing);
        section.rows = staying;
        rest.push(section);
    }
    if lifted.is_empty() {
        return rest;
    }
    lifted.sort_by(needs_owner_order);
    let mut out = Vec::with_capacity(rest.len() + 1);
    out.push(GroupSection {
        id: NEEDS_YOU_ID.into(),
        name: NEEDS_YOU_NAME.into(),
        collapsed: false,
        rows: lifted,
    });
    out.extend(rest);
    out
}

#[cfg(test)]
mod tests {
    use time::OffsetDateTime;
    use time::format_description::well_known::Rfc3339;

    use super::*;
    use crate::session::{SessionKey, SessionRow};

    fn row(
        id: &str,
        name: &str,
        owner_state: OwnerState,
        waiting_since: Option<&str>,
    ) -> SessionRow {
        SessionRow {
            key: SessionKey {
                host: "local".into(),
                session_id: id.into(),
            },
            display_name: name.into(),
            agent: "claude".into(),
            state: "running".into(),
            owner_state,
            waiting_since: waiting_since
                .map(|t| OffsetDateTime::parse(t, &Rfc3339).expect("fixture timestamp")),
            badge: None,
            project: None,
            work: None,
            attach_refusal: None,
            can_stop: false,
            can_remove: false,
            open: crate::open::OpenTargets::default(),
        }
    }

    fn section(id: &str, rows: Vec<SessionRow>) -> GroupSection {
        GroupSection {
            id: id.into(),
            name: id.into(),
            collapsed: false,
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
    fn lifts_waiting_sessions_in_wait_then_name_order_and_keeps_the_rest_in_place() {
        let out = with_needs_you(vec![
            section(
                "a",
                vec![
                    row("1", "one", OwnerState::Working, None),
                    row("2", "zed", OwnerState::NeedsOwner, None),
                    row("3", "three", OwnerState::Idle, None),
                ],
            ),
            section(
                "b",
                vec![
                    row(
                        "4",
                        "beta",
                        OwnerState::NeedsOwner,
                        Some("2026-01-01T01:00:00Z"),
                    ),
                    row(
                        "5",
                        "alpha",
                        OwnerState::NeedsOwner,
                        Some("2026-01-01T00:00:00Z"),
                    ),
                ],
            ),
        ]);
        assert_eq!(out[0].id, NEEDS_YOU_ID);
        assert_eq!(ids(&out[0]), ["5", "4", "2"]);
        assert_eq!(ids(&out[1]), ["1", "3"]);
        assert!(out[2].rows.is_empty());
        assert_eq!(out.len(), 3);
    }

    #[test]
    fn every_session_appears_exactly_once_and_clearing_returns_it_to_its_place() {
        let rows = |state| {
            vec![section(
                "a",
                vec![
                    row("1", "one", OwnerState::Idle, None),
                    row("2", "two", state, None),
                    row("3", "three", OwnerState::Idle, None),
                ],
            )]
        };
        let lifted = with_needs_you(rows(OwnerState::NeedsOwner));
        let mut all: Vec<&str> = lifted.iter().flat_map(ids).collect();
        all.sort_unstable();
        assert_eq!(all, ["1", "2", "3"]);

        let cleared = with_needs_you(rows(OwnerState::Working));
        assert_eq!(cleared.len(), 1);
        assert_eq!(ids(&cleared[0]), ["1", "2", "3"]);
    }

    #[test]
    fn a_collapsed_group_still_lifts_its_waiting_sessions() {
        let mut collapsed = section(
            "a",
            vec![
                row("1", "one", OwnerState::Working, None),
                row("2", "two", OwnerState::NeedsOwner, None),
            ],
        );
        collapsed.collapsed = true;
        let out = with_needs_you(vec![collapsed]);
        assert_eq!(ids(&out[0]), ["2"]);
        assert!(out[1].collapsed);
        assert_eq!(ids(&out[1]), ["1"]);
    }

    #[test]
    fn section_is_omitted_when_nothing_needs_the_owner() {
        let out = with_needs_you(vec![section(
            "a",
            vec![row("1", "one", OwnerState::Working, None)],
        )]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, "a");
        assert!(with_needs_you(Vec::new()).is_empty());
    }
}
