//! The facts the session detail view shows: a few grouped, copyable fields
//! taken from the snapshot the companion already has. A field with no value is
//! absent, so the view has no empty rows.

use std::path::Path;

use pohunek_client::protocol::SessionInfo;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::badge::{WorkFacts, sanitize};
use crate::session::ProjectRef;

/// One labelled, copyable value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetailField {
    pub label: &'static str,
    /// The complete value, as it is copied.
    pub value: String,
    /// The value is a Work link the view can open.
    pub link: bool,
}

/// A titled group of fields; never empty.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetailSection {
    pub title: &'static str,
    pub fields: Vec<DetailField>,
}

/// The grouped fields of one session, in display order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionDetails {
    pub sections: Vec<DetailSection>,
}

/// What the detail fields are read from.
pub(crate) struct DetailSource<'a> {
    pub info: &'a SessionInfo,
    pub host: &'a str,
    /// Display name of the remote host; `None` for the local one.
    pub host_label: Option<&'a str>,
    pub runtime_detail: &'a str,
    pub waiting_since: Option<OffsetDateTime>,
    pub project: Option<&'a ProjectRef>,
    pub work: Option<&'a WorkFacts>,
    /// The `http(s)` Work link, when the metadata has one.
    pub link: Option<&'a str>,
}

/// Local host value shown for a session of the local daemon.
const LOCAL_HOST_NAME: &str = "local";

/// Drops control and bidirectional-format characters but keeps spaces, which
/// are significant in paths.
fn plain(value: &str) -> String {
    value
        .chars()
        .filter(|c| !c.is_control() && *c != '\u{200B}' && !('\u{202A}'..='\u{202E}').contains(c))
        .collect()
}

fn path_text(path: &Path) -> String {
    plain(&path.to_string_lossy())
}

struct Section {
    title: &'static str,
    fields: Vec<DetailField>,
}

impl Section {
    fn new(title: &'static str) -> Self {
        Self {
            title,
            fields: Vec::new(),
        }
    }

    fn add(&mut self, label: &'static str, value: impl Into<Option<String>>) {
        self.push(label, value.into(), false);
    }

    fn push(&mut self, label: &'static str, value: Option<String>, link: bool) {
        if let Some(value) = value.filter(|value| !value.trim().is_empty()) {
            self.fields.push(DetailField { label, value, link });
        }
    }

    fn finish(self, into: &mut Vec<DetailSection>) {
        if !self.fields.is_empty() {
            into.push(DetailSection {
                title: self.title,
                fields: self.fields,
            });
        }
    }
}

fn timestamp(at: OffsetDateTime) -> Option<String> {
    at.format(&Rfc3339).ok()
}

impl SessionDetails {
    pub(crate) fn build(source: &DetailSource<'_>) -> Self {
        let info = source.info;
        let mut sections = Vec::new();

        let mut identity = Section::new("Identity");
        identity.add("Session id", sanitize(&info.id.0));
        identity.add(
            "Host",
            source
                .host_label
                .map_or_else(|| LOCAL_HOST_NAME.to_owned(), str::to_owned),
        );
        identity.add("Host route", source.host_label.map(|_| plain(source.host)));
        identity.finish(&mut sections);

        let mut agent = Section::new("Agent");
        agent.add("Agent", sanitize(&info.agent));
        let state = info.state.as_str();
        agent.add(
            "State",
            if source.runtime_detail.is_empty() {
                state.to_owned()
            } else {
                format!("{state} ({})", source.runtime_detail)
            },
        );
        agent.add("Waiting since", source.waiting_since.and_then(timestamp));
        agent.add(
            "Native session id",
            info.native_session_id.as_deref().map(sanitize),
        );
        agent.add(
            "Native session path",
            info.native_session_path.as_deref().map(plain),
        );
        agent.finish(&mut sections);

        let mut location = Section::new("Location");
        location.add("Working directory", path_text(&info.cwd));
        location.add(
            "Worktree",
            info.worktree_path
                .as_deref()
                .filter(|worktree| *worktree != info.cwd)
                .map(path_text),
        );
        location.add("Repository", info.repo.as_deref().map(path_text));
        location.add("Branch", info.branch.as_deref().map(sanitize));
        if let Some(project) = source.project {
            location.add("Project", project.label.clone());
            location.add(
                "Project id",
                Some(project.id.clone()).filter(|id| *id != project.label),
            );
        }
        location.finish(&mut sections);

        let mut work = Section::new("Work");
        if let Some(facts) = source.work {
            work.add("Provider", facts.provider.clone());
            work.add("Kind", facts.kind.clone());
            work.add("Id", facts.id.clone());
            work.add("Role", facts.role.clone());
        }
        work.push("Link", source.link.map(str::to_owned), true);
        work.finish(&mut sections);

        Self { sections }
    }

    /// A plain-text block of `label: value` lines under the session's name,
    /// for pasting into an issue or a chat.
    pub fn copy_all(&self, name: &str) -> String {
        let mut text = String::from(name);
        for field in self.sections.iter().flat_map(|section| &section.fields) {
            text.push('\n');
            text.push_str(field.label);
            text.push_str(": ");
            text.push_str(&field.value);
        }
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn details(sections: Vec<(&'static str, Vec<(&'static str, &str)>)>) -> SessionDetails {
        SessionDetails {
            sections: sections
                .into_iter()
                .map(|(title, fields)| DetailSection {
                    title,
                    fields: fields
                        .into_iter()
                        .map(|(label, value)| DetailField {
                            label,
                            value: value.to_owned(),
                            link: false,
                        })
                        .collect(),
                })
                .collect(),
        }
    }

    #[test]
    fn copy_all_lists_the_name_then_one_labelled_line_per_field() {
        let all = details(vec![
            ("Identity", vec![("Session id", "s-1"), ("Host", "local")]),
            ("Location", vec![("Working directory", "/tmp/a b")]),
        ]);
        assert_eq!(
            all.copy_all("alpha"),
            "alpha\nSession id: s-1\nHost: local\nWorking directory: /tmp/a b"
        );
    }

    #[test]
    fn plain_keeps_spaces_and_drops_control_and_bidi_characters() {
        assert_eq!(plain("/tmp/a  b\n\u{202E}c\u{7}"), "/tmp/a  bc");
    }
}
