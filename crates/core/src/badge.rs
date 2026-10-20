use std::collections::BTreeMap;

const WORK_PREFIX: &str = "work.link.";
const LEGACY_PREFIX: &str = "link.";
const ROLE_KEY: &str = "work.role";

/// Marker appended to a clipped value.
const ELLIPSIS: char = '…';

/// The Pohunek Work link fields of a session as sanitized plain text.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkFacts {
    pub provider: Option<String>,
    pub kind: Option<String>,
    pub id: Option<String>,
    /// The session's `work.role`.
    pub role: Option<String>,
}

impl WorkFacts {
    /// Reads the fields from session metadata. The `work.link.*` keys win
    /// when any of them is present; the legacy `link.*` keys are the
    /// fallback. `None` when the metadata carries none of the fields.
    pub(crate) fn from_metadata(metadata: &BTreeMap<String, String>) -> Option<Self> {
        let prefix = if metadata.keys().any(|k| k.starts_with(WORK_PREFIX)) {
            WORK_PREFIX
        } else {
            LEGACY_PREFIX
        };
        let field = |key: &str| {
            metadata
                .get(key)
                .map(|v| sanitize(v))
                .filter(|v| !v.is_empty())
        };
        let facts = Self {
            provider: field(&format!("{prefix}provider")),
            kind: field(&format!("{prefix}kind")),
            id: field(&format!("{prefix}id")),
            role: field(ROLE_KEY),
        };
        (facts != Self::default()).then_some(facts)
    }
}

/// The Pohunek Work link of a session, ready to render as plain text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkBadge {
    /// Abbreviated provider and kind plus the id, for example `GH PR #12`.
    pub link: String,
    /// The session's `work.role`, when set.
    pub role: Option<String>,
}

impl WorkBadge {
    /// Builds the badge from the Work facts. A session without a link id has
    /// no badge.
    pub(crate) fn from_facts(facts: &WorkFacts) -> Option<Self> {
        let id = facts.id.as_deref()?;
        let provider = facts.provider.as_deref().map(abbreviate_provider);
        let kind = facts.kind.as_deref().and_then(abbreviate_kind);
        let id = if id.chars().all(|c| c.is_ascii_digit()) {
            format!("#{id}")
        } else {
            id.to_owned()
        };
        let link = [provider, kind, Some(id)]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" ");
        Some(Self {
            link,
            role: facts.role.clone(),
        })
    }

    #[cfg(test)]
    fn from_metadata(metadata: &BTreeMap<String, String>) -> Option<Self> {
        Self::from_facts(&WorkFacts::from_metadata(metadata)?)
    }
}

fn abbreviate_provider(provider: &str) -> String {
    match provider.to_ascii_lowercase().as_str() {
        "github" => "GH".to_owned(),
        "gitlab" => "GL".to_owned(),
        "linear" => "Linear".to_owned(),
        "jira" => "Jira".to_owned(),
        _ => provider.to_owned(),
    }
}

/// `None` for plain issues: the id alone already reads as an issue.
fn abbreviate_kind(kind: &str) -> Option<String> {
    match kind.to_ascii_lowercase().as_str() {
        "issue" => None,
        "pr" | "pull_request" | "pull-request" | "pullrequest" => Some("PR".to_owned()),
        "mr" | "merge_request" | "merge-request" | "mergerequest" => Some("MR".to_owned()),
        _ => Some(kind.to_owned()),
    }
}

/// Drops control and bidirectional-format characters from untrusted metadata,
/// turns whitespace into single spaces, and trims the ends.
pub(crate) fn sanitize(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        if c.is_whitespace() {
            if !out.ends_with(' ') && !out.is_empty() {
                out.push(' ');
            }
        } else if !c.is_control() && !is_format_char(c) {
            out.push(c);
        }
    }
    out.trim_end().to_owned()
}

fn is_format_char(c: char) -> bool {
    matches!(c, '\u{061C}' | '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2069}' | '\u{FEFF}')
}

/// Clips `text` to at most `max_chars` characters, ending with an ellipsis
/// when it was cut.
pub fn clip_display(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_owned();
    }
    let keep = max_chars.saturating_sub(1);
    let mut clipped: String = text.chars().take(keep).collect();
    clipped.push(ELLIPSIS);
    clipped
}

/// Clips `text` to at most `max_chars` characters by replacing the middle with
/// an ellipsis, so both the start and the end of a path or id stay readable.
pub fn clip_middle(text: &str, max_chars: usize) -> String {
    let count = text.chars().count();
    if count <= max_chars {
        return text.to_owned();
    }
    let keep = max_chars.saturating_sub(1);
    let tail = keep / 2;
    let head = keep - tail;
    let mut clipped: String = text.chars().take(head).collect();
    clipped.push(ELLIPSIS);
    clipped.extend(text.chars().skip(count - tail));
    clipped
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn work_keys_take_precedence_over_legacy_keys() {
        let badge = WorkBadge::from_metadata(&meta(&[
            ("work.link.provider", "github"),
            ("work.link.kind", "pull_request"),
            ("work.link.id", "12"),
            ("work.role", "implementer"),
            ("link.provider", "linear"),
            ("link.id", "ABC-1"),
        ]))
        .unwrap();
        assert_eq!(badge.link, "GH PR #12");
        assert_eq!(badge.role.as_deref(), Some("implementer"));
    }

    #[test]
    fn legacy_keys_apply_without_work_keys() {
        let badge = WorkBadge::from_metadata(&meta(&[
            ("link.provider", "linear"),
            ("link.kind", "issue"),
            ("link.id", "ABC-123"),
        ]))
        .unwrap();
        assert_eq!(badge.link, "Linear ABC-123");
        assert_eq!(badge.role, None);
    }

    #[test]
    fn no_link_id_means_no_badge() {
        assert_eq!(WorkBadge::from_metadata(&meta(&[("work.role", "x")])), None);
    }

    #[test]
    fn facts_follow_the_same_precedence_and_keep_a_role_without_a_link() {
        let facts = WorkFacts::from_metadata(&meta(&[
            ("work.link.provider", "github"),
            ("work.link.id", "12"),
            ("link.kind", "issue"),
            ("work.role", "reviewer"),
        ]))
        .unwrap();
        assert_eq!(facts.provider.as_deref(), Some("github"));
        assert_eq!(facts.kind, None);
        assert_eq!(facts.id.as_deref(), Some("12"));
        assert_eq!(facts.role.as_deref(), Some("reviewer"));
        let role_only = WorkFacts::from_metadata(&meta(&[("work.role", "x")])).unwrap();
        assert_eq!(role_only.id, None);
        assert_eq!(WorkFacts::from_metadata(&meta(&[("other", "x")])), None);
    }

    #[test]
    fn untrusted_values_are_single_line_plain_text() {
        let badge = WorkBadge::from_metadata(&meta(&[
            ("work.link.id", "A\nB\u{202E}\u{7}C"),
            ("work.link.provider", "weird"),
        ]))
        .unwrap();
        assert_eq!(badge.link, "weird A BC");
        assert_eq!(clip_display("abcdef", 4), "abc…");
    }
}
