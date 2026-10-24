//! The `companion.group` session metadata key: the group a session is shown
//! in, shared with the other companion clients. Reading and writing a value
//! apply the same rules.

use std::collections::BTreeMap;

use crate::badge::{is_format_char, sanitize};

/// The session metadata key that holds the group's display name.
pub const GROUP_KEY: &str = "companion.group";

/// The value that places a session in Inbox explicitly, compared ignoring case.
pub const INBOX_VALUE: &str = "Inbox";

/// Why a group name cannot be stored in or read from [`GROUP_KEY`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum GroupNameProblem {
    #[error("the name is empty")]
    Empty,
    #[error("the name is longer than the configured limit")]
    TooLong,
    #[error("the name contains a control or bidirectional-format character")]
    ControlCharacter,
}

impl GroupNameProblem {
    /// A label for logs; it never carries the name itself.
    pub fn code(self) -> &'static str {
        match self {
            Self::Empty => "empty",
            Self::TooLong => "too_long",
            Self::ControlCharacter => "control_character",
        }
    }
}

/// Trims `raw` and refuses it when it is empty, longer than `max_bytes` UTF-8
/// bytes, or holds a control or bidirectional-format character.
pub fn check_group_name(raw: &str, max_bytes: usize) -> Result<&str, GroupNameProblem> {
    let name = raw.trim();
    if name.is_empty() {
        return Err(GroupNameProblem::Empty);
    }
    if name.chars().any(|c| c.is_control() || is_format_char(c)) {
        return Err(GroupNameProblem::ControlCharacter);
    }
    if name.len() > max_bytes {
        return Err(GroupNameProblem::TooLong);
    }
    Ok(name)
}

/// What a session's metadata says about its group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GroupValue {
    /// The key is missing.
    Absent,
    /// The key holds a usable name, sanitized for display and matching.
    Present(String),
    /// The key holds a value that is treated as absent. The value is not
    /// kept, so it cannot reach a log.
    Rejected(GroupNameProblem),
}

/// Reads [`GROUP_KEY`] from a session's metadata.
pub fn parse_group_value(metadata: &BTreeMap<String, String>, max_bytes: usize) -> GroupValue {
    match metadata.get(GROUP_KEY) {
        None => GroupValue::Absent,
        Some(raw) => match check_group_name(raw, max_bytes) {
            Ok(name) => GroupValue::Present(sanitize(name)),
            Err(problem) => GroupValue::Rejected(problem),
        },
    }
}

/// What group names are compared by: as shown, ignoring case.
pub(crate) fn match_key(name: &str) -> String {
    sanitize(name).to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metadata(value: &str) -> BTreeMap<String, String> {
        BTreeMap::from([(GROUP_KEY.to_owned(), value.to_owned())])
    }

    #[test]
    fn a_name_is_trimmed_and_checked_in_bytes() {
        assert_eq!(check_group_name("  Reviews \n", 64), Ok("Reviews"));
        assert_eq!(check_group_name(" \t ", 64), Err(GroupNameProblem::Empty));
        assert_eq!(check_group_name("abcd", 3), Err(GroupNameProblem::TooLong));
        assert_eq!(check_group_name("abc", 3), Ok("abc"));
        // Two characters, four bytes.
        assert_eq!(check_group_name("éé", 3), Err(GroupNameProblem::TooLong));
    }

    #[test]
    fn control_and_format_characters_are_refused() {
        for bad in ["a\u{7}b", "a\nb", "a\u{202E}b", "a\u{200B}b", "a\u{FEFF}b"] {
            assert_eq!(
                check_group_name(bad, 64),
                Err(GroupNameProblem::ControlCharacter),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn a_stored_value_reads_as_absent_present_or_rejected() {
        assert_eq!(parse_group_value(&BTreeMap::new(), 64), GroupValue::Absent);
        assert_eq!(
            parse_group_value(&metadata("  Night   shift "), 64),
            GroupValue::Present("Night shift".to_owned())
        );
        assert_eq!(
            parse_group_value(&metadata(""), 64),
            GroupValue::Rejected(GroupNameProblem::Empty)
        );
        assert_eq!(
            parse_group_value(&metadata("x\u{1b}[0m"), 64),
            GroupValue::Rejected(GroupNameProblem::ControlCharacter)
        );
        assert_eq!(
            parse_group_value(&metadata("abcdef"), 5),
            GroupValue::Rejected(GroupNameProblem::TooLong)
        );
    }
}
