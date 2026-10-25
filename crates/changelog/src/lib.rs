//! Changelog fragments and release sections of the ecosystem changelog
//! contract, version 1. Every operation validates all fragments before it
//! writes anything.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use thiserror::Error;

pub const CONFIG_FILE: &str = "changelog.toml";
pub const FRAGMENT_DIR: &str = "changelog.d";
pub const START_MARKER: &str = "<!-- pohunek-changelog:start -->";
pub const END_MARKER: &str = "<!-- pohunek-changelog:end -->";

const DELIMITER: &str = "---";
const NO_CHANGES: &str = "No user-facing changes.";
const SURFACE_SEPARATOR: &str = ", ";
const CHANGELOG_TITLE: &str = "# Changelog";

#[derive(Debug, Error)]
pub enum Error {
    #[error("{}: {source}", path.display())]
    Io { path: PathBuf, source: io::Error },
    #[error("{0}")]
    Config(String),
    #[error("{0}")]
    Argument(String),
    /// One message per invalid fragment, each prefixed with its path.
    #[error("{}", .0.join("\n"))]
    Fragments(Vec<String>),
    #[error("{0}")]
    Changelog(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Added,
    Changed,
    Deprecated,
    Removed,
    Fixed,
    Security,
}

/// Section order inside a version.
const KINDS: [Kind; 6] = [
    Kind::Added,
    Kind::Changed,
    Kind::Deprecated,
    Kind::Removed,
    Kind::Fixed,
    Kind::Security,
];

impl Kind {
    fn parse(value: &str) -> Option<Self> {
        KINDS.into_iter().find(|kind| kind.key() == value)
    }

    fn key(self) -> &'static str {
        match self {
            Self::Added => "added",
            Self::Changed => "changed",
            Self::Deprecated => "deprecated",
            Self::Removed => "removed",
            Self::Fixed => "fixed",
            Self::Security => "security",
        }
    }

    fn heading(self) -> &'static str {
        match self {
            Self::Added => "Added",
            Self::Changed => "Changed",
            Self::Deprecated => "Deprecated",
            Self::Removed => "Removed",
            Self::Fixed => "Fixed",
            Self::Security => "Security",
        }
    }
}

/// How the repository splits its changelog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Layout {
    Single {
        changelog: PathBuf,
    },
    /// Surface name to its changelog file; fragments name the surfaces they affect.
    Surfaces(BTreeMap<String, PathBuf>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub repository: String,
    pub layout: Layout,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    repository: String,
    changelog: Option<PathBuf>,
    surfaces: Option<BTreeMap<String, PathBuf>>,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, Error> {
        let text = fs::read_to_string(path).map_err(|source| Error::Io {
            path: path.to_owned(),
            source,
        })?;
        let raw: RawConfig = toml::from_str(&text)
            .map_err(|source| Error::Config(format!("{}: {source}", path.display())))?;
        Self::from_raw(raw)
    }

    fn from_raw(raw: RawConfig) -> Result<Self, Error> {
        if !valid_repository(&raw.repository) {
            return Err(Error::Config(format!(
                "repository must be owner/name: {}",
                raw.repository
            )));
        }
        let layout = match (raw.changelog, raw.surfaces) {
            (Some(changelog), None) => Layout::Single { changelog },
            (None, Some(surfaces)) if !surfaces.is_empty() => {
                if let Some(name) = surfaces.keys().find(|name| !valid_slug(name)) {
                    return Err(Error::Config(format!("invalid surface name: {name}")));
                }
                Layout::Surfaces(surfaces)
            }
            _ => {
                return Err(Error::Config(
                    "declare either changelog or a non-empty [surfaces] table".to_owned(),
                ));
            }
        };
        Ok(Self {
            repository: raw.repository,
            layout,
        })
    }

    /// Changelog file (relative to the root) that a command of `surface` touches.
    fn changelog_path(&self, surface: Option<&str>) -> Result<&Path, Error> {
        match (&self.layout, surface) {
            (Layout::Single { changelog }, None) => Ok(changelog),
            (Layout::Single { .. }, Some(_)) => Err(Error::Argument(
                "--surface does not apply to a single-product repository".to_owned(),
            )),
            (Layout::Surfaces(_), None) => Err(Error::Argument(
                "this repository has surfaces: pass --surface <name>".to_owned(),
            )),
            (Layout::Surfaces(surfaces), Some(name)) => surfaces
                .get(name)
                .map(PathBuf::as_path)
                .ok_or_else(|| Error::Argument(format!("unknown surface: {name}"))),
        }
    }

    /// Surfaces a fragment may name; `None` in a single-product repository.
    fn declared_surfaces(&self) -> Option<Vec<String>> {
        match &self.layout {
            Layout::Single { .. } => None,
            Layout::Surfaces(surfaces) => Some(surfaces.keys().cloned().collect()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fragment {
    pub file_name: String,
    pub number: u64,
    pub slug: String,
    pub kind: Kind,
    pub breaking: bool,
    pub surfaces: Vec<String>,
    pub text: String,
}

impl Fragment {
    fn belongs_to(&self, surface: Option<&str>) -> bool {
        surface.is_none_or(|name| self.surfaces.iter().any(|s| s == name))
    }
}

/// Reads and validates every fragment; any invalid one fails the whole call.
pub fn load_fragments(root: &Path, config: &Config) -> Result<Vec<Fragment>, Error> {
    let dir = root.join(FRAGMENT_DIR);
    let mut names = Vec::new();
    match fs::read_dir(&dir) {
        Ok(entries) => {
            for entry in entries {
                let entry = entry.map_err(|source| Error::Io {
                    path: dir.clone(),
                    source,
                })?;
                let name = entry.file_name().to_string_lossy().into_owned();
                // Dotfiles (such as .gitkeep) keep the empty directory in git.
                if !name.starts_with('.') {
                    names.push(name);
                }
            }
        }
        Err(source) if source.kind() == io::ErrorKind::NotFound => {}
        Err(source) => return Err(Error::Io { path: dir, source }),
    }
    names.sort();

    let surfaces = config.declared_surfaces();
    let mut fragments = Vec::new();
    let mut problems = Vec::new();
    for name in names {
        let outcome = fs::read(dir.join(&name))
            .map_err(|source| source.to_string())
            .and_then(|bytes| parse_fragment(&name, &bytes, surfaces.as_deref()));
        match outcome {
            Ok(fragment) => fragments.push(fragment),
            Err(reason) => problems.push(format!("{FRAGMENT_DIR}/{name}: {reason}")),
        }
    }
    if problems.is_empty() {
        Ok(fragments)
    } else {
        Err(Error::Fragments(problems))
    }
}

fn parse_fragment(
    file_name: &str,
    bytes: &[u8],
    surfaces: Option<&[String]>,
) -> Result<Fragment, String> {
    let (number, slug) =
        parse_file_name(file_name).ok_or("bad file name: expected <N>-<slug>.md")?;
    let text = std::str::from_utf8(bytes).map_err(|_| "not valid UTF-8")?;
    if text.contains('\r') {
        return Err("CR in the file; line endings must be LF".to_owned());
    }
    let mut lines: Vec<&str> = text.split('\n').collect();
    if text.ends_with('\n') {
        lines.pop();
    }
    if lines.first().copied() != Some(DELIMITER) {
        return Err("no opening ---".to_owned());
    }
    let close = lines
        .iter()
        .skip(1)
        .position(|line| *line == DELIMITER)
        .map(|index| index + 1)
        .ok_or("no closing ---")?;

    let mut fields: BTreeMap<&str, &str> = BTreeMap::new();
    for line in &lines[1..close] {
        let (key, value) = line
            .split_once(": ")
            .filter(|(key, _)| !key.is_empty() && key.bytes().all(|b| b.is_ascii_lowercase()))
            .ok_or_else(|| format!("bad header line: {line}"))?;
        if value.is_empty() || value.trim() != value {
            return Err(format!("bad value for {key}: {value:?}"));
        }
        if !matches!(key, "kind" | "breaking" | "surfaces") {
            return Err(format!("unknown key: {key}"));
        }
        if fields.insert(key, value).is_some() {
            return Err(format!("duplicate key: {key}"));
        }
    }

    let kind_value = fields.get("kind").copied().ok_or("missing kind")?;
    let kind = Kind::parse(kind_value).ok_or_else(|| format!("unknown kind: {kind_value}"))?;
    let breaking = match fields.get("breaking").copied() {
        None | Some("false") => false,
        Some("true") => true,
        Some(other) => return Err(format!("breaking must be true or false: {other}")),
    };
    let fragment_surfaces = match (surfaces, fields.get("surfaces").copied()) {
        (None, None) => Vec::new(),
        (None, Some(_)) => {
            return Err("surfaces is not allowed in a single-product repository".to_owned());
        }
        (Some(_), None) => return Err("missing surfaces".to_owned()),
        (Some(declared), Some(value)) => {
            let names: Vec<String> = value.split(SURFACE_SEPARATOR).map(str::to_owned).collect();
            for (index, name) in names.iter().enumerate() {
                if !declared.contains(name) {
                    return Err(format!("unknown surface: {name}"));
                }
                if names[..index].contains(name) {
                    return Err(format!("duplicate surface: {name}"));
                }
            }
            names
        }
    };

    let body = trim_blank_lines(&lines[close + 1..]);
    if body.is_empty() {
        return Err("empty body".to_owned());
    }
    if body.iter().any(|line| line.trim().is_empty()) {
        return Err("blank line in body".to_owned());
    }
    if body.iter().any(|line| line.starts_with('#')) {
        return Err("heading line in body".to_owned());
    }

    Ok(Fragment {
        file_name: file_name.to_owned(),
        number,
        slug: slug.to_owned(),
        kind,
        breaking,
        surfaces: fragment_surfaces,
        text: body
            .iter()
            .map(|line| line.trim())
            .collect::<Vec<_>>()
            .join(" "),
    })
}

/// `<N>-<slug>.md`: N has no leading zero; the slug is lowercase words joined by single hyphens.
fn parse_file_name(name: &str) -> Option<(u64, &str)> {
    let stem = name.strip_suffix(".md")?;
    let (number, slug) = stem.split_once('-')?;
    if number.starts_with('0') || !number.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let number = number.parse().ok()?;
    valid_slug(slug).then_some((number, slug))
}

fn trim_blank_lines<'a>(lines: &[&'a str]) -> Vec<&'a str> {
    let start = lines
        .iter()
        .position(|line| !line.trim().is_empty())
        .unwrap_or(lines.len());
    let end = lines
        .iter()
        .rposition(|line| !line.trim().is_empty())
        .map_or(start, |index| index + 1);
    lines[start..end].to_vec()
}

fn valid_slug(value: &str) -> bool {
    !value.is_empty()
        && value.split('-').all(|word| {
            !word.is_empty()
                && word
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        })
}

fn valid_repository(value: &str) -> bool {
    let valid_part = |part: &str| {
        !part.is_empty()
            && part
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    };
    value
        .split_once('/')
        .is_some_and(|(owner, name)| valid_part(owner) && valid_part(name))
}

/// `X.Y.Z` or `X.Y.Z-pre` with dot-separated identifiers.
pub fn validate_version(version: &str) -> Result<(), Error> {
    let (core, pre) = match version.split_once('-') {
        Some((core, pre)) => (core, Some(pre)),
        None => (version, None),
    };
    let core_ok = {
        let parts: Vec<&str> = core.split('.').collect();
        parts.len() == 3
            && parts
                .iter()
                .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
    };
    let pre_ok = pre.is_none_or(|pre| {
        pre.split('.')
            .all(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'))
    });
    if core_ok && pre_ok {
        Ok(())
    } else {
        Err(Error::Argument(format!(
            "version must be X.Y.Z or X.Y.Z-pre: {version}"
        )))
    }
}

/// `YYYY-MM-DD` with a real calendar date.
pub fn validate_date(date: &str) -> Result<(), Error> {
    let bytes = date.as_bytes();
    let shape_ok = bytes.len() == 10
        && bytes.iter().enumerate().all(|(index, &b)| match index {
            4 | 7 => b == b'-',
            _ => b.is_ascii_digit(),
        });
    let field = |range: std::ops::Range<usize>| date.get(range)?.parse::<u64>().ok();
    if let (true, Some(year), Some(month), Some(day)) =
        (shape_ok, field(0..4), field(5..7), field(8..10))
        && (1..=12).contains(&month)
        && (1..=days_in_month(year, month)).contains(&day)
    {
        return Ok(());
    }
    Err(Error::Argument(format!("date must be YYYY-MM-DD: {date}")))
}

fn days_in_month(year: u64, month: u64) -> u64 {
    match month {
        2 if is_leap_year(year) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

fn is_leap_year(year: u64) -> bool {
    year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400))
}

/// The current UTC date as `YYYY-MM-DD`.
pub fn today_utc() -> Result<String, Error> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|source| Error::Changelog(format!("system clock is before 1970: {source}")))?;
    let days = i64::try_from(elapsed.as_secs() / 86_400)
        .map_err(|source| Error::Changelog(format!("system clock out of range: {source}")))?;
    let (year, month, day) = civil_from_days(days);
    Ok(format!("{year:04}-{month:02}-{day:02}"))
}

/// Gregorian date for a day count since 1970-01-01 (Howard Hinnant's algorithm).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month as u32, day as u32)
}

/// Writes the version section, deletes the folded fragments. Surface folds
/// remove only that surface from shared fragments.
pub fn fold(
    root: &Path,
    config: &Config,
    version: &str,
    date: &str,
    surface: Option<&str>,
) -> Result<(), Error> {
    validate_version(version)?;
    validate_date(date)?;
    let changelog = root.join(config.changelog_path(surface)?);
    let text = read_changelog(&changelog)?;
    if find_version_heading(&text, version).is_some() {
        return Err(Error::Changelog(format!(
            "{version} already has a section in {}",
            changelog.display()
        )));
    }
    let fragments = load_fragments(root, config)?;
    let selected: Vec<&Fragment> = fragments
        .iter()
        .filter(|fragment| fragment.belongs_to(surface))
        .collect();

    let block = format!(
        "## {version} - {date}\n\n{}\n",
        render_section(&selected, &config.repository)
    );
    write_file(&changelog, &insert_section(&text, &block))?;

    let dir = root.join(FRAGMENT_DIR);
    for fragment in selected {
        let path = dir.join(&fragment.file_name);
        let left: Vec<&str> = fragment
            .surfaces
            .iter()
            .map(String::as_str)
            .filter(|name| Some(*name) != surface)
            .collect();
        match surface {
            Some(name) if !left.is_empty() => {
                let text = read_file(&path)?;
                write_file(&path, &remove_surface(&text, name))?;
            }
            _ => remove_file(&path)?,
        }
    }
    Ok(())
}

/// The release body block for `version`: its section content between markers.
/// Fails while fragments of the released product remain unfolded.
pub fn notes(
    root: &Path,
    config: &Config,
    version: &str,
    surface: Option<&str>,
) -> Result<String, Error> {
    validate_version(version)?;
    let changelog = root.join(config.changelog_path(surface)?);
    let fragments = load_fragments(root, config)?;
    let unfolded: Vec<&str> = fragments
        .iter()
        .filter(|fragment| fragment.belongs_to(surface))
        .map(|fragment| fragment.file_name.as_str())
        .collect();
    if !unfolded.is_empty() {
        return Err(Error::Changelog(format!(
            "fragments not folded into {version}: {}",
            unfolded.join(", ")
        )));
    }
    let text = read_changelog(&changelog)?;
    let content = section_content(&text, version).ok_or_else(|| {
        Error::Changelog(format!(
            "no section for {version} in {}",
            changelog.display()
        ))
    })?;
    Ok(format!("{START_MARKER}\n{content}\n{END_MARKER}\n"))
}

/// Validates every fragment; returns how many there are.
pub fn check(root: &Path, config: &Config) -> Result<usize, Error> {
    load_fragments(root, config).map(|fragments| fragments.len())
}

fn render_section(fragments: &[&Fragment], repository: &str) -> String {
    if fragments.is_empty() {
        return NO_CHANGES.to_owned();
    }
    let mut parts = Vec::new();
    for kind in KINDS {
        let mut items: Vec<&Fragment> = fragments
            .iter()
            .copied()
            .filter(|fragment| fragment.kind == kind)
            .collect();
        if items.is_empty() {
            continue;
        }
        items.sort_by(|a, b| {
            (!a.breaking, a.number, &a.slug).cmp(&(!b.breaking, b.number, &b.slug))
        });
        let entries: Vec<String> = items
            .iter()
            .map(|fragment| {
                let prefix = if fragment.breaking {
                    "**Breaking:** "
                } else {
                    ""
                };
                format!(
                    "- {prefix}{} ([#{n}](https://github.com/{repository}/issues/{n}))",
                    fragment.text,
                    n = fragment.number
                )
            })
            .collect();
        parts.push(format!("### {}\n\n{}", kind.heading(), entries.join("\n")));
    }
    parts.join("\n\n")
}

/// Inserts `block` before the first version heading, or after the intro when
/// there is none, and ends the file with exactly one newline.
fn insert_section(text: &str, block: &str) -> String {
    let updated = match line_offsets(text).find(|(_, line)| line.starts_with("## ")) {
        Some((offset, _)) => format!("{}{block}\n{}", &text[..offset], &text[offset..]),
        None => format!("{}\n\n{block}", text.trim_end_matches('\n')),
    };
    format!("{}\n", updated.trim_end_matches('\n'))
}

fn line_offsets(text: &str) -> impl Iterator<Item = (usize, &str)> {
    let mut offset = 0;
    text.split_inclusive('\n').map(move |line| {
        let start = offset;
        offset += line.len();
        (start, line)
    })
}

fn find_version_heading(text: &str, version: &str) -> Option<usize> {
    text.lines().position(|line| {
        line.strip_prefix("## ")
            .and_then(|rest| rest.strip_prefix(version))
            .is_some_and(|rest| rest.starts_with(" - "))
    })
}

/// Content after the heading and its blank line, up to the next version
/// heading, without trailing blank lines.
fn section_content(text: &str, version: &str) -> Option<String> {
    let lines: Vec<&str> = text.lines().collect();
    let heading = find_version_heading(text, version)?;
    if lines.get(heading + 1) != Some(&"") {
        return None;
    }
    let rest = &lines[heading + 2..];
    let end = rest
        .iter()
        .position(|line| line.starts_with("## "))
        .unwrap_or(rest.len());
    let mut content = &rest[..end];
    while content.last().is_some_and(|line| line.trim().is_empty()) {
        content = &content[..content.len() - 1];
    }
    Some(content.join("\n"))
}

/// Removes `surface` from the header's `surfaces:` line; every other line is kept.
fn remove_surface(text: &str, surface: &str) -> String {
    let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
    for line in lines
        .iter_mut()
        .skip(1)
        .take_while(|line| line.as_str() != DELIMITER)
    {
        if let Some(value) = line.strip_prefix("surfaces: ") {
            let left: Vec<&str> = value
                .split(SURFACE_SEPARATOR)
                .filter(|name| *name != surface)
                .collect();
            let updated = format!("surfaces: {}", left.join(SURFACE_SEPARATOR));
            *line = updated;
        }
    }
    format!("{}\n", lines.join("\n"))
}

fn read_changelog(path: &Path) -> Result<String, Error> {
    let text = read_file(path)?;
    if text.lines().next() != Some(CHANGELOG_TITLE) {
        return Err(Error::Changelog(format!(
            "{} must start with \"{CHANGELOG_TITLE}\"",
            path.display()
        )));
    }
    Ok(text)
}

fn read_file(path: &Path) -> Result<String, Error> {
    fs::read_to_string(path).map_err(|source| Error::Io {
        path: path.to_owned(),
        source,
    })
}

fn write_file(path: &Path, text: &str) -> Result<(), Error> {
    fs::write(path, text).map_err(|source| Error::Io {
        path: path.to_owned(),
        source,
    })
}

fn remove_file(path: &Path) -> Result<(), Error> {
    fs::remove_file(path).map_err(|source| Error::Io {
        path: path.to_owned(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_date_from_day_count() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        // 2000-02-29 is day 11016 after the epoch.
        assert_eq!(civil_from_days(11_016), (2000, 2, 29));
    }

    #[test]
    fn calendar_dates_are_checked() {
        assert!(validate_date("2026-10-24").is_ok());
        assert!(validate_date("2024-02-29").is_ok());
        assert!(validate_date("2026-02-29").is_err());
        assert!(validate_date("2026-13-01").is_err());
        assert!(validate_date("2026-1-01").is_err());
    }
}
