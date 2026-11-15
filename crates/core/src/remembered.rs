//! The agent and option choices of the last session created per host and
//! project, kept in `new-session.toml` next to `layout.toml`. Only the
//! new-session dialog reads and writes the file; the sidebar never does, so
//! the two processes never write one file.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tracing::info;

use crate::layout::{LayoutError, state_dir, write_atomic};

const REMEMBERED_FILE: &str = "new-session.toml";

/// Location of `new-session.toml`.
pub fn remembered_path() -> Result<PathBuf, LayoutError> {
    Ok(state_dir()?.join(REMEMBERED_FILE))
}

/// What the dialog last created on one host in one project.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Remembered {
    /// `local`, or the remote host's target.
    pub host: String,
    /// Project id.
    pub project: String,
    pub agent: String,
    /// Option id to choice id of the non-default choices.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub options: BTreeMap<String, String>,
}

/// The whole file.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RememberedChoices {
    #[serde(default, rename = "entry")]
    entries: Vec<Remembered>,
}

impl RememberedChoices {
    /// The entry of `host` and `project`.
    pub fn get(&self, host: &str, project: &str) -> Option<&Remembered> {
        self.entries
            .iter()
            .find(|entry| entry.host == host && entry.project == project)
    }

    /// Replaces the entry of the same host and project, or appends one.
    fn upsert(&mut self, remembered: Remembered) {
        let (host, project) = (remembered.host.clone(), remembered.project.clone());
        let same = |entry: &Remembered| entry.host == host && entry.project == project;
        match self.entries.iter().position(same) {
            Some(index) => {
                self.entries[index] = remembered;
                // A hand-edited file may hold the pair more than once.
                let mut seen = false;
                self.entries.retain(|entry| {
                    if !same(entry) {
                        return true;
                    }
                    let first = !seen;
                    seen = true;
                    first
                });
            }
            None => self.entries.push(remembered),
        }
    }
}

/// Reads the file at `path`; a missing file is empty. An unreadable or
/// invalid file is an error, and the caller works without it.
pub fn load_remembered(path: &Path) -> Result<RememberedChoices, LayoutError> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            return Ok(RememberedChoices::default());
        }
        Err(source) => {
            return Err(LayoutError::Read {
                path: path.to_owned(),
                source,
            });
        }
    };
    toml::from_str(&text).map_err(|err| LayoutError::Invalid {
        path: path.to_owned(),
        message: err.to_string(),
    })
}

/// Records `remembered` in the file at `path`. The file is read again first,
/// so an entry another dialog wrote meanwhile is kept, and written atomically
/// (directory 0700, file 0600) only when it changes. An unreadable or
/// invalid file is never overwritten. Returns whether the file was written.
pub fn remember(path: &Path, remembered: Remembered) -> Result<bool, LayoutError> {
    let current = load_remembered(path)?;
    let mut next = current.clone();
    next.upsert(remembered);
    if next == current && path.exists() {
        return Ok(false);
    }
    let text = toml::to_string(&next).map_err(|err| LayoutError::Invalid {
        path: path.to_owned(),
        message: err.to_string(),
    })?;
    write_atomic(path, &text).map_err(|source| LayoutError::Write {
        path: path.to_owned(),
        source,
    })?;
    info!(
        path = %path.display(),
        entries = next.entries.len(),
        "new-session choices saved"
    );
    Ok(true)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "companion-remembered-{name}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn entry(host: &str, project: &str, agent: &str, options: &[(&str, &str)]) -> Remembered {
        Remembered {
            host: host.to_owned(),
            project: project.to_owned(),
            agent: agent.to_owned(),
            options: options
                .iter()
                .map(|(option, choice)| ((*option).to_owned(), (*choice).to_owned()))
                .collect(),
        }
    }

    #[test]
    fn a_missing_file_is_empty() {
        let path = scratch("missing").join(REMEMBERED_FILE);
        let loaded = load_remembered(&path).expect("a missing file loads");
        assert_eq!(loaded, RememberedChoices::default());
    }

    #[test]
    fn remember_writes_private_files_once_and_keeps_other_entries() {
        let dir = scratch("write");
        let path = dir.join(REMEMBERED_FILE);
        let first = entry("local", "p-1", "opt", &[("mode", "build")]);
        assert!(remember(&path, first.clone()).expect("first write"));
        assert!(!remember(&path, first.clone()).expect("unchanged"));
        assert!(remember(&path, entry("box", "p-1", "plain", &[])).expect("second host"));
        let replaced = entry("local", "p-1", "opt", &[]);
        assert!(remember(&path, replaced.clone()).expect("replace"));
        let loaded = load_remembered(&path).expect("reload");
        assert_eq!(loaded.get("local", "p-1"), Some(&replaced));
        assert_eq!(
            loaded.get("box", "p-1").map(|e| e.agent.as_str()),
            Some("plain")
        );
        assert_eq!(loaded.get("local", "p-2"), None);
        let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&path), 0o600);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_invalid_file_is_reported_and_never_overwritten() {
        let dir = scratch("invalid");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(REMEMBERED_FILE);
        let text = "[[entry]]\nhost = \"local\"\nsurprise = 1\n";
        fs::write(&path, text).unwrap();
        let err = load_remembered(&path).expect_err("unknown key");
        assert!(matches!(err, LayoutError::Invalid { .. }), "{err}");
        let refused = remember(&path, entry("local", "p-1", "opt", &[]));
        assert!(refused.is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), text);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unreadable_file_is_an_error() {
        let dir = scratch("unreadable");
        // A directory where the file belongs cannot be read as text.
        let path = dir.join(REMEMBERED_FILE);
        fs::create_dir_all(&path).unwrap();
        let err = load_remembered(&path).expect_err("a directory is not a file");
        assert!(matches!(err, LayoutError::Read { .. }), "{err}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_duplicated_pair_collapses_to_the_recorded_entry() {
        let dir = scratch("duplicate");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(REMEMBERED_FILE);
        fs::write(
            &path,
            "[[entry]]\nhost = \"local\"\nproject = \"p\"\nagent = \"a\"\n\n\
             [[entry]]\nhost = \"local\"\nproject = \"p\"\nagent = \"b\"\n",
        )
        .unwrap();
        assert!(remember(&path, entry("local", "p", "c", &[])).expect("write"));
        let loaded = load_remembered(&path).expect("reload");
        assert_eq!(loaded.entries.len(), 1);
        assert_eq!(
            loaded.get("local", "p").map(|e| e.agent.as_str()),
            Some("c")
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
