//! The sidebar's fuzzy filter: a query narrows presented sections to the
//! sessions whose fields match every typed token. Names, labels and badges
//! match as subsequences; identifiers (session id, Work id) match as
//! substrings, because a random-looking id contains almost any short
//! subsequence.

use serde::{Deserialize, Serialize};

use crate::layout::GroupSection;
use crate::owner_state::OwnerState;
use crate::session::{ProjectRef, SessionRow};

/// Every owner state in chip order.
const STATE_ORDER: [OwnerState; 6] = [
    OwnerState::NeedsOwner,
    OwnerState::Working,
    OwnerState::Idle,
    OwnerState::RuntimeProblem,
    OwnerState::Ended,
    OwnerState::NoAgentState,
];

/// Value stored in `layout.toml` and sent back by the window for a state chip.
pub fn state_key(state: OwnerState) -> &'static str {
    match state {
        OwnerState::NeedsOwner => "needs-you",
        OwnerState::Working => "working",
        OwnerState::Idle => "idle",
        OwnerState::RuntimeProblem => "runtime-problem",
        OwnerState::Ended => "ended",
        OwnerState::NoAgentState => "no-agent-state",
    }
}

/// The state a chip key stands for.
pub fn state_from_key(key: &str) -> Option<OwnerState> {
    STATE_ORDER.into_iter().find(|s| state_key(*s) == key)
}

/// Chip label of a state.
fn state_chip_label(state: OwnerState) -> &'static str {
    match state {
        OwnerState::NeedsOwner => "Needs you",
        OwnerState::Working => "Working",
        OwnerState::Idle => "Idle",
        OwnerState::RuntimeProblem => "Problem",
        OwnerState::Ended => "Ended",
        OwnerState::NoAgentState => "No agent",
    }
}

/// The owner states a view shows; a session passes when its state is in the
/// set. The default shows every state except Ended.
#[derive(Debug, Clone)]
pub struct StateSelection(Vec<OwnerState>);

impl PartialEq for StateSelection {
    fn eq(&self, other: &Self) -> bool {
        STATE_ORDER
            .into_iter()
            .all(|s| self.contains(s) == other.contains(s))
    }
}

impl Eq for StateSelection {}

impl Default for StateSelection {
    fn default() -> Self {
        Self(
            STATE_ORDER
                .into_iter()
                .filter(|s| *s != OwnerState::Ended)
                .collect(),
        )
    }
}

impl StateSelection {
    pub fn contains(&self, state: OwnerState) -> bool {
        self.0.contains(&state)
    }

    /// Adds the state when absent, removes it when present.
    pub fn toggle(&mut self, state: OwnerState) {
        if let Some(at) = self.0.iter().position(|s| *s == state) {
            self.0.remove(at);
        } else {
            self.0.push(state);
        }
    }

    /// Whether every state passes, so the selection narrows nothing.
    fn is_all(&self) -> bool {
        STATE_ORDER.into_iter().all(|s| self.contains(s))
    }

    fn keys(&self) -> Vec<String> {
        STATE_ORDER
            .into_iter()
            .filter(|s| self.contains(*s))
            .map(|s| state_key(s).to_owned())
            .collect()
    }
}

/// One chip of the state row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateChip {
    pub label: &'static str,
    pub key: &'static str,
    pub selected: bool,
}

/// The state chips: Needs you, Working, Idle, Ended, plus Problem when some
/// session has a runtime problem and No agent when some session has no agent
/// state. Selection is multi-choice: one click toggles
/// one state.
pub fn state_chips(rows: &[SessionRow], selected: &StateSelection) -> Vec<StateChip> {
    STATE_ORDER
        .into_iter()
        .filter(|state| {
            !matches!(state, OwnerState::NoAgentState | OwnerState::RuntimeProblem)
                || rows.iter().any(|row| row.owner_state == *state)
        })
        .map(|state| StateChip {
            label: state_chip_label(state),
            key: state_key(state),
            selected: selected.contains(state),
        })
        .collect()
}

/// A view's stored filter: the selected states and project. Unknown state
/// keys are ignored; a project that no longer exists keeps filtering as an
/// empty project.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredFilter {
    /// State keys; absent selects the default states.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    states: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    project_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    project_label: Option<String>,
    /// Sessions without a project; ignored when `project_id` is set.
    #[serde(default, skip_serializing_if = "is_false")]
    no_project: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

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
    states: StateSelection,
}

impl SessionFilter {
    pub fn new(query: &str) -> Self {
        Self {
            tokens: query.split_whitespace().map(lowercase_chars).collect(),
            project: ProjectSelection::All,
            states: StateSelection::default(),
        }
    }

    /// A filter with no query and the project and states a view stored.
    pub fn from_stored(stored: &StoredFilter) -> Self {
        let mut filter = Self::default();
        if let Some(keys) = &stored.states {
            let parsed: Vec<OwnerState> = keys.iter().filter_map(|k| state_from_key(k)).collect();
            // A list of only unknown keys is a file from another version, not
            // an owner choice to hide everything.
            if !parsed.is_empty() || keys.is_empty() {
                filter.states = StateSelection(parsed);
                // A list saved before the Runtime problem state existed must
                // not hide it; the state is never persistently deselected.
                if !filter.states.contains(OwnerState::RuntimeProblem) {
                    filter.states.toggle(OwnerState::RuntimeProblem);
                }
            }
        }
        filter.project = match (&stored.project_id, stored.no_project) {
            (Some(id), _) => ProjectSelection::Project(ProjectRef {
                id: id.clone(),
                label: stored.project_label.clone().unwrap_or_else(|| id.clone()),
            }),
            (None, true) => ProjectSelection::NoProject,
            (None, false) => ProjectSelection::All,
        };
        filter
    }

    /// The project and states, as stored per view; the query is not stored.
    pub fn to_stored(&self) -> StoredFilter {
        let (project_id, project_label) = match &self.project {
            ProjectSelection::Project(p) => (Some(p.id.clone()), Some(p.label.clone())),
            _ => (None, None),
        };
        StoredFilter {
            states: (self.states != StateSelection::default()).then(|| self.states.keys()),
            project_id,
            project_label,
            no_project: self.project == ProjectSelection::NoProject,
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

    pub fn states(&self) -> &StateSelection {
        &self.states
    }

    pub fn toggle_state(&mut self, state: OwnerState) {
        self.states.toggle(state);
    }

    /// Whether a query or project narrows the list; section expansion and
    /// hiding empty sections follow this, not the state selection.
    pub fn is_searching(&self) -> bool {
        !self.tokens.is_empty() || self.project != ProjectSelection::All
    }

    /// Whether any part of the filter hides sessions.
    pub fn is_active(&self) -> bool {
        self.is_searching() || !self.states.is_all()
    }

    pub fn matches(&self, row: &SessionRow) -> bool {
        if !self.states.contains(row.owner_state) || !self.project.admits(row) {
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

/// Indices of the items that match every whitespace-separated token of
/// `query` as a case-insensitive subsequence, in their original order. An
/// empty query matches every item. Drives the New session dialog's searchable
/// selects.
pub fn fuzzy_matches<S: AsRef<str>>(query: &str, items: &[S]) -> Vec<usize> {
    let tokens: Vec<Vec<char>> = query.split_whitespace().map(lowercase_chars).collect();
    items
        .iter()
        .enumerate()
        .filter(|(_, item)| {
            let text = lowercase_chars(item.as_ref());
            tokens.iter().all(|token| is_subsequence(token, &text))
        })
        .map(|(index, _)| index)
        .collect()
}

/// Keeps only the matching rows of each section, in their order. While a
/// query or project is selected, sections left without a row are dropped and
/// the rest are shown expanded so their matches are visible. With only a state
/// selection, sections the filter emptied are dropped, sections that were
/// already empty stay, and collapsing is kept. An inactive filter returns the
/// sections unchanged.
pub fn filter_sections(sections: Vec<GroupSection>, filter: &SessionFilter) -> Vec<GroupSection> {
    if !filter.is_active() {
        return sections;
    }
    let searching = filter.is_searching();
    sections
        .into_iter()
        .filter_map(|mut section| {
            let before = section.rows.len();
            section.rows.retain(|row| filter.matches(row));
            if searching {
                section.collapsed = false;
            }
            (!section.rows.is_empty() || (!searching && before == 0)).then_some(section)
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
            can_stop: false,
            can_remove: false,
            runtime_detail: String::new(),
            open: crate::open::OpenTargets::default(),
            host_label: None,
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
    fn fuzzy_matches_keeps_order_and_ignores_case_and_blank_queries() {
        let items = ["Proj-Alpha", "proj-beta", "gamma", "delta-build"];
        assert_eq!(fuzzy_matches("", &items), [0, 1, 2, 3]);
        assert_eq!(fuzzy_matches("  ", &items), [0, 1, 2, 3]);
        assert_eq!(fuzzy_matches("PRB", &items), [1]);
        assert_eq!(fuzzy_matches("a", &items), [0, 1, 2, 3]);
        assert_eq!(fuzzy_matches("pj al", &items), [0]);
        assert!(fuzzy_matches("zzz", &items).is_empty());
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
            assert!(!filter.is_searching());
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

    #[test]
    fn the_default_states_hide_ended_and_a_state_must_pass_with_query_and_project() {
        let mut ended = project_row("5", "build-old", Some(("p-b", "beta")));
        ended.owner_state = OwnerState::Ended;
        let mut filter = SessionFilter::default();
        assert!(!filter.matches(&ended));
        assert!(filter.matches(&project_row("1", "build", Some(("p-b", "beta")))));
        filter.toggle_state(OwnerState::Ended);
        assert!(filter.matches(&ended));
        filter.set_project(project_sel("p-a", "Alpha"));
        assert!(!filter.matches(&ended));
        filter.set_project(project_sel("p-b", "beta"));
        filter.set_query("zzz");
        assert!(!filter.matches(&ended));
        filter.set_query("old");
        filter.toggle_state(OwnerState::Ended);
        assert!(!filter.matches(&ended));
    }

    #[test]
    fn a_state_only_filter_keeps_empty_and_collapsed_sections() {
        let mut ended = row("2", "old");
        ended.owner_state = OwnerState::Ended;
        let mut collapsed = section("c", vec![row("3", "live")]);
        collapsed.collapsed = true;
        let sections = vec![
            section("a", vec![ended]),
            section("b", Vec::new()),
            collapsed,
        ];
        let out = filter_sections(sections, &SessionFilter::default());
        assert_eq!(ids(&out), [vec![], vec!["3"]]);
        assert!(out[1].collapsed);
    }

    #[test]
    fn a_stored_filter_round_trips_and_unknown_values_load_gracefully() {
        let mut filter = SessionFilter::default();
        assert_eq!(filter.to_stored(), StoredFilter::default());
        filter.toggle_state(OwnerState::Ended);
        filter.toggle_state(OwnerState::Idle);
        filter.set_project(project_sel("p-b", "beta"));
        let restored = SessionFilter::from_stored(&filter.to_stored());
        assert_eq!(restored.states(), filter.states());
        assert_eq!(restored.project(), filter.project());

        let stale = StoredFilter {
            states: Some(vec!["banana".into()]),
            project_id: Some("p-gone".into()),
            ..StoredFilter::default()
        };
        let loaded = SessionFilter::from_stored(&stale);
        assert_eq!(loaded.states(), &StateSelection::default());
        assert!(matching(&loaded).is_empty());
    }

    #[test]
    fn a_stored_state_list_without_runtime_problem_still_shows_it() {
        let stored = StoredFilter {
            states: Some(vec!["working".into(), "idle".into()]),
            ..StoredFilter::default()
        };
        let loaded = SessionFilter::from_stored(&stored);
        assert!(loaded.states().contains(OwnerState::RuntimeProblem));
        assert!(!loaded.states().contains(OwnerState::Ended));
    }

    #[test]
    fn the_no_agent_chip_appears_only_when_needed() {
        let default = StateSelection::default();
        let labels = |rows: &[SessionRow]| -> Vec<&str> {
            state_chips(rows, &default)
                .iter()
                .map(|c| c.label)
                .collect()
        };
        assert_eq!(labels(&sample()), ["Needs you", "Working", "Idle", "Ended"]);
        let mut shell = row("9", "shell");
        shell.owner_state = OwnerState::NoAgentState;
        assert_eq!(labels(&[shell]).last(), Some(&"No agent"));
    }
}
