//! Placements recorded by the new-session dialog for sessions that do not
//! exist in the sidebar yet.
//!
//! The dialog is a separate process, so it cannot touch the sidebar's layout.
//! After `session.new` returns the id it appends an entry here; the sidebar
//! moves the session to the entry's group once the session is in its list and
//! deletes the entry. Entries for sessions that never show up expire.

use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::layout::{state_dir, write_atomic};
use crate::session::SessionKey;

const PENDING_FILE: &str = "pending-placement.toml";
const LOCK_FILE: &str = "pending-placement.lock";
const FILE_MODE: u32 = 0o600;
const DIR_MODE: u32 = 0o700;
const FORMAT_VERSION: u32 = 1;

#[derive(Debug, thiserror::Error)]
pub enum PendingError {
    #[error("cannot locate the state directory: {0}")]
    StateDir(String),
    #[error("cannot access {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

/// One session waiting for its group.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingEntry {
    pub host: String,
    pub session_id: String,
    /// Id of the target group.
    pub group: String,
    /// Unix seconds when the dialog recorded the entry.
    pub recorded: u64,
}

impl PendingEntry {
    fn key(&self) -> SessionKey {
        SessionKey {
            host: self.host.clone(),
            session_id: self.session_id.clone(),
        }
    }
}

/// What [`Pending::settle`] decided.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Settled {
    /// Sessions that are present now, with the group to move them to. Their
    /// entries are removed.
    pub apply: Vec<(SessionKey, String)>,
    /// Sessions whose entry outlived the maximum age without the session
    /// appearing. Their entries are removed.
    pub expired: Vec<SessionKey>,
}

/// The entries of the pending file as plain data.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pending {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    entries: Vec<PendingEntry>,
}

impl Pending {
    pub fn entries(&self) -> &[PendingEntry] {
        &self.entries
    }

    /// Records `group` for `key`, replacing an earlier entry for the session.
    pub fn add(&mut self, key: &SessionKey, group: &str, now: u64) {
        self.entries.retain(|entry| entry.key() != *key);
        self.entries.push(PendingEntry {
            host: key.host.clone(),
            session_id: key.session_id.clone(),
            group: group.to_owned(),
            recorded: now,
        });
    }

    /// Removes expired entries, and entries of sessions in `present`; the
    /// latter are returned to be applied. An entry that is both present and
    /// too old is still applied: the session did appear.
    pub fn settle(
        &mut self,
        present: &HashSet<SessionKey>,
        now: u64,
        max_age: Duration,
    ) -> Settled {
        let mut settled = Settled::default();
        self.entries.retain(|entry| {
            let key = entry.key();
            if present.contains(&key) {
                settled.apply.push((key, entry.group.clone()));
                false
            } else if now.saturating_sub(entry.recorded) > max_age.as_secs() {
                settled.expired.push(key);
                false
            } else {
                true
            }
        });
        settled
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// The pending file with an exclusive lock around each read-modify-write, so
/// the dialog and the sidebar never overwrite each other's change.
#[derive(Debug, Clone)]
pub struct PendingStore {
    path: PathBuf,
    lock_path: PathBuf,
}

impl PendingStore {
    pub fn open() -> Result<Self, PendingError> {
        let dir = state_dir().map_err(|err| PendingError::StateDir(err.to_string()))?;
        Ok(Self::in_dir(&dir))
    }

    fn in_dir(dir: &Path) -> Self {
        Self {
            path: dir.join(PENDING_FILE),
            lock_path: dir.join(LOCK_FILE),
        }
    }

    /// Records that `key` goes to `group` and drops entries past `max_age`.
    pub fn record(
        &self,
        key: &SessionKey,
        group: &str,
        max_age: Duration,
    ) -> Result<(), PendingError> {
        self.update(|pending| {
            let now = now_secs();
            for key in pending.settle(&HashSet::new(), now, max_age).expired {
                tracing::warn!(session = %key.session_id, "pending placement expired before the session appeared");
            }
            pending.add(key, group, now);
        })
        .map(|_| ())
    }

    /// Takes the placements whose sessions are in `present` and drops the
    /// expired ones. Does not touch the disk when no entry exists.
    pub fn take(
        &self,
        present: &HashSet<SessionKey>,
        max_age: Duration,
    ) -> Result<Settled, PendingError> {
        if !self.path.exists() {
            return Ok(Settled::default());
        }
        self.update(|pending| pending.settle(present, now_secs(), max_age))
    }

    fn update<T>(&self, change: impl FnOnce(&mut Pending) -> T) -> Result<T, PendingError> {
        let io_err = |path: &Path| {
            let path = path.to_owned();
            move |source| PendingError::Io { path, source }
        };
        let dir = self.path.parent().unwrap_or(Path::new("."));
        fs::DirBuilder::new()
            .recursive(true)
            .mode(DIR_MODE)
            .create(dir)
            .map_err(io_err(dir))?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(FILE_MODE)
            .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
            .open(&self.lock_path)
            .map_err(io_err(&self.lock_path))?;
        rustix::fs::flock(&lock, rustix::fs::FlockOperation::LockExclusive)
            .map_err(|errno| io_err(&self.lock_path)(errno.into()))?;
        let mut pending = match fs::read_to_string(&self.path) {
            Ok(text) => toml::from_str(&text).unwrap_or_else(|err| {
                // The file holds only transient entries; a damaged one is
                // replaced rather than blocking every later placement.
                tracing::warn!(path = %self.path.display(), error = %err, "pending placements unreadable; starting empty");
                Pending::default()
            }),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Pending::default(),
            Err(err) => return Err(io_err(&self.path)(err)),
        };
        pending.version = FORMAT_VERSION;
        let before = pending.clone();
        let result = change(&mut pending);
        if pending != before {
            let text = toml::to_string(&pending).map_err(|err| PendingError::Io {
                path: self.path.clone(),
                source: io::Error::other(err),
            })?;
            write_atomic(&self.path, &text).map_err(io_err(&self.path))?;
        }
        // The lock is released when `lock` is dropped.
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(id: &str) -> SessionKey {
        SessionKey {
            host: "local".to_owned(),
            session_id: id.to_owned(),
        }
    }

    const MAX: Duration = Duration::from_secs(100);

    #[test]
    fn a_present_session_is_applied_once_and_its_entry_removed() {
        let mut pending = Pending::default();
        pending.add(&key("a"), "g1", 1000);
        pending.add(&key("b"), "g2", 1000);
        let present = HashSet::from([key("a")]);
        let settled = pending.settle(&present, 1010, MAX);
        assert_eq!(settled.apply, vec![(key("a"), "g1".to_owned())]);
        assert!(settled.expired.is_empty());
        assert_eq!(pending.entries().len(), 1);
        assert!(pending.settle(&present, 1011, MAX).apply.is_empty());
    }

    #[test]
    fn an_entry_for_a_session_that_never_appears_expires() {
        let mut pending = Pending::default();
        pending.add(&key("a"), "g1", 1000);
        let none = HashSet::new();
        assert_eq!(pending.settle(&none, 1100, MAX), Settled::default());
        let settled = pending.settle(&none, 1101, MAX);
        assert_eq!(settled.expired, vec![key("a")]);
        assert!(pending.entries().is_empty());
    }

    #[test]
    fn a_session_that_appears_late_is_still_applied() {
        let mut pending = Pending::default();
        pending.add(&key("a"), "g1", 1000);
        let present = HashSet::from([key("a")]);
        let settled = pending.settle(&present, 5000, MAX);
        assert_eq!(settled.apply.len(), 1);
        assert!(settled.expired.is_empty());
    }

    #[test]
    fn the_file_round_trips_between_record_and_take_and_expires_stale_entries() {
        let dir = std::env::temp_dir().join(format!("pending-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let store = PendingStore::in_dir(&dir);
        assert_eq!(
            store.take(&HashSet::new(), MAX).unwrap(),
            Settled::default()
        );
        store.record(&key("a"), "g1", MAX).unwrap();
        store.record(&key("b"), "g2", MAX).unwrap();
        let taken = store.take(&HashSet::from([key("a")]), MAX).unwrap();
        assert_eq!(taken.apply, vec![(key("a"), "g1".to_owned())]);
        let again = store.take(&HashSet::from([key("a")]), MAX).unwrap();
        assert!(again.apply.is_empty());
        let expired = store.take(&HashSet::new(), Duration::ZERO);
        // Entries recorded within the same second are not yet older than zero.
        assert!(expired.unwrap().apply.is_empty());
        fs::write(dir.join(PENDING_FILE), "not toml {").unwrap();
        store.record(&key("c"), "g1", MAX).unwrap();
        let taken = store.take(&HashSet::from([key("c")]), MAX).unwrap();
        assert_eq!(taken.apply.len(), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn recording_twice_keeps_the_latest_group() {
        let mut pending = Pending::default();
        pending.add(&key("a"), "g1", 1000);
        pending.add(&key("a"), "g2", 1001);
        assert_eq!(pending.entries().len(), 1);
        assert_eq!(pending.entries()[0].group, "g2");
    }
}
