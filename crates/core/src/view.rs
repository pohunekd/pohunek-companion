//! The sidebar views: how the sessions are sectioned. My groups is the owner's
//! editable layout; the others are read-only derivations of it.

use std::collections::BTreeMap;

use crate::badge::clip_display;
use crate::layout::GroupSection;
use crate::needs_you::with_needs_you;
use crate::owner_state::OwnerState;
use crate::session::SessionRow;

/// Which sectioning the sidebar shows. Adding a view is one variant, one entry
/// in [`View::ALL`], and one arm in each match below.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum View {
    #[default]
    MyGroups,
    ByState,
    ByProject,
    ByWorkItem,
}

impl View {
    /// Every view in selector order.
    pub const ALL: [View; 4] = [
        View::MyGroups,
        View::ByState,
        View::ByProject,
        View::ByWorkItem,
    ];

    /// Value stored in `layout.toml`.
    pub fn id(self) -> &'static str {
        match self {
            Self::MyGroups => "my-groups",
            Self::ByState => "by-state",
            Self::ByProject => "by-project",
            Self::ByWorkItem => "by-work-item",
        }
    }

    /// Selector label.
    pub fn label(self) -> &'static str {
        match self {
            Self::MyGroups => "My groups",
            Self::ByState => "By state",
            Self::ByProject => "By project",
            Self::ByWorkItem => "By Work item",
        }
    }

    /// Short label for the chip row, which must fit the narrow sidebar.
    pub fn tab_label(self) -> &'static str {
        match self {
            Self::MyGroups => "Groups",
            Self::ByState => "State",
            Self::ByProject => "Project",
            Self::ByWorkItem => "Work",
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
    /// lift applies in every view; `title_max_chars` clips the titles of the
    /// sections a view derives from session metadata.
    pub fn sections(
        self,
        arranged: Vec<GroupSection>,
        title_max_chars: usize,
    ) -> Vec<GroupSection> {
        match self {
            Self::MyGroups => with_needs_you(arranged),
            Self::ByState => by_state(arranged),
            Self::ByProject => by_project(arranged, title_max_chars),
            Self::ByWorkItem => by_work_item(arranged, title_max_chars),
        }
    }
}

/// The sections of the By state view, in display order after Needs you.
const STATE_SECTIONS: [(OwnerState, &str); 5] = [
    (OwnerState::Working, "working"),
    (OwnerState::Idle, "idle"),
    (OwnerState::RuntimeProblem, "runtime-problem"),
    (OwnerState::Ended, "ended"),
    (OwnerState::NoAgentState, "no-agent-state"),
];

/// Regroups the arranged sessions by owner state: Needs you, Working, Idle,
/// Runtime problem, Ended, No agent state. Inside a section sessions keep their My groups order
/// (groups in order, then items in order); empty sections are omitted.
pub fn by_state(arranged: Vec<GroupSection>) -> Vec<GroupSection> {
    let all = GroupSection {
        id: String::new(),
        name: String::new(),
        collapsed: false,
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
                collapsed: false,
                rows,
            });
        }
    }
    out
}

/// Id of the trailing section of sessions without a project.
pub const NO_PROJECT_ID: &str = "no-project";
const NO_PROJECT_NAME: &str = "No project";
/// Id of the trailing section of sessions without a Work link.
pub const NO_WORK_ID: &str = "no-work-item";
const NO_WORK_NAME: &str = "No Work item";
/// Joins the parts of a section id; `sanitize` strips control characters from
/// metadata, so no part can contain it.
const ID_SEPARATOR: char = '\u{1f}';

/// Regroups the arranged sessions by project: Needs you, then one section per
/// project label (the same label on several hosts is one section) ordered by
/// title, then No project. Inside a section sessions keep
/// their My groups order.
pub fn by_project(arranged: Vec<GroupSection>, title_max_chars: usize) -> Vec<GroupSection> {
    by_key(
        arranged,
        title_max_chars,
        (NO_PROJECT_ID, NO_PROJECT_NAME),
        |row| {
            let project = row.project.as_ref()?;
            Some((
                format!("project{ID_SEPARATOR}{}", project.label),
                project.label.clone(),
            ))
        },
    )
}

/// Regroups the arranged sessions by linked Pohunek Work item: Needs you, then
/// one section per (provider, kind, id) ordered by title, then No Work item.
/// The role is not part of the key.
pub fn by_work_item(arranged: Vec<GroupSection>, title_max_chars: usize) -> Vec<GroupSection> {
    by_key(
        arranged,
        title_max_chars,
        (NO_WORK_ID, NO_WORK_NAME),
        |row| {
            let work = row.work.as_ref()?;
            let id = work.id.as_deref()?;
            let badge = row.badge.as_ref()?;
            let part = |value: &Option<String>| value.clone().unwrap_or_default();
            let key = format!(
                "work{ID_SEPARATOR}{}{ID_SEPARATOR}{}{ID_SEPARATOR}{id}",
                part(&work.provider),
                part(&work.kind),
            );
            Some((key, badge.link.clone()))
        },
    )
}

/// Buckets the sessions by the `(section id, title)` the key function gives;
/// sessions it gives `None` for go to the trailing `none` section. Sections
/// are ordered by title, then id; the Needs-you lift comes first.
fn by_key(
    arranged: Vec<GroupSection>,
    title_max_chars: usize,
    none: (&str, &str),
    key: impl Fn(&SessionRow) -> Option<(String, String)>,
) -> Vec<GroupSection> {
    let all = GroupSection {
        id: String::new(),
        name: String::new(),
        collapsed: false,
        rows: arranged.into_iter().flat_map(|s| s.rows).collect(),
    };
    let mut lifted = with_needs_you(vec![all]);
    let rest = lifted.pop().map(|s| s.rows).unwrap_or_default();
    let mut keyed: BTreeMap<String, (String, Vec<SessionRow>)> = BTreeMap::new();
    let mut unkeyed = Vec::new();
    for row in rest {
        match key(&row) {
            Some((id, title)) => keyed
                .entry(id)
                .or_insert_with(|| (title, Vec::new()))
                .1
                .push(row),
            None => unkeyed.push(row),
        }
    }
    let mut sections: Vec<_> = keyed.into_iter().collect();
    sections
        .sort_by_cached_key(|(id, (title, _))| (title.to_lowercase(), title.clone(), id.clone()));
    let mut out = lifted;
    out.extend(
        sections
            .into_iter()
            .map(|(id, (title, rows))| GroupSection {
                id,
                name: clip_display(&title, title_max_chars),
                collapsed: false,
                rows,
            }),
    );
    if !unkeyed.is_empty() {
        out.push(GroupSection {
            id: none.0.to_owned(),
            name: none.1.to_owned(),
            collapsed: false,
            rows: unkeyed,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::needs_you::NEEDS_YOU_ID;
    use crate::session::SessionKey;

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
        }
    }

    fn group(id: &str, rows: Vec<SessionRow>) -> GroupSection {
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
        assert_eq!(View::parse("by-herd"), None);
    }

    fn project_row(id: &str, project: Option<(&str, &str)>) -> SessionRow {
        SessionRow {
            project: project.map(|(id, label)| crate::session::ProjectRef {
                id: id.into(),
                label: label.into(),
            }),
            ..row(id, OwnerState::Working)
        }
    }

    fn work_row(id: &str, link: Option<(&str, &str, &str, &str)>) -> SessionRow {
        let work = link.map(|(provider, kind, work_id, role)| crate::WorkFacts {
            provider: Some(provider.into()),
            kind: Some(kind.into()),
            id: Some(work_id.into()),
            role: Some(role.into()),
        });
        SessionRow {
            badge: work.as_ref().and_then(crate::WorkBadge::from_facts),
            work,
            ..row(id, OwnerState::Working)
        }
    }

    fn named(out: &[GroupSection]) -> Vec<(&str, Vec<&str>)> {
        out.iter().map(|s| (s.name.as_str(), ids(s))).collect()
    }

    #[test]
    fn by_project_orders_sections_by_title_with_no_project_last() {
        let mut waiting = project_row("6", Some(("p-b", "beta")));
        waiting.owner_state = OwnerState::NeedsOwner;
        let out = by_project(
            vec![
                group(
                    "g1",
                    vec![
                        project_row("1", Some(("p-b", "beta"))),
                        project_row("2", None),
                        project_row("3", Some(("p-a", "Alpha"))),
                    ],
                ),
                group(
                    "g2",
                    vec![
                        project_row("4", Some(("p-b-other-host", "beta"))),
                        waiting,
                        project_row("5", None),
                    ],
                ),
            ],
            40,
        );
        assert_eq!(
            named(&out),
            [
                ("Needs you", vec!["6"]),
                ("Alpha", vec!["3"]),
                ("beta", vec!["1", "4"]),
                ("No project", vec!["2", "5"]),
            ]
        );
    }

    #[test]
    fn by_work_item_shares_a_section_across_roles_and_clips_titles() {
        let out = by_work_item(
            vec![group(
                "g",
                vec![
                    work_row("1", Some(("github", "pr", "12", "implementer"))),
                    work_row("2", None),
                    work_row("3", Some(("github", "pr", "12", "reviewer"))),
                    work_row("4", Some(("github", "issue", "7", "implementer"))),
                    work_row("5", Some(("linear", "issue", "ABCDEFGHIJKL-1", "x"))),
                ],
            )],
            12,
        );
        assert_eq!(
            named(&out),
            [
                ("GH #7", vec!["4"]),
                ("GH PR #12", vec!["1", "3"]),
                ("Linear ABCD…", vec!["5"]),
                ("No Work item", vec!["2"]),
            ]
        );
        let unique: std::collections::HashSet<_> = out.iter().map(|s| &s.id).collect();
        assert_eq!(unique.len(), out.len());
    }

    #[test]
    fn derived_views_with_nothing_to_group_are_empty() {
        assert!(by_project(Vec::new(), 40).is_empty());
        assert!(by_work_item(Vec::new(), 40).is_empty());
    }
}
