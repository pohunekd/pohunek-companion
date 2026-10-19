//! The sidebar's fuzzy filter: a query narrows presented sections to the
//! sessions whose fields match every typed token. Names, labels and badges
//! match as subsequences; identifiers (session id, Work id) match as
//! substrings, because a random-looking id contains almost any short
//! subsequence.

use crate::layout::GroupSection;
use crate::session::{ProjectRef, SessionRow};

/// Which project the sidebar is narrowed to.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum ProjectSelection {
    #[default]
    All,
    /// Sessions of this project; the label is the last one seen, kept so the
    /// chip stays readable after the project has no sessions.
    Project(ProjectRef),
    /// Sessions that belong to no project.
    NoProject,
}

impl ProjectSelection {
    fn admits(&self, row: &SessionRow) -> bool {
        match self {
            Self::All => true,
            Self::Project(wanted) => row.project.as_ref().is_some_and(|p| p.id == wanted.id),
            Self::NoProject => row.project.is_none(),
        }
    }
}

/// One chip of the project row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectChip {
    pub label: String,
    pub selection: ProjectSelection,
}

/// The project chips for the current sessions: "All", one chip per project
/// ordered by label ignoring case (then id), then "No project" when some
/// session has none. The selected project keeps its chip, with its last
/// label, even when no session belongs to it any more. Returns the chips and
/// the index of the selected one.
pub fn project_chips(
    rows: &[SessionRow],
    selected: &ProjectSelection,
) -> (Vec<ProjectChip>, usize) {
    let mut projects: Vec<ProjectRef> = Vec::new();
    for project in rows.iter().filter_map(|row| row.project.as_ref()) {
        if !projects.iter().any(|p| p.id == project.id) {
            projects.push(project.clone());
        }
    }
    if let ProjectSelection::Project(wanted) = selected
        && !projects.iter().any(|p| p.id == wanted.id)
    {
        projects.push(wanted.clone());
    }
    projects.sort_by_cached_key(|p| (lowercase_chars(&p.label), p.id.clone()));
    let mut chips = vec![ProjectChip {
        label: "All".to_owned(),
        selection: ProjectSelection::All,
    }];
    chips.extend(projects.into_iter().map(|p| ProjectChip {
        label: p.label.clone(),
        selection: ProjectSelection::Project(p),
    }));
    if rows.iter().any(|row| row.project.is_none()) || *selected == ProjectSelection::NoProject {
        chips.push(ProjectChip {
            label: "No project".to_owned(),
            selection: ProjectSelection::NoProject,
        });
    }
    let index = chips
        .iter()
        .position(|chip| match (&chip.selection, selected) {
            (ProjectSelection::Project(a), ProjectSelection::Project(b)) => a.id == b.id,
            (a, b) => a == b,
        })
        .unwrap_or(0);
    (chips, index)
}

/// A parsed filter query. The query is split on whitespace; a session matches
/// when every token matches at least one of its fields, case-insensitively:
/// as a subsequence of a text field, as a substring of an identifier field. An
/// empty query matches everything. A selected project narrows further: a
/// session must pass both.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionFilter {
    tokens: Vec<Vec<char>>,
    project: ProjectSelection,
}

impl SessionFilter {
    pub fn new(query: &str) -> Self {
        Self {
            tokens: query.split_whitespace().map(lowercase_chars).collect(),
            project: ProjectSelection::All,
        }
    }

    pub fn set_query(&mut self, query: &str) {
        self.tokens = Self::new(query).tokens;
    }

    pub fn set_project(&mut self, project: ProjectSelection) {
        self.project = project;
    }

    pub fn project(&self) -> &ProjectSelection {
        &self.project
    }

    pub fn is_active(&self) -> bool {
        !self.tokens.is_empty() || self.project != ProjectSelection::All
    }

    pub fn matches(&self, row: &SessionRow) -> bool {
        if !self.project.admits(row) {
            return false;
        }
        if self.tokens.is_empty() {
            return true;
        }
        let texts: Vec<Vec<char>> = [
            Some(row.display_name.as_str()),
            row.project.as_ref().map(|p| p.label.as_str()),
            Some(row.agent.as_str()),
            row.badge.as_ref().map(|b| b.link.as_str()),
        ]
        .into_iter()
        .flatten()
        .map(lowercase_chars)
        .collect();
        let identifiers: Vec<Vec<char>> = [
            Some(row.key.session_id.as_str()),
            row.work.as_ref().and_then(|w| w.id.as_deref()),
        ]
        .into_iter()
        .flatten()
        .map(lowercase_chars)
        .collect();
        self.tokens.iter().all(|token| {
            texts.iter().any(|field| is_subsequence(token, field))
                || identifiers.iter().any(|id| is_substring(token, id))
        })
    }
}

fn lowercase_chars(text: &str) -> Vec<char> {
    text.chars().flat_map(char::to_lowercase).collect()
}

fn is_substring(needle: &[char], haystack: &[char]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

fn is_subsequence(needle: &[char], haystack: &[char]) -> bool {
    let mut rest = haystack.iter();
    needle.iter().all(|n| rest.any(|h| h == n))
}

/// Keeps only the matching rows of each section, in their order. While the
/// filter is active, sections left without a row are dropped and the rest are
/// shown expanded so their matches are visible; an inactive filter returns the
/// sections unchanged.
pub fn filter_sections(sections: Vec<GroupSection>, filter: &SessionFilter) -> Vec<GroupSection> {
    if !filter.is_active() {
        return sections;
    }
    sections
        .into_iter()
        .filter_map(|mut section| {
            section.rows.retain(|row| filter.matches(row));
            section.collapsed = false;
            (!section.rows.is_empty()).then_some(section)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::badge::{WorkBadge, WorkFacts};
    use crate::needs_you::{NEEDS_YOU_ID, with_needs_you};
    use crate::owner_state::OwnerState;
    use crate::session::{ProjectRef, SessionKey};

    fn row(id: &str, name: &str) -> SessionRow {
        SessionRow {
            key: SessionKey {
                host: "local".into(),
                session_id: id.into(),
            },
            display_name: name.into(),
            agent: "shell".into(),
            state: "running".into(),
            owner_state: OwnerState::Working,
            waiting_since: None,
            badge: None,
            project: None,
            work: None,
            attach_refusal: None,
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

    fn ids(sections: &[GroupSection]) -> Vec<Vec<&str>> {
        sections
            .iter()
            .map(|s| s.rows.iter().map(|r| r.key.session_id.as_str()).collect())
            .collect()
    }

    #[test]
    fn matches_a_subsequence_ignoring_case_and_unicode_case() {
        let filter = SessionFilter::new("bta");
        assert!(filter.matches(&row("s-1", "beta-review")));
        assert!(!filter.matches(&row("s-2", "alpha-agent")));
        assert!(SessionFilter::new("ŽEL").matches(&row("s-3", "železná")));
        assert!(SessionFilter::new("žel").matches(&row("s-3", "ŽELEZNÁ")));
    }

    #[test]
    fn every_token_must_match_some_field_and_fields_may_differ() {
        let mut session = row("s-9", "docs");
        session.project = Some(ProjectRef {
            id: "p".into(),
            label: "proj-gamma".into(),
        });
        let work = WorkFacts {
            provider: Some("github".into()),
            kind: Some("pr".into()),
            id: Some("42".into()),
            role: None,
        };
        session.badge = WorkBadge::from_facts(&work);
        session.work = Some(work);
        assert!(SessionFilter::new("docs gam").matches(&session));
        assert!(SessionFilter::new("shell 42").matches(&session));
        assert!(SessionFilter::new("gh pr").matches(&session));
        assert!(SessionFilter::new("s-9").matches(&session));
        // An id matches only as a contiguous substring.
        assert!(!SessionFilter::new("s9").matches(&session));
        assert!(!SessionFilter::new("docs zzz").matches(&session));
        // Characters of one token never span two fields.
        assert!(!SessionFilter::new("dosh").matches(&session));
    }

    #[test]
    fn an_empty_or_blank_query_matches_everything() {
        for query in ["", "   ", "\t"] {
            let filter = SessionFilter::new(query);
            assert!(!filter.is_active());
            assert!(filter.matches(&row("s-1", "anything")));
        }
    }

    #[test]
    fn filter_sections_keeps_order_and_hides_empty_sections() {
        let sections = vec![
            section(
                "a",
                vec![
                    row("1", "beta-two"),
                    row("2", "alpha"),
                    row("3", "beta-one"),
                ],
            ),
            section("b", vec![row("4", "gamma")]),
            section("c", Vec::new()),
        ];
        let out = filter_sections(sections.clone(), &SessionFilter::new("beta"));
        assert_eq!(ids(&out), [vec!["1", "3"]]);
        assert!(filter_sections(sections.clone(), &SessionFilter::new("qqq")).is_empty());
        let mut collapsed = sections.clone();
        collapsed[1].collapsed = true;
        let out = filter_sections(collapsed, &SessionFilter::new("gamma"));
        assert_eq!(ids(&out), [vec!["4"]]);
        assert!(!out[0].collapsed);
        assert_eq!(
            filter_sections(sections.clone(), &SessionFilter::new(" ")),
            sections
        );
    }

    fn project_row(id: &str, name: &str, project: Option<(&str, &str)>) -> SessionRow {
        let mut r = row(id, name);
        r.project = project.map(|(id, label)| ProjectRef {
            id: id.into(),
            label: label.into(),
        });
        r
    }

    fn project_sel(id: &str, label: &str) -> ProjectSelection {
        ProjectSelection::Project(ProjectRef {
            id: id.into(),
            label: label.into(),
        })
    }

    fn sample() -> Vec<SessionRow> {
        vec![
            project_row("1", "build", Some(("p-b", "beta"))),
            project_row("2", "docs", Some(("p-a", "Alpha"))),
            project_row("3", "build-two", None),
            project_row("4", "docs-two", Some(("p-b", "beta"))),
        ]
    }

    fn matching(filter: &SessionFilter) -> Vec<String> {
        sample()
            .iter()
            .filter(|r| filter.matches(r))
            .map(|r| r.key.session_id.clone())
            .collect()
    }

    #[test]
    fn the_project_and_the_query_both_have_to_match() {
        let mut filter = SessionFilter::default();
        filter.set_project(project_sel("p-b", "beta"));
        assert!(filter.is_active());
        assert_eq!(matching(&filter), ["1", "4"]);
        filter.set_query("docs");
        assert_eq!(matching(&filter), ["4"]);
        filter.set_project(ProjectSelection::All);
        assert_eq!(matching(&filter), ["2", "4"]);
        filter.set_project(ProjectSelection::NoProject);
        filter.set_query("");
        assert_eq!(matching(&filter), ["3"]);
        filter.set_query("docs");
        assert!(matching(&filter).is_empty());
    }

    #[test]
    fn a_selected_project_without_sessions_filters_everything_out() {
        let mut filter = SessionFilter::default();
        filter.set_project(project_sel("p-gone", "gone"));
        assert!(matching(&filter).is_empty());
        let sections = vec![section("a", sample())];
        assert!(filter_sections(sections, &filter).is_empty());
    }

    #[test]
    fn chips_are_ordered_by_label_with_no_project_last() {
        let (chips, index) = project_chips(&sample(), &ProjectSelection::All);
        let labels: Vec<_> = chips.iter().map(|c| c.label.as_str()).collect();
        assert_eq!(labels, ["All", "Alpha", "beta", "No project"]);
        assert_eq!(index, 0);
        let (_, index) = project_chips(&sample(), &project_sel("p-b", "beta"));
        assert_eq!(index, 2);
        let with_project: Vec<_> = sample()
            .into_iter()
            .filter(|r| r.project.is_some())
            .collect();
        let (chips, _) = project_chips(&with_project, &ProjectSelection::All);
        assert_eq!(chips.last().map(|c| c.label.as_str()), Some("beta"));
    }

    #[test]
    fn the_selected_project_keeps_its_chip_after_its_sessions_are_gone() {
        let rows: Vec<_> = sample()
            .into_iter()
            .filter(|r| r.key.session_id != "2")
            .collect();
        let (chips, index) = project_chips(&rows, &project_sel("p-a", "Alpha"));
        let labels: Vec<_> = chips.iter().map(|c| c.label.as_str()).collect();
        assert_eq!(labels, ["All", "Alpha", "beta", "No project"]);
        assert_eq!(index, 1);
        let only_projects: Vec<_> = rows.into_iter().filter(|r| r.project.is_some()).collect();
        let (chips, index) = project_chips(&only_projects, &ProjectSelection::NoProject);
        assert_eq!(chips.last().map(|c| c.label.as_str()), Some("No project"));
        assert_eq!(index, chips.len() - 1);
    }

    #[test]
    fn the_needs_you_section_is_filtered_too() {
        let mut waiting = row("1", "beta-review");
        waiting.owner_state = OwnerState::NeedsOwner;
        let mut other = row("2", "alpha");
        other.owner_state = OwnerState::NeedsOwner;
        let sections = with_needs_you(vec![section("a", vec![waiting, other, row("3", "beta")])]);
        let out = filter_sections(sections, &SessionFilter::new("bet"));
        assert_eq!(out[0].id, NEEDS_YOU_ID);
        assert_eq!(ids(&out), [vec!["1"], vec!["3"]]);
    }
}
