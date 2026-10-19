//! The owner's client-side layout: groups of sessions in a manual order,
//! persisted in `layout.toml` under the state directory. Never stored in
//! session metadata and never sent to the daemon.

use std::collections::{HashMap, HashSet};
use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::session::{SessionKey, SessionRow};
use crate::sort_key;

const FORMAT_VERSION: u32 = 1;
const LAYOUT_FILE: &str = "layout.toml";
const APP_DIR: &str = "pohunek-companion";
const STATE_HOME_ENV: &str = "XDG_STATE_HOME";
const DIR_MODE: u32 = 0o700;
const FILE_MODE: u32 = 0o600;
/// Id and display name of the group that receives sessions without a place.
pub const INBOX_ID: &str = "inbox";
const INBOX_NAME: &str = "Inbox";

/// Why the layout file could not be located, read, or accepted. The message
/// names the file and is shown to the owner as is.
#[derive(Debug, thiserror::Error)]
pub enum LayoutError {
    #[error("cannot locate the state directory: neither {STATE_HOME_ENV} nor HOME is set")]
    NoStateDir,
    #[error("cannot read {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("invalid {path}: {message}")]
    Invalid { path: PathBuf, message: String },
    #[error("cannot save {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

/// The directory for the app's state files: `$XDG_STATE_HOME/pohunek-companion`
/// when `XDG_STATE_HOME` is absolute, else `~/.local/state/pohunek-companion`
/// on Linux and `~/Library/Application Support/pohunek-companion` on macOS.
pub fn state_dir() -> Result<PathBuf, LayoutError> {
    if let Some(dir) = std::env::var_os(STATE_HOME_ENV).map(PathBuf::from)
        && dir.is_absolute()
    {
        return Ok(dir.join(APP_DIR));
    }
    let home = std::env::home_dir().ok_or(LayoutError::NoStateDir)?;
    let base = if cfg!(target_os = "macos") {
        home.join("Library/Application Support")
    } else {
        home.join(".local/state")
    };
    Ok(base.join(APP_DIR))
}

/// Location of `layout.toml`.
pub fn layout_path() -> Result<PathBuf, LayoutError> {
    Ok(state_dir()?.join(LAYOUT_FILE))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Group {
    /// Stable identity; the name can change without touching items.
    pub id: String,
    pub name: String,
    /// Fractional sort key among groups.
    pub order: String,
}

/// A session's place: its group and its sort key within that group.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Item {
    pub host: String,
    pub session_id: String,
    pub group: String,
    pub order: String,
}

impl Item {
    fn key(&self) -> SessionKey {
        SessionKey {
            host: self.host.clone(),
            session_id: self.session_id.clone(),
        }
    }
}

/// Groups and the placement of every known session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Layout {
    version: u32,
    groups: Vec<Group>,
    items: Vec<Item>,
}

/// One group with its sessions in display order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupSection {
    pub id: String,
    pub name: String,
    pub rows: Vec<SessionRow>,
}

/// What a reconcile changed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Reconciled {
    pub added: Vec<SessionKey>,
    pub pruned: Vec<SessionKey>,
}

impl Default for Layout {
    /// Inbox only.
    fn default() -> Self {
        Self {
            version: FORMAT_VERSION,
            groups: vec![inbox_group(sort_key::between(None, None))],
            items: Vec::new(),
        }
    }
}

fn inbox_group(order: String) -> Group {
    Group {
        id: INBOX_ID.to_owned(),
        name: INBOX_NAME.to_owned(),
        order,
    }
}

impl Layout {
    pub fn groups(&self) -> &[Group] {
        &self.groups
    }

    pub fn items(&self) -> &[Item] {
        &self.items
    }

    /// Appends sessions the layout does not know to the end of Inbox, in list
    /// order, and drops placements of sessions missing from `current`.
    pub fn reconcile(&mut self, current: &[SessionKey]) -> Reconciled {
        let listed: HashSet<&SessionKey> = current.iter().collect();
        let mut result = Reconciled::default();
        self.items.retain(|item| {
            let keep = listed.contains(&item.key());
            if !keep {
                result.pruned.push(item.key());
            }
            keep
        });
        let mut known: HashSet<SessionKey> = self.items.iter().map(Item::key).collect();
        let mut last = self
            .items
            .iter()
            .filter(|item| item.group == INBOX_ID)
            .map(|item| item.order.as_str())
            .max()
            .map(str::to_owned);
        for key in current {
            if !known.insert(key.clone()) {
                continue;
            }
            let order = sort_key::between(last.as_deref(), None);
            last = Some(order.clone());
            self.items.push(Item {
                host: key.host.clone(),
                session_id: key.session_id.clone(),
                group: INBOX_ID.to_owned(),
                order,
            });
            result.added.push(key.clone());
        }
        result
    }

    /// Groups in order, each with the rows of its items in order. A row
    /// without a placement is left out until the next reconcile.
    pub fn arrange(&self, rows: &[SessionRow]) -> Vec<GroupSection> {
        let by_key: HashMap<&SessionKey, &SessionRow> =
            rows.iter().map(|row| (&row.key, row)).collect();
        let mut groups: Vec<&Group> = self.groups.iter().collect();
        groups.sort_by(|a, b| (&a.order, &a.id).cmp(&(&b.order, &b.id)));
        groups
            .into_iter()
            .map(|group| {
                let mut items: Vec<&Item> = self
                    .items
                    .iter()
                    .filter(|item| item.group == group.id)
                    .collect();
                items.sort_by(|a, b| {
                    (&a.order, &a.host, &a.session_id).cmp(&(&b.order, &b.host, &b.session_id))
                });
                GroupSection {
                    id: group.id.clone(),
                    name: group.name.clone(),
                    rows: items
                        .into_iter()
                        .filter_map(|item| by_key.get(&item.key()).map(|row| (*row).clone()))
                        .collect(),
                }
            })
            .collect()
    }

    /// Checks a parsed layout and repairs what a hand edit can leave
    /// inconsistent without losing the owner's data; returns whether it
    /// repaired anything.
    fn validate(&mut self) -> Result<bool, String> {
        if self.version != FORMAT_VERSION {
            return Err(format!(
                "unsupported format version {} (this build reads version {FORMAT_VERSION})",
                self.version
            ));
        }
        let mut ids = HashSet::new();
        for group in &self.groups {
            if group.id.is_empty() || !ids.insert(group.id.as_str()) {
                return Err(format!("group id {:?} is empty or repeated", group.id));
            }
            if !sort_key::is_valid(&group.order) {
                return Err(format!(
                    "group {:?} has an invalid order key {:?}",
                    group.id, group.order
                ));
            }
        }
        let mut sessions = HashSet::new();
        for item in &self.items {
            if !sort_key::is_valid(&item.order) {
                return Err(format!(
                    "session {:?} has an invalid order key {:?}",
                    item.session_id, item.order
                ));
            }
            if !sessions.insert((item.host.as_str(), item.session_id.as_str())) {
                return Err(format!("session {:?} is listed twice", item.session_id));
            }
        }
        let mut repaired = false;
        if !self.groups.iter().any(|group| group.id == INBOX_ID) {
            let first = self.groups.iter().map(|group| group.order.as_str()).min();
            let order = sort_key::between(None, first);
            self.groups.insert(0, inbox_group(order));
            repaired = true;
        }
        let group_ids: HashSet<String> = self.groups.iter().map(|g| g.id.clone()).collect();
        let mut last = self
            .items
            .iter()
            .filter(|item| item.group == INBOX_ID)
            .map(|item| item.order.clone())
            .max();
        for item in &mut self.items {
            if !group_ids.contains(&item.group) {
                let order = sort_key::between(last.as_deref(), None);
                last = Some(order.clone());
                item.group = INBOX_ID.to_owned();
                item.order = order;
                repaired = true;
            }
        }
        Ok(repaired)
    }

    fn to_toml(&self) -> String {
        // Plain strings, integers, and arrays of tables always serialize.
        toml::to_string(self).expect("layout serializes to TOML")
    }
}

/// The layout plus where it lives. A layout that failed to load is kept in
/// memory only and never written, so the damaged file survives for repair.
#[derive(Debug)]
pub struct LayoutStore {
    path: PathBuf,
    layout: Layout,
    writable: bool,
    /// The layout as last read from or written to the file, so a hand-edited
    /// file is not rewritten for formatting alone; `None` when the file needs
    /// writing.
    saved: Option<Layout>,
}

impl LayoutStore {
    /// Reads `path`. A missing file starts the default layout (saved by the
    /// first [`save_if_changed`](Self::save_if_changed)); an unreadable or
    /// invalid one yields an in-memory Inbox-only store and the error.
    pub fn open(path: PathBuf) -> (Self, Option<LayoutError>) {
        match Self::load(&path) {
            Ok((layout, saved)) => {
                info!(
                    path = %path.display(),
                    groups = layout.groups.len(),
                    sessions = layout.items.len(),
                    "layout loaded"
                );
                (
                    Self {
                        path,
                        layout,
                        writable: true,
                        saved,
                    },
                    None,
                )
            }
            Err(err) => {
                warn!(error = %err, "layout not loaded; running with an unsaved Inbox-only layout");
                (
                    Self {
                        path,
                        layout: Layout::default(),
                        writable: false,
                        saved: None,
                    },
                    Some(err),
                )
            }
        }
    }

    /// An Inbox-only layout with nowhere to save, for when the state
    /// directory cannot be located.
    pub fn unsaved() -> Self {
        Self {
            path: PathBuf::new(),
            layout: Layout::default(),
            writable: false,
            saved: None,
        }
    }

    fn load(path: &Path) -> Result<(Layout, Option<Layout>), LayoutError> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                return Ok((Layout::default(), None));
            }
            Err(source) => {
                return Err(LayoutError::Read {
                    path: path.to_owned(),
                    source,
                });
            }
        };
        let invalid = |message: String| LayoutError::Invalid {
            path: path.to_owned(),
            message,
        };
        let mut layout: Layout = toml::from_str(&text).map_err(|err| invalid(err.to_string()))?;
        let repaired = layout.validate().map_err(invalid)?;
        // A repaired layout differs from the file, so it is saved next.
        let saved = (!repaired).then(|| layout.clone());
        Ok((layout, saved))
    }

    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    /// Reconciles with the daemon's full session list and logs the outcome.
    pub fn reconcile(&mut self, current: &[SessionKey]) -> Reconciled {
        let result = self.layout.reconcile(current);
        if !result.added.is_empty() || !result.pruned.is_empty() {
            info!(
                added = ?result.added.iter().map(|k| k.session_id.as_str()).collect::<Vec<_>>(),
                pruned = ?result.pruned.iter().map(|k| k.session_id.as_str()).collect::<Vec<_>>(),
                "layout reconciled with the session list"
            );
        }
        result
    }

    /// Writes the layout when it differs from the file; returns whether it
    /// wrote. Does nothing for a layout that failed to load.
    pub fn save_if_changed(&mut self) -> Result<bool, LayoutError> {
        if !self.writable {
            return Ok(false);
        }
        if self.saved.as_ref() == Some(&self.layout) {
            return Ok(false);
        }
        write_atomic(&self.path, &self.layout.to_toml()).map_err(|source| LayoutError::Write {
            path: self.path.clone(),
            source,
        })?;
        info!(
            path = %self.path.display(),
            groups = self.layout.groups.len(),
            sessions = self.layout.items.len(),
            "layout saved"
        );
        self.saved = Some(self.layout.clone());
        Ok(true)
    }
}

/// Writes `text` to a temporary file next to `path` and renames it over
/// `path`, creating the directory with owner-only access.
fn write_atomic(path: &Path, text: &str) -> io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::other("layout path has no directory"))?;
    DirBuilder::new()
        .recursive(true)
        .mode(DIR_MODE)
        .create(dir)?;
    let tmp = dir.join(format!("{LAYOUT_FILE}.tmp-{}", std::process::id()));
    let written = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(FILE_MODE)
            .open(&tmp)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        fs::rename(&tmp, path)
    })();
    if written.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    written
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

    fn keys(ids: &[&str]) -> Vec<SessionKey> {
        ids.iter().map(|id| key(id)).collect()
    }

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("companion-layout-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn inbox_order(layout: &Layout) -> Vec<String> {
        let mut items: Vec<&Item> = layout.items().iter().collect();
        items.sort_by(|a, b| a.order.cmp(&b.order));
        items.iter().map(|item| item.session_id.clone()).collect()
    }

    #[test]
    fn reconcile_appends_unknown_sessions_in_list_order_and_prunes_missing_ones() {
        let mut layout = Layout::default();
        layout.reconcile(&keys(&["s1", "s2", "s3"]));
        assert_eq!(inbox_order(&layout), ["s1", "s2", "s3"]);

        let result = layout.reconcile(&keys(&["s3", "s1", "s4", "s5"]));
        assert_eq!(result.pruned, keys(&["s2"]));
        assert_eq!(result.added, keys(&["s4", "s5"]));
        assert_eq!(inbox_order(&layout), ["s1", "s3", "s4", "s5"]);
    }

    #[test]
    fn a_session_in_a_hand_edited_group_keeps_its_place() {
        let text = r#"
version = 1
[[groups]]
id = "inbox"
name = "Inbox"
order = "n"
[[groups]]
id = "work"
name = "Work"
order = "h"
[[items]]
host = "local"
session_id = "s2"
group = "work"
order = "n"
[[items]]
host = "local"
session_id = "s1"
group = "work"
order = "g"
"#;
        let mut layout: Layout = toml::from_str(text).expect("parses");
        assert_eq!(layout.validate(), Ok(false));
        layout.reconcile(&keys(&["s1", "s2", "s3"]));
        let sections = layout.arrange(&[]);
        assert_eq!(
            sections.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
            ["work", "inbox"]
        );
        let work: Vec<&Item> = layout
            .items()
            .iter()
            .filter(|i| i.group == "work")
            .collect();
        assert_eq!(work.len(), 2);
        let s3 = layout
            .items()
            .iter()
            .find(|i| i.session_id == "s3")
            .expect("s3");
        assert_eq!(s3.group, INBOX_ID);
    }

    #[test]
    fn save_then_load_gives_an_equal_layout_and_an_unchanged_layout_is_not_rewritten() {
        let dir = scratch("roundtrip");
        let path = dir.join("state").join(LAYOUT_FILE);
        let (mut store, error) = LayoutStore::open(path.clone());
        assert!(error.is_none());
        store.reconcile(&keys(&["s1", "s2"]));
        assert!(store.save_if_changed().expect("saves"));
        assert!(!store.save_if_changed().expect("no rewrite"));

        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| fs::metadata(p).expect("metadata").permissions().mode() & 0o777;
        assert_eq!(mode(&path), FILE_MODE);
        assert_eq!(mode(path.parent().expect("dir")), DIR_MODE);

        let (reloaded, error) = LayoutStore::open(path.clone());
        assert!(error.is_none());
        assert_eq!(reloaded.layout(), store.layout());
        let mut reloaded = reloaded;
        reloaded.reconcile(&keys(&["s1", "s2"]));
        assert!(!reloaded.save_if_changed().expect("no rewrite"));

        let annotated = format!(
            "# hand-written note\n{}",
            fs::read_to_string(&path).expect("read")
        );
        fs::write(&path, &annotated).expect("write");
        let (mut edited, error) = LayoutStore::open(path.clone());
        assert!(error.is_none());
        edited.reconcile(&keys(&["s1", "s2"]));
        assert!(!edited.save_if_changed().expect("no rewrite"));
        assert_eq!(fs::read_to_string(&path).expect("read"), annotated);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unparsable_file_is_reported_and_never_replaced() {
        let dir = scratch("corrupt");
        fs::create_dir_all(&dir).expect("dir");
        let path = dir.join(LAYOUT_FILE);
        let damaged = "version = 1\n[[groups]]\nid = \"inb";
        fs::write(&path, damaged).expect("write");

        let (mut store, error) = LayoutStore::open(path.clone());
        assert!(matches!(error, Some(LayoutError::Invalid { .. })));
        store.reconcile(&keys(&["s1"]));
        assert_eq!(store.layout().arrange(&[]).len(), 1);
        assert!(!store.save_if_changed().expect("skips"));
        assert_eq!(fs::read_to_string(&path).expect("read"), damaged);
        let _ = fs::remove_dir_all(&dir);
    }
}
