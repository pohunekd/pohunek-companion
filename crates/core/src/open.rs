//! Opening a session's folder or Work link through the configured argv
//! template. The target reaches the opener as one argv element; no shell ever
//! sees it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use pohunek_client::protocol::SessionInfo;
use serde::{Deserialize, Deserializer};
use tracing::{error, info};

use crate::attach::{AttachError, observe, resolve_program, spawn_detached};

const TARGET_PLACEHOLDER: &str = "target";
const WORK_LINK_URL_KEY: &str = "work.link.url";
const LEGACY_LINK_URL_KEY: &str = "link.url";
/// Longest link that is offered; longer metadata values are not URLs worth opening.
const LINK_MAX_BYTES: usize = 2048;

/// Where a session's folder and Work link point, as far as its snapshot says.
/// Only candidates: the folder may have disappeared since the snapshot.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OpenTargets {
    /// The session's worktree when it has one, else its working directory.
    pub folder: Option<PathBuf>,
    /// The `http` or `https` Work link URL.
    pub link: Option<String>,
}

impl OpenTargets {
    pub(crate) fn from_info(info: &SessionInfo) -> Self {
        Self::select(info.worktree_path.as_deref(), &info.cwd, &info.metadata)
    }

    fn select(
        worktree_path: Option<&Path>,
        cwd: &Path,
        metadata: &BTreeMap<String, String>,
    ) -> Self {
        let folder = worktree_path.unwrap_or(cwd).to_path_buf();
        let link = [WORK_LINK_URL_KEY, LEGACY_LINK_URL_KEY]
            .into_iter()
            .find_map(|key| metadata.get(key))
            .filter(|url| is_openable_url(url))
            .cloned();
        Self {
            folder: folder.is_absolute().then_some(folder),
            link,
        }
    }

    /// The folder when it exists as a directory right now.
    pub fn existing_folder(&self) -> Option<&Path> {
        self.folder.as_deref().filter(|path| path.is_dir())
    }
}

/// True for an `http` or `https` URL with a host, made of printable ASCII only
/// and without backslashes (browsers read `\\` as `/`). Session metadata is untrusted, so anything else is not opened.
pub fn is_openable_url(url: &str) -> bool {
    if url.len() > LINK_MAX_BYTES || url.chars().any(|c| !c.is_ascii_graphic() || c == '\\') {
        return false;
    }
    let Some((scheme, rest)) = url.split_once("://") else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host = authority.rsplit('@').next().unwrap_or_default();
    (scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https"))
        && !host.is_empty()
        && !host.starts_with(':')
}

/// Parsed `open_command`: a program word followed by argument words, at least
/// one containing `{target}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenTemplate {
    words: Vec<String>,
}

impl OpenTemplate {
    /// Accepts `words` when the program word is non-empty, `{target}` is the
    /// only placeholder, and it appears at least once. Braces that do not
    /// enclose an identifier stay literal.
    pub fn parse(words: &[String]) -> Result<Self, String> {
        let program = words
            .first()
            .ok_or_else(|| "must contain at least the program word".to_owned())?;
        if program.is_empty() {
            return Err("the program word must not be empty".to_owned());
        }
        if program.contains(&format!("{{{TARGET_PLACEHOLDER}}}")) {
            return Err("the program word must not contain a placeholder".to_owned());
        }
        let mut uses_target = false;
        for word in words {
            let mut rest = word.as_str();
            while let Some(open) = rest.find('{') {
                let after = &rest[open + 1..];
                let name_len = after
                    .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                    .unwrap_or(after.len());
                let name = &after[..name_len];
                let is_placeholder = after[name_len..].starts_with('}')
                    && name.chars().next().is_some_and(|c| !c.is_ascii_digit());
                if is_placeholder {
                    if name != TARGET_PLACEHOLDER {
                        return Err(format!(
                            "unknown placeholder `{{{name}}}` (known: {{{TARGET_PLACEHOLDER}}})"
                        ));
                    }
                    uses_target = true;
                    rest = &after[name_len + 1..];
                } else {
                    rest = after;
                }
            }
        }
        if !uses_target {
            return Err(format!("must contain `{{{TARGET_PLACEHOLDER}}}`"));
        }
        Ok(Self {
            words: words.to_vec(),
        })
    }

    /// Replaces `{target}` in each word in one pass; the inserted value is
    /// never rescanned.
    fn render(&self, target: &str) -> Vec<String> {
        self.words
            .iter()
            .map(|word| word.replace(&format!("{{{TARGET_PLACEHOLDER}}}"), target))
            .collect()
    }
}

impl<'de> Deserialize<'de> for OpenTemplate {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let words = Vec::<String>::deserialize(deserializer)?;
        Self::parse(&words)
            .map_err(|message| serde::de::Error::custom(format!("open_command: {message}")))
    }
}

/// The opener the platform ships with, used when `open_command` is not set.
pub fn default_open_template() -> OpenTemplate {
    let program = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    OpenTemplate {
        words: vec![program.to_owned(), format!("{{{TARGET_PLACEHOLDER}}}")],
    }
}

/// Settings of `open_command` and the timers it shares with attach.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenConfig {
    pub command: OpenTemplate,
    pub observe: Duration,
    pub login_shell_timeout: Duration,
}

/// What an open is for; only used in log lines and error messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenKind {
    Folder,
    Link,
}

impl OpenKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Folder => "folder",
            Self::Link => "link",
        }
    }
}

/// Runs the opener on `target`. Blocks for at most the observation window: an
/// opener that cannot start, or exits unsuccessfully within it, is an error.
pub fn open(config: &OpenConfig, kind: OpenKind, target: &str) -> Result<(), AttachError> {
    let outcome = launch(config, target);
    match &outcome {
        Ok(program) => info!(
            kind = kind.label(),
            program = %program.display(),
            outcome = "launched",
            "open"
        ),
        Err(err) => error!(kind = kind.label(), error = %err, outcome = "failed", "open"),
    }
    outcome.map(|_| ())
}

fn launch(config: &OpenConfig, target: &str) -> Result<PathBuf, AttachError> {
    let argv = config.command.render(target);
    let program = resolve_program(&argv[0], config.login_shell_timeout)?;
    let child = spawn_detached(&program, &argv[1..])?;
    observe(child, &program, config.observe)?;
    Ok(program)
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
    fn only_http_and_https_urls_with_a_host_are_openable() {
        for ok in [
            "https://github.com/o/r/pull/1",
            "http://example.org",
            "HTTPS://Example.org/x?y=1#z",
            "https://user@host:8080/p",
        ] {
            assert!(is_openable_url(ok), "{ok}");
        }
        for bad in [
            "",
            "file:///etc/passwd",
            "javascript:alert(1)",
            "ftp://host/x",
            "https://",
            "https:///path",
            "https://:80/x",
            "github.com/o/r",
            "https://host/a b",
            "https://host/\nx",
            "https://evil.example\\@good.example/",
            "https://h\u{202e}/x",
            "--help",
            "-https://host",
        ] {
            assert!(!is_openable_url(bad), "{bad:?}");
        }
        assert!(!is_openable_url(&format!(
            "https://h/{}",
            "a".repeat(LINK_MAX_BYTES)
        )));
    }

    #[test]
    fn folder_prefers_the_worktree_and_must_be_absolute() {
        let none = BTreeMap::new();
        let targets = OpenTargets::select(Some(Path::new("/wt")), Path::new("/cwd"), &none);
        assert_eq!(targets.folder.as_deref(), Some(Path::new("/wt")));
        let targets = OpenTargets::select(None, Path::new("/cwd"), &none);
        assert_eq!(targets.folder.as_deref(), Some(Path::new("/cwd")));
        let targets = OpenTargets::select(None, Path::new("relative"), &none);
        assert_eq!(targets.folder, None);
    }

    #[test]
    fn link_prefers_work_link_url_then_legacy_and_skips_unsafe_values() {
        let cwd = Path::new("/c");
        let both = meta(&[
            ("work.link.url", "https://a.example/1"),
            ("link.url", "https://b.example/2"),
        ]);
        assert_eq!(
            OpenTargets::select(None, cwd, &both).link.as_deref(),
            Some("https://a.example/1")
        );
        let legacy = meta(&[("link.url", "https://b.example/2")]);
        assert_eq!(
            OpenTargets::select(None, cwd, &legacy).link.as_deref(),
            Some("https://b.example/2")
        );
        let unsafe_work = meta(&[
            ("work.link.url", "file:///etc/passwd"),
            ("link.url", "https://b.example/2"),
        ]);
        assert_eq!(OpenTargets::select(None, cwd, &unsafe_work).link, None);
    }

    #[test]
    fn target_stays_one_argv_element() {
        let t = OpenTemplate::parse(&["op".into(), "--".into(), "{target}".into()]).unwrap();
        assert_eq!(
            t.render("https://h/a b;$(x){target}"),
            ["op", "--", "https://h/a b;$(x){target}"]
        );
        assert!(OpenTemplate::parse(&["op".into()]).is_err());
        assert!(OpenTemplate::parse(&["op".into(), "{id}".into()]).is_err());
        assert!(OpenTemplate::parse(&["{target}".into()]).is_err());
    }
}
