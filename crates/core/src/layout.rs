//! The owner's client-side layout: groups of sessions in a manual order,
//! persisted in `layout.toml` under the state directory. Never stored in
//! session metadata and never sent to the daemon.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::config::PlacementRule;
use crate::filter::StoredFilter;
use crate::needs_you::NEEDS_YOU_ID;
use crate::session::{SessionKey, SessionRow};
use crate::sort_key;
use crate::view::View;

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
    /// The header shows with the session count and the rows are hidden.
    #[serde(default, skip_serializing_if = "is_false")]
    pub collapsed: bool,
}

/// A session's place: its group and its sort key within that group.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Item {
    pub host: String,
    pub session_id: String,
    pub group: String,
    pub order: String,
    /// Set once the owner has dragged the session, so placement rules leave
    /// it where the owner put it.
    #[serde(default, skip_serializing_if = "is_false")]
    pub manual: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
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
    /// Id of the selected view; kept as written so an unknown value survives
    /// until the owner picks a view.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    view: Option<String>,
    /// Filter of each view by view id; a view without an entry uses the
    /// default filter.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    filters: BTreeMap<String, StoredFilter>,
    /// Ids of the collapsed sections of each derived view by view id. My
    /// groups keeps its state on the groups themselves.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    collapsed_sections: BTreeMap<String, BTreeSet<String>>,
    #[serde(default)]
    groups: Vec<Group>,
    #[serde(default)]
    items: Vec<Item>,
}

/// One group with its sessions in display order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupSection {
    pub id: String,
    pub name: String,
    pub collapsed: bool,
    pub rows: Vec<SessionRow>,
}

/// Where a moved session lands in its target group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement<'a> {
    Before(&'a SessionKey),
    After(&'a SessionKey),
    /// After the last session; the only placement into an empty group.
    End,
}

/// Why a move was refused; the layout is left unchanged.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MoveError {
    #[error("session {0:?} is not in the layout")]
    UnknownSession(String),
    #[error("group {0:?} is not in the layout")]
    UnknownGroup(String),
    #[error("session {0:?} is not in the target group")]
    AnchorNotInGroup(String),
    #[error("sessions in group {0:?} share an order key; fix layout.toml by hand")]
    AmbiguousOrder(String),
}

/// Where a moved group lands among the others.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupPlacement<'a> {
    /// Before the group with this id.
    Before(&'a str),
    /// After the group with this id.
    After(&'a str),
}

/// Why a group operation was refused; the layout is left unchanged.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GroupError {
    #[error("a group name cannot be empty")]
    EmptyName,
    #[error("a group named {0:?} already exists")]
    DuplicateName(String),
    #[error("group {0:?} is not in the layout")]
    UnknownGroup(String),
    #[error("the Inbox group cannot be renamed or deleted")]
    InboxFixed,
    #[error("groups share an order key; fix layout.toml by hand")]
    AmbiguousOrder,
}

impl GroupError {
    /// A label for logs: names the refusal without the group name it carries.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::EmptyName => "empty name",
            Self::DuplicateName(_) => "duplicate name",
            Self::UnknownGroup(_) => "unknown group",
            Self::InboxFixed => "inbox is fixed",
            Self::AmbiguousOrder => "ambiguous order",
        }
    }
}

/// Why creating a group and moving a session into it was refused; the layout
/// is left unchanged.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CreateAndMoveError {
    #[error(transparent)]
    Group(#[from] GroupError),
    #[error(transparent)]
    Move(#[from] MoveError),
}

impl CreateAndMoveError {
    /// A label for logs that names the refusal without the group name.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Group(err) => err.kind(),
            Self::Move(_) => "move refused",
        }
    }
}

/// What a reconcile changed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Reconciled {
    pub added: Vec<SessionKey>,
    pub pruned: Vec<SessionKey>,
}

/// A session a placement pass moved: the rule that selected its group, or
/// `None` when it returned to Inbox, and the group id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RulePlacement {
    pub key: SessionKey,
    pub rule: Option<usize>,
    pub group: String,
}

/// Whether every condition of `rule` holds for `row`.
fn rule_matches(rule: &PlacementRule, row: &SessionRow) -> bool {
    let work = row.work.as_ref();
    let checks = [
        (
            &rule.project,
            row.project
                .as_ref()
                .map(|p| (p.label.as_str(), p.id.as_str())),
        ),
        (&rule.agent, Some((row.agent.as_str(), row.agent.as_str()))),
        (
            &rule.work_provider,
            work.and_then(|w| w.provider.as_deref()).map(|v| (v, v)),
        ),
        (
            &rule.work_kind,
            work.and_then(|w| w.kind.as_deref()).map(|v| (v, v)),
        ),
        (
            &rule.work_role,
            work.and_then(|w| w.role.as_deref()).map(|v| (v, v)),
        ),
    ];
    checks.into_iter().all(|(wanted, actual)| match wanted {
        None => true,
        Some(wanted) => actual.is_some_and(|(a, b)| a == wanted || b == wanted),
    })
}

impl Default for Layout {
    /// Inbox only.
    fn default() -> Self {
        Self {
            version: FORMAT_VERSION,
            view: None,
            filters: BTreeMap::new(),
            collapsed_sections: BTreeMap::new(),
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
        collapsed: false,
    }
}

/// The name as stored: trimmed and non-empty.
fn clean_name(name: &str) -> Result<&str, GroupError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(GroupError::EmptyName);
    }
    Ok(name)
}

impl Layout {
    /// The selected view; My groups when none is stored or the stored id is
    /// unknown.
    pub fn view(&self) -> View {
        self.view
            .as_deref()
            .and_then(View::parse)
            .unwrap_or_default()
    }

    /// The stored filter of a view; the default filter when none is stored.
    pub fn filter(&self, view: View) -> StoredFilter {
        self.filters.get(view.id()).cloned().unwrap_or_default()
    }

    /// Sections of the selected view for display; derived section titles are
    /// clipped to `title_max_chars`.
    pub fn present(&self, rows: &[SessionRow], title_max_chars: usize) -> Vec<GroupSection> {
        let view = self.view();
        let mut sections = view.sections(self.arrange(rows), title_max_chars);
        if view.is_editable() {
            return sections;
        }
        if let Some(collapsed) = self.collapsed_sections.get(view.id()) {
            for section in &mut sections {
                section.collapsed = section.id != NEEDS_YOU_ID && collapsed.contains(&section.id);
            }
        }
        sections
    }

    /// Drops stored collapsed ids of derived views that no section of the
    /// current `rows` has, so the file stays bounded by the live sections.
    /// Does nothing without rows, so a disconnected daemon keeps the state.
    /// Returns whether anything was dropped.
    pub fn prune_collapsed_sections(
        &mut self,
        rows: &[SessionRow],
        title_max_chars: usize,
    ) -> bool {
        if rows.is_empty() || self.collapsed_sections.is_empty() {
            return false;
        }
        let arranged = self.arrange(rows);
        let before: usize = self.collapsed_sections.values().map(BTreeSet::len).sum();
        for view in View::ALL.into_iter().filter(|v| !v.is_editable()) {
            let Some(collapsed) = self.collapsed_sections.get_mut(view.id()) else {
                continue;
            };
            let live: HashSet<String> = view
                .sections(arranged.clone(), title_max_chars)
                .into_iter()
                .map(|section| section.id)
                .collect();
            collapsed.retain(|id| live.contains(id));
        }
        self.collapsed_sections.retain(|_, ids| !ids.is_empty());
        let after: usize = self.collapsed_sections.values().map(BTreeSet::len).sum();
        after != before
    }

    pub fn groups(&self) -> &[Group] {
        &self.groups
    }

    pub fn items(&self) -> &[Item] {
        &self.items
    }

    /// Appends sessions the layout does not know to the end of Inbox, in list
    /// order, and drops placements of `host`'s sessions missing from
    /// `current`, the full seeded list of that host. Items of other hosts are
    /// never pruned, so a host that is disconnected or not yet seeded keeps
    /// its placements.
    pub fn reconcile(&mut self, host: &str, current: &[SessionKey]) -> Reconciled {
        let listed: HashSet<&SessionKey> = current.iter().collect();
        let mut result = Reconciled::default();
        self.items.retain(|item| {
            let keep = item.host != host || listed.contains(&item.key());
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
                manual: false,
            });
            result.added.push(key.clone());
        }
        result
    }

    /// Places every session the owner never dragged: the first rule in
    /// `rules` that matches selects the group (created when missing), else
    /// the session belongs in Inbox. A session already in its group keeps its
    /// position; any other is appended to the end of the group. Sessions
    /// without a placement are skipped; [`reconcile`](Self::reconcile) adds
    /// them first.
    pub fn apply_rules(
        &mut self,
        rows: &[SessionRow],
        rules: &[PlacementRule],
    ) -> Vec<RulePlacement> {
        let mut moved = Vec::new();
        for row in rows {
            let Some(index) = self.items.iter().position(|item| item.key() == row.key) else {
                continue;
            };
            if self.items[index].manual {
                continue;
            }
            let rule = rules.iter().position(|rule| rule_matches(rule, row));
            let group = match rule {
                Some(rule) => self.group_for_name(&rules[rule].group),
                None => INBOX_ID.to_owned(),
            };
            if self.items[index].group == group {
                continue;
            }
            let last = self
                .group_items(&group)
                .last()
                .map(|item| item.order.clone());
            let item = &mut self.items[index];
            item.order = sort_key::between(last.as_deref(), None);
            item.group = group.clone();
            moved.push(RulePlacement {
                key: row.key.clone(),
                rule,
                group,
            });
        }
        moved
    }

    /// Clears the manual flag of `row`'s item and places it by `rules` like
    /// any session the owner never dragged (see [`apply_rules`](Self::apply_rules)).
    /// Returns whether the item changed; a session that is not manual is left
    /// alone.
    pub fn return_to_rule(
        &mut self,
        row: &SessionRow,
        rules: &[PlacementRule],
    ) -> Result<bool, MoveError> {
        let item = self
            .items
            .iter_mut()
            .find(|item| item.key() == row.key)
            .ok_or_else(|| MoveError::UnknownSession(row.key.session_id.clone()))?;
        if !std::mem::take(&mut item.manual) {
            return Ok(false);
        }
        self.apply_rules(std::slice::from_ref(row), rules);
        Ok(true)
    }

    /// Id of the group named `name` (ignoring case); appended after the last
    /// group when no group has that name.
    fn group_for_name(&mut self, name: &str) -> String {
        let name = name.trim();
        if let Some(group) = self
            .groups
            .iter()
            .find(|group| group.name.to_lowercase() == name.to_lowercase())
        {
            return group.id.clone();
        }
        let slug: String = name
            .chars()
            .map(|c| {
                if c.is_alphanumeric() {
                    c.to_ascii_lowercase()
                } else {
                    '-'
                }
            })
            .collect();
        let base = format!("rule-{}", slug.trim_matches('-'));
        let mut id = base.clone();
        let mut n = 1;
        while self.groups.iter().any(|group| group.id == id) {
            n += 1;
            id = format!("{base}-{n}");
        }
        let last = self.groups.iter().map(|g| g.order.as_str()).max();
        let order = sort_key::between(last, None);
        self.groups.push(Group {
            id: id.clone(),
            name: name.to_owned(),
            order,
            collapsed: false,
        });
        id
    }

    /// Items of `group` in display order.
    fn group_items(&self, group: &str) -> Vec<&Item> {
        let mut items: Vec<&Item> = self.items.iter().filter(|i| i.group == group).collect();
        items.sort_by(|a, b| {
            (&a.order, &a.host, &a.session_id).cmp(&(&b.order, &b.host, &b.session_id))
        });
        items
    }

    /// Moves `key` into `group` at `placement` and marks it manually placed.
    /// Only that session's item changes: its group and one new sort key.
    /// Returns whether anything changed. A drop where the session already is,
    /// or on itself, keeps its position but still marks it manually placed,
    /// so only the flag changes.
    pub fn move_session(
        &mut self,
        key: &SessionKey,
        group: &str,
        placement: Placement<'_>,
    ) -> Result<bool, MoveError> {
        if !self.groups.iter().any(|g| g.id == group) {
            return Err(MoveError::UnknownGroup(group.to_owned()));
        }
        let index = self
            .items
            .iter()
            .position(|item| item.key() == *key)
            .ok_or_else(|| MoveError::UnknownSession(key.session_id.clone()))?;
        let anchor = match placement {
            Placement::Before(anchor) | Placement::After(anchor) => Some(anchor),
            Placement::End => None,
        };
        if anchor == Some(key) {
            return Ok(self.pin(index));
        }
        let neighbours: Vec<&Item> = self
            .group_items(group)
            .into_iter()
            .filter(|item| item.key() != *key)
            .collect();
        let (lower, upper) = match (placement, anchor) {
            (Placement::End, _) | (_, None) => (neighbours.last().map(|i| i.order.as_str()), None),
            (placement, Some(anchor)) => {
                let at = neighbours
                    .iter()
                    .position(|item| item.key() == *anchor)
                    .ok_or_else(|| MoveError::AnchorNotInGroup(anchor.session_id.clone()))?;
                let slot = at + usize::from(matches!(placement, Placement::After(_)));
                (
                    slot.checked_sub(1).map(|i| neighbours[i].order.as_str()),
                    neighbours.get(slot).map(|i| i.order.as_str()),
                )
            }
        };
        let current = &self.items[index];
        let stays = current.group == group
            && lower.is_none_or(|lower| lower < current.order.as_str())
            && upper.is_none_or(|upper| current.order.as_str() < upper);
        if stays {
            return Ok(self.pin(index));
        }
        if let (Some(lower), Some(upper)) = (lower, upper)
            && lower >= upper
        {
            return Err(MoveError::AmbiguousOrder(group.to_owned()));
        }
        let order = sort_key::between(lower, upper);
        let item = &mut self.items[index];
        item.group = group.to_owned();
        item.order = order;
        item.manual = true;
        Ok(true)
    }

    /// Marks the item manually placed; returns whether the flag changed.
    fn pin(&mut self, index: usize) -> bool {
        !std::mem::replace(&mut self.items[index].manual, true)
    }

    fn group_index(&self, id: &str) -> Result<usize, GroupError> {
        self.groups
            .iter()
            .position(|group| group.id == id)
            .ok_or_else(|| GroupError::UnknownGroup(id.to_owned()))
    }

    /// Refuses a name another group (other than `except`) has, ignoring case:
    /// placement rules address groups by name.
    fn check_unique(&self, name: &str, except: Option<&str>) -> Result<(), GroupError> {
        let folded = name.to_lowercase();
        if self
            .groups
            .iter()
            .any(|g| Some(g.id.as_str()) != except && g.name.to_lowercase() == folded)
        {
            return Err(GroupError::DuplicateName(name.to_owned()));
        }
        Ok(())
    }

    /// Groups in display order.
    pub fn ordered_groups(&self) -> Vec<&Group> {
        let mut groups: Vec<&Group> = self.groups.iter().collect();
        groups.sort_by(|a, b| (&a.order, &a.id).cmp(&(&b.order, &b.id)));
        groups
    }

    /// Appends a group after the last one and returns its id.
    pub fn create_group(&mut self, name: &str) -> Result<String, GroupError> {
        let name = clean_name(name)?;
        self.check_unique(name, None)?;
        let id = (self.groups.len()..)
            .map(|n| format!("g{n}"))
            .find(|id| self.groups.iter().all(|g| g.id != *id))
            .expect("an unused group id exists");
        let last = self.ordered_groups().last().map(|g| g.order.clone());
        self.groups.push(Group {
            id: id.clone(),
            name: name.to_owned(),
            order: sort_key::between(last.as_deref(), None),
            collapsed: false,
        });
        Ok(id)
    }

    /// Appends a group and moves `key` to its end as one change: on any
    /// refusal the layout is left as it was. Returns the new group's id.
    pub fn create_group_and_move(
        &mut self,
        name: &str,
        key: &SessionKey,
    ) -> Result<String, CreateAndMoveError> {
        if !self.items.iter().any(|item| item.key() == *key) {
            return Err(MoveError::UnknownSession(key.session_id.clone()).into());
        }
        let before = self.clone();
        let outcome = self
            .create_group(name)
            .map_err(CreateAndMoveError::from)
            .and_then(|id| {
                self.move_session(key, &id, Placement::End)?;
                Ok(id)
            });
        if outcome.is_err() {
            *self = before;
        }
        outcome
    }

    /// Renames a group; Inbox keeps its name. Returns whether the name changed.
    pub fn rename_group(&mut self, id: &str, name: &str) -> Result<bool, GroupError> {
        let index = self.group_index(id)?;
        if id == INBOX_ID {
            return Err(GroupError::InboxFixed);
        }
        let name = clean_name(name)?;
        self.check_unique(name, Some(id))?;
        if self.groups[index].name == name {
            return Ok(false);
        }
        self.groups[index].name = name.to_owned();
        Ok(true)
    }

    /// Collapses or expands a section of the selected view: an owner group in
    /// My groups, a section id in a derived view. Returns whether the
    /// collapsed state changed.
    pub fn set_collapsed(&mut self, id: &str, collapsed: bool) -> Result<bool, GroupError> {
        let view = self.view();
        if !view.is_editable() {
            let ids = self
                .collapsed_sections
                .entry(view.id().to_owned())
                .or_default();
            let changed = if collapsed {
                ids.insert(id.to_owned())
            } else {
                ids.remove(id)
            };
            if ids.is_empty() {
                self.collapsed_sections.remove(view.id());
            }
            return Ok(changed);
        }
        let index = self.group_index(id)?;
        let group = &mut self.groups[index];
        let changed = group.collapsed != collapsed;
        group.collapsed = collapsed;
        Ok(changed)
    }

    /// Moves a group before or after another; only that group's sort key
    /// changes. Returns whether anything changed.
    pub fn move_group(
        &mut self,
        id: &str,
        placement: GroupPlacement<'_>,
    ) -> Result<bool, GroupError> {
        let index = self.group_index(id)?;
        let (GroupPlacement::Before(anchor) | GroupPlacement::After(anchor)) = placement;
        self.group_index(anchor)?;
        if anchor == id {
            return Ok(false);
        }
        let others: Vec<&Group> = self
            .ordered_groups()
            .into_iter()
            .filter(|g| g.id != id)
            .collect();
        let at = others
            .iter()
            .position(|g| g.id == anchor)
            .ok_or_else(|| GroupError::UnknownGroup(anchor.to_owned()))?;
        let slot = at + usize::from(matches!(placement, GroupPlacement::After(_)));
        let lower = slot.checked_sub(1).map(|i| others[i].order.as_str());
        let upper = others.get(slot).map(|g| g.order.as_str());
        let current = self.groups[index].order.as_str();
        if lower.is_none_or(|lower| lower < current) && upper.is_none_or(|upper| current < upper) {
            return Ok(false);
        }
        if let (Some(lower), Some(upper)) = (lower, upper)
            && lower >= upper
        {
            return Err(GroupError::AmbiguousOrder);
        }
        let order = sort_key::between(lower, upper);
        self.groups[index].order = order;
        Ok(true)
    }

    /// Deletes a group and appends its sessions to the end of Inbox in their
    /// display order, keeping their manual flag. Returns the moved sessions.
    pub fn delete_group(&mut self, id: &str) -> Result<Vec<SessionKey>, GroupError> {
        let index = self.group_index(id)?;
        if id == INBOX_ID {
            return Err(GroupError::InboxFixed);
        }
        let moving: Vec<SessionKey> = self.group_items(id).iter().map(|i| i.key()).collect();
        let mut last = self
            .items
            .iter()
            .filter(|item| item.group == INBOX_ID)
            .map(|item| item.order.clone())
            .max();
        for key in &moving {
            let order = sort_key::between(last.as_deref(), None);
            last = Some(order.clone());
            if let Some(item) = self.items.iter_mut().find(|item| item.key() == *key) {
                item.group = INBOX_ID.to_owned();
                item.order = order;
            }
        }
        self.groups.remove(index);
        Ok(moving)
    }

    /// Groups in order, each with the rows of its items in order. A row
    /// without a placement is left out until the next reconcile.
    pub fn arrange(&self, rows: &[SessionRow]) -> Vec<GroupSection> {
        let by_key: HashMap<&SessionKey, &SessionRow> =
            rows.iter().map(|row| (&row.key, row)).collect();
        self.ordered_groups()
            .into_iter()
            .map(|group| {
                let items = self.group_items(&group.id);
                GroupSection {
                    id: group.id.clone(),
                    name: group.name.clone(),
                    collapsed: group.collapsed,
                    rows: items
                        .into_iter()
                        .filter_map(|item| by_key.get(&item.key()).map(|row| (*row).clone()))
                        .collect(),
                }
            })
            .collect()
    }

    /// Rejects an inconsistent hand-edited layout and re-adds a deleted Inbox;
    /// returns whether it changed anything.
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
            if !ids.contains(item.group.as_str()) {
                return Err(format!(
                    "session {:?} is in unknown group {:?}",
                    item.session_id, item.group
                ));
            }
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
        if let Some(stored) = self.view.as_deref()
            && View::parse(stored).is_none()
        {
            warn!(
                view = stored,
                "unknown view in the layout file; showing My groups"
            );
        }
        let mut repaired = false;
        if !self.groups.iter().any(|group| group.id == INBOX_ID) {
            let first = self.groups.iter().map(|group| group.order.as_str()).min();
            let order = sort_key::between(None, first);
            self.groups.insert(0, inbox_group(order));
            warn!("the layout file has no Inbox group; adding it");
            repaired = true;
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

    /// Reconciles with the full session list of `host` and logs the outcome.
    pub fn reconcile(&mut self, host: &str, current: &[SessionKey]) -> Reconciled {
        let result = self.layout.reconcile(host, current);
        if !result.added.is_empty() || !result.pruned.is_empty() {
            info!(
                host,
                added = ?result.added.iter().map(|k| k.session_id.as_str()).collect::<Vec<_>>(),
                pruned = ?result.pruned.iter().map(|k| k.session_id.as_str()).collect::<Vec<_>>(),
                "layout reconciled with the session list"
            );
        }
        result
    }

    /// Selects the view; saved by the next [`save_if_changed`](Self::save_if_changed).
    pub fn set_view(&mut self, view: View) {
        if self.layout.view != Some(view.id().to_owned()) {
            info!(view = view.id(), "view selected");
        }
        self.layout.view = Some(view.id().to_owned());
    }

    /// Stores a view's filter; saved by the next
    /// [`save_if_changed`](Self::save_if_changed). The default filter is
    /// stored as no entry.
    pub fn set_filter(&mut self, view: View, filter: StoredFilter) {
        if self.layout.filter(view) == filter {
            return;
        }
        info!(view = view.id(), "view filter changed");
        if filter == StoredFilter::default() {
            self.layout.filters.remove(view.id());
        } else {
            self.layout.filters.insert(view.id().to_owned(), filter);
        }
    }

    /// Applies the placement rules (see [`Layout::apply_rules`]) and logs
    /// each placement with the session key, rule index, and group id.
    pub fn apply_rules(
        &mut self,
        rows: &[SessionRow],
        rules: &[PlacementRule],
    ) -> Vec<RulePlacement> {
        let moved = self.layout.apply_rules(rows, rules);
        for placement in &moved {
            info!(
                host = %placement.key.host,
                session = %placement.key.session_id,
                rule = ?placement.rule,
                group = %placement.group,
                "session placed by rule"
            );
        }
        moved
    }

    /// Moves a session (see [`Layout::move_session`]) and logs it with keys
    /// and group ids only. The change reaches the file at the next
    /// [`save_if_changed`](Self::save_if_changed).
    pub fn move_session(
        &mut self,
        key: &SessionKey,
        group: &str,
        placement: Placement<'_>,
    ) -> Result<bool, MoveError> {
        let changed = self.layout.move_session(key, group, placement)?;
        if changed {
            info!(session = %key.session_id, group, placement = ?placement_log(placement), "session moved");
        }
        Ok(changed)
    }

    /// Hands a manually placed session back to the placement rules (see
    /// [`Layout::return_to_rule`]) and logs the user action with keys and the
    /// resulting group id only.
    pub fn return_to_rule(
        &mut self,
        row: &SessionRow,
        rules: &[PlacementRule],
    ) -> Result<bool, MoveError> {
        let changed = self.layout.return_to_rule(row, rules)?;
        if changed {
            let group = self
                .layout
                .items()
                .iter()
                .find(|item| item.key() == row.key)
                .map(|item| item.group.as_str())
                .unwrap_or_default();
            info!(host = %row.key.host, session = %row.key.session_id, group, "session returned to rule placement");
        }
        Ok(changed)
    }

    /// Creates a group and returns its id. Group operations on the store log
    /// the group id and the operation, never the name; each change reaches
    /// the file at the next [`save_if_changed`](Self::save_if_changed).
    pub fn create_group(&mut self, name: &str) -> Result<String, GroupError> {
        let id = self.layout.create_group(name)?;
        info!(group = %id, operation = "create", "group changed");
        Ok(id)
    }

    /// Creates a group and moves a session to its end (see
    /// [`Layout::create_group_and_move`]); logs keys and the group id only.
    pub fn create_group_and_move(
        &mut self,
        name: &str,
        key: &SessionKey,
    ) -> Result<String, CreateAndMoveError> {
        let id = self.layout.create_group_and_move(name, key)?;
        info!(group = %id, session = %key.session_id, operation = "create and move", "group changed");
        Ok(id)
    }

    pub fn rename_group(&mut self, id: &str, name: &str) -> Result<bool, GroupError> {
        let changed = self.layout.rename_group(id, name)?;
        if changed {
            info!(group = %id, operation = "rename", "group changed");
        }
        Ok(changed)
    }

    pub fn set_collapsed(&mut self, id: &str, collapsed: bool) -> Result<bool, GroupError> {
        let changed = self.layout.set_collapsed(id, collapsed)?;
        if changed {
            let operation = if collapsed { "collapse" } else { "expand" };
            info!(group = %id, view = self.layout.view().id(), operation, "group changed");
        }
        Ok(changed)
    }

    /// Drops collapsed ids that no section of `rows` has any more; saved by
    /// the next [`save_if_changed`](Self::save_if_changed).
    pub fn prune_collapsed_sections(&mut self, rows: &[SessionRow], title_max_chars: usize) {
        if self.layout.prune_collapsed_sections(rows, title_max_chars) {
            info!("stale collapsed sections dropped");
        }
    }

    pub fn move_group(
        &mut self,
        id: &str,
        placement: GroupPlacement<'_>,
    ) -> Result<bool, GroupError> {
        let changed = self.layout.move_group(id, placement)?;
        if changed {
            info!(group = %id, operation = "move", "group changed");
        }
        Ok(changed)
    }

    pub fn delete_group(&mut self, id: &str) -> Result<Vec<SessionKey>, GroupError> {
        let moved = self.layout.delete_group(id)?;
        info!(group = %id, operation = "delete", sessions = moved.len(), "group changed");
        Ok(moved)
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

fn placement_log(placement: Placement<'_>) -> String {
    match placement {
        Placement::Before(anchor) => format!("before {}", anchor.session_id),
        Placement::After(anchor) => format!("after {}", anchor.session_id),
        Placement::End => "end".to_owned(),
    }
}

/// Writes `text` to a temporary file next to `path` and renames it over
/// `path`, creating the directory with owner-only access.
pub(crate) fn write_atomic(path: &Path, text: &str) -> io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::other("layout path has no directory"))?;
    DirBuilder::new()
        .recursive(true)
        .mode(DIR_MODE)
        .create(dir)?;
    fs::set_permissions(dir, fs::Permissions::from_mode(DIR_MODE))?;
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let tmp = dir.join(format!("{name}.tmp-{}", std::process::id()));
    // A stale temporary file from a crashed run is replaced first.
    let _ = fs::remove_file(&tmp);
    let written = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
            .mode(FILE_MODE)
            .open(&tmp)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        fs::rename(&tmp, path)?;
        fs::File::open(dir)?.sync_all()
    })();
    if written.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    written
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter::{ProjectSelection, SessionFilter};
    use crate::owner_state::OwnerState;

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
        layout.reconcile("local", &keys(&["s1", "s2", "s3"]));
        assert_eq!(inbox_order(&layout), ["s1", "s2", "s3"]);

        let result = layout.reconcile("local", &keys(&["s3", "s1", "s4", "s5"]));
        assert_eq!(result.pruned, keys(&["s2"]));
        assert_eq!(result.added, keys(&["s4", "s5"]));
        assert_eq!(inbox_order(&layout), ["s1", "s3", "s4", "s5"]);
    }

    fn remote_key(host: &str, id: &str) -> SessionKey {
        SessionKey {
            host: host.to_owned(),
            session_id: id.to_owned(),
        }
    }

    #[test]
    fn a_host_seed_prunes_only_that_hosts_items() {
        let a = |ids: &[&str]| -> Vec<SessionKey> {
            ids.iter().map(|id| remote_key("host-a", id)).collect()
        };
        let b = |ids: &[&str]| -> Vec<SessionKey> {
            ids.iter().map(|id| remote_key("host-b", id)).collect()
        };
        let mut layout = Layout::default();
        layout.reconcile("host-a", &a(&["a1", "a2"]));
        layout.reconcile("host-b", &b(&["b1", "b2"]));
        assert_eq!(inbox_order(&layout), ["a1", "a2", "b1", "b2"]);

        // Host A lost a2 and gained a3; host B was not seeded again.
        let result = layout.reconcile("host-a", &a(&["a1", "a3"]));
        assert_eq!(result.pruned, a(&["a2"]));
        assert_eq!(result.added, a(&["a3"]));
        assert_eq!(inbox_order(&layout), ["a1", "b1", "b2", "a3"]);

        // An empty seed of B empties B only; a host never seeded (C) prunes
        // nothing and no host is touched by a reconcile of another.
        layout.reconcile("host-b", &[]);
        assert_eq!(inbox_order(&layout), ["a1", "a3"]);
        let before = layout.items().to_vec();
        let result = layout.reconcile("host-c", &[]);
        assert!(result.pruned.is_empty() && result.added.is_empty());
        assert_eq!(layout.items(), before);
    }

    const THREE_GROUPS: &str = r#"
version = 1
[[groups]]
id = "inbox"
name = "Inbox"
order = "g"
[[groups]]
id = "work"
name = "Work"
order = "n"
[[groups]]
id = "empty"
name = "Empty"
order = "t"
"#;

    fn arranged(ids: &[&str]) -> Layout {
        let mut layout: Layout = toml::from_str(THREE_GROUPS).expect("parses");
        layout.reconcile("local", &keys(ids));
        layout
    }

    fn order_of(layout: &Layout, group: &str) -> Vec<String> {
        layout
            .group_items(group)
            .iter()
            .map(|item| item.session_id.clone())
            .collect()
    }

    #[test]
    fn a_move_before_after_or_to_the_end_orders_within_and_across_groups() {
        let mut layout = arranged(&["a", "b", "c", "d"]);
        let move_to = |layout: &mut Layout, id: &str, group: &str, p: Placement<'_>| {
            assert_eq!(layout.move_session(&key(id), group, p), Ok(true));
        };
        move_to(&mut layout, "d", INBOX_ID, Placement::Before(&key("a")));
        assert_eq!(order_of(&layout, INBOX_ID), ["d", "a", "b", "c"]);
        move_to(&mut layout, "d", INBOX_ID, Placement::After(&key("b")));
        assert_eq!(order_of(&layout, INBOX_ID), ["a", "b", "d", "c"]);
        move_to(&mut layout, "a", INBOX_ID, Placement::End);
        assert_eq!(order_of(&layout, INBOX_ID), ["b", "d", "c", "a"]);

        move_to(&mut layout, "b", "empty", Placement::End);
        assert_eq!(order_of(&layout, "empty"), ["b"]);
        move_to(&mut layout, "c", "work", Placement::End);
        move_to(&mut layout, "a", "work", Placement::Before(&key("c")));
        move_to(&mut layout, "d", "work", Placement::After(&key("a")));
        assert_eq!(order_of(&layout, "work"), ["a", "d", "c"]);
        assert!(order_of(&layout, INBOX_ID).is_empty());
    }

    #[test]
    fn a_move_changes_one_item_and_marks_it_manual() {
        let mut layout = arranged(&["a", "b", "c"]);
        let before = layout.clone();
        layout
            .move_session(&key("c"), "work", Placement::End)
            .expect("moves");
        for (old, new) in before.items().iter().zip(layout.items()) {
            if old.session_id == "c" {
                assert_eq!((new.group.as_str(), new.manual), ("work", true));
            } else {
                assert_eq!(old, new);
            }
        }
        let text = layout.to_toml();
        assert_eq!(text.matches("manual = true").count(), 1);
    }

    #[test]
    fn dropping_a_moved_session_where_it_is_or_on_itself_changes_nothing() {
        let mut layout = arranged(&["a", "b", "c"]);
        layout
            .move_session(&key("b"), INBOX_ID, Placement::After(&key("a")))
            .expect("moves");
        let settled = layout.clone();
        for placement in [
            Placement::After(&key("a")),
            Placement::Before(&key("c")),
            Placement::Before(&key("b")),
        ] {
            assert_eq!(
                layout.move_session(&key("b"), INBOX_ID, placement),
                Ok(false)
            );
        }
        assert_eq!(layout, settled);
    }

    #[test]
    fn a_move_between_neighbours_with_equal_keys_is_refused() {
        let mut layout = arranged(&["a", "b", "c"]);
        let same = layout.items()[0].order.clone();
        layout.items[1].order = same;
        let before = layout.clone();
        assert_eq!(
            layout.move_session(&key("c"), INBOX_ID, Placement::Before(&key("b"))),
            Err(MoveError::AmbiguousOrder(INBOX_ID.to_owned()))
        );
        assert_eq!(layout, before);
    }

    #[test]
    fn a_refused_move_leaves_the_layout_unchanged() {
        let mut layout = arranged(&["a", "b"]);
        let before = layout.clone();
        assert_eq!(
            layout.move_session(&key("zz"), INBOX_ID, Placement::End),
            Err(MoveError::UnknownSession("zz".to_owned()))
        );
        assert_eq!(
            layout.move_session(&key("a"), "nope", Placement::End),
            Err(MoveError::UnknownGroup("nope".to_owned()))
        );
        assert_eq!(
            layout.move_session(&key("a"), "work", Placement::Before(&key("b"))),
            Err(MoveError::AnchorNotInGroup("b".to_owned()))
        );
        assert_eq!(layout, before);
    }

    fn group_ids(layout: &Layout) -> Vec<String> {
        layout.arrange(&[]).into_iter().map(|s| s.id).collect()
    }

    #[test]
    fn deleting_a_group_appends_its_sessions_to_inbox_in_order_keeping_the_manual_flag() {
        let mut layout = arranged(&["a", "b", "c", "d"]);
        for id in ["c", "a", "d"] {
            layout
                .move_session(&key(id), "work", Placement::End)
                .expect("moves");
        }
        assert_eq!(order_of(&layout, "work"), ["c", "a", "d"]);
        let moved = layout.delete_group("work").expect("deletes");
        assert_eq!(moved, keys(&["c", "a", "d"]));
        assert_eq!(order_of(&layout, INBOX_ID), ["b", "c", "a", "d"]);
        assert_eq!(group_ids(&layout), [INBOX_ID, "empty"]);
        assert!(layout.items().iter().filter(|i| i.manual).count() == 3);
    }

    #[test]
    fn inbox_cannot_be_renamed_or_deleted_and_an_unknown_group_is_refused() {
        let mut layout = arranged(&["a"]);
        let before = layout.clone();
        assert_eq!(layout.delete_group(INBOX_ID), Err(GroupError::InboxFixed));
        assert_eq!(
            layout.rename_group(INBOX_ID, "Main"),
            Err(GroupError::InboxFixed)
        );
        assert_eq!(
            layout.delete_group("nope"),
            Err(GroupError::UnknownGroup("nope".to_owned()))
        );
        assert_eq!(layout, before);
    }

    #[test]
    fn group_names_are_trimmed_and_must_be_non_empty_and_unique_ignoring_case() {
        let mut layout = arranged(&[]);
        let before = layout.clone();
        assert_eq!(layout.create_group("  \t "), Err(GroupError::EmptyName));
        assert_eq!(
            layout.create_group("WORK"),
            Err(GroupError::DuplicateName("WORK".to_owned()))
        );
        assert_eq!(
            layout.create_group(" inbox "),
            Err(GroupError::DuplicateName("inbox".to_owned()))
        );
        assert_eq!(
            layout.rename_group("empty", "work"),
            Err(GroupError::DuplicateName("work".to_owned()))
        );
        assert_eq!(
            layout.rename_group("empty", " "),
            Err(GroupError::EmptyName)
        );
        assert_eq!(layout, before);

        let id = layout.create_group("  Review ").expect("creates");
        assert_eq!(group_ids(&layout), [INBOX_ID, "work", "empty", id.as_str()]);
        assert_eq!(
            layout.groups().last().map(|g| g.name.as_str()),
            Some("Review")
        );
        // Renaming a group to its own name in another case is allowed.
        assert_eq!(layout.rename_group(&id, "REVIEW"), Ok(true));
        assert_eq!(layout.rename_group(&id, "REVIEW"), Ok(false));
    }

    #[test]
    fn create_and_move_adds_one_group_and_one_manual_item_or_changes_nothing() {
        let mut layout = arranged(&["a", "b"]);
        let before = layout.clone();
        for name in ["", "  ", "work", "INBOX"] {
            assert!(layout.create_group_and_move(name, &key("a")).is_err());
            assert_eq!(layout, before);
        }
        assert!(layout.create_group_and_move("Builds", &key("zz")).is_err());
        assert_eq!(layout, before);

        let id = layout
            .create_group_and_move(" Builds ", &key("a"))
            .expect("creates and moves");
        assert_eq!(layout.groups().len(), before.groups().len() + 1);
        assert_eq!(
            layout.groups().last().map(|g| g.id.as_str()),
            Some(id.as_str())
        );
        let moved: Vec<_> = layout.items().iter().filter(|i| i.group == id).collect();
        assert_eq!(moved.len(), 1);
        assert_eq!(moved[0].key(), key("a"));
        assert!(moved[0].manual);
    }

    #[test]
    fn collapsed_sections_persist_per_view_and_stale_ids_are_dropped() {
        let dir = scratch("collapsed-sections");
        let path = dir.join(LAYOUT_FILE);
        let (mut store, _) = LayoutStore::open(path.clone());
        let rows = [
            session("s1", Some("alpha"), None),
            session("s2", Some("beta"), None),
        ];
        let keys: Vec<SessionKey> = rows.iter().map(|r| r.key.clone()).collect();
        store.reconcile("local", &keys);
        store.set_view(View::ByProject);
        let alpha = store.layout().present(&rows, 40)[0].id.clone();
        assert_eq!(store.set_collapsed(&alpha, true), Ok(true));
        assert_eq!(store.set_collapsed(&alpha, true), Ok(false));
        assert_eq!(store.set_collapsed("gone", true), Ok(true));
        store.prune_collapsed_sections(&rows, 40);
        assert!(store.save_if_changed().expect("saves"));

        let (mut reloaded, error) = LayoutStore::open(path);
        assert!(error.is_none());
        let collapsed = |layout: &Layout| -> Vec<bool> {
            layout
                .present(&rows, 40)
                .iter()
                .map(|s| s.collapsed)
                .collect()
        };
        assert_eq!(collapsed(reloaded.layout()), [true, false]);
        reloaded.set_view(View::ByState);
        assert!(collapsed(reloaded.layout()).iter().all(|c| !c));
        reloaded.set_view(View::ByProject);
        assert_eq!(reloaded.set_collapsed(&alpha, false), Ok(true));
        assert!(collapsed(reloaded.layout()).iter().all(|c| !c));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn collapsed_state_and_group_order_survive_a_save_and_load() {
        let dir = scratch("groups");
        let path = dir.join(LAYOUT_FILE);
        let (mut store, _) = LayoutStore::open(path.clone());
        store.reconcile("local", &keys(&["s1"]));
        let work = store.create_group("Work").expect("creates");
        let review = store.create_group("Review").expect("creates");
        assert_eq!(
            store.move_group(&review, GroupPlacement::Before(INBOX_ID)),
            Ok(true)
        );
        assert_eq!(store.set_collapsed(&work, true), Ok(true));
        assert_eq!(store.set_collapsed(&work, true), Ok(false));
        assert!(store.save_if_changed().expect("saves"));

        let (reloaded, error) = LayoutStore::open(path);
        assert!(error.is_none());
        assert_eq!(reloaded.layout(), store.layout());
        let sections = reloaded.layout().arrange(&[]);
        assert_eq!(
            sections.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            ["Review", "Inbox", "Work"]
        );
        assert_eq!(
            sections.iter().map(|s| s.collapsed).collect::<Vec<_>>(),
            [false, false, true]
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn moving_a_group_changes_only_its_key_and_a_no_op_move_changes_nothing() {
        let mut layout = arranged(&[]);
        let before = layout.clone();
        assert_eq!(
            layout.move_group("work", GroupPlacement::After(INBOX_ID)),
            Ok(false)
        );
        assert_eq!(
            layout.move_group("work", GroupPlacement::Before("empty")),
            Ok(false)
        );
        assert_eq!(
            layout.move_group("work", GroupPlacement::Before("work")),
            Ok(false)
        );
        assert_eq!(layout, before);

        assert_eq!(
            layout.move_group("empty", GroupPlacement::Before(INBOX_ID)),
            Ok(true)
        );
        assert_eq!(group_ids(&layout), ["empty", INBOX_ID, "work"]);
        for (old, new) in before.groups().iter().zip(layout.groups()) {
            if new.id != "empty" {
                assert_eq!(old, new);
            }
        }
    }

    #[test]
    fn a_manual_flag_survives_save_and_load() {
        let dir = scratch("manual");
        let path = dir.join(LAYOUT_FILE);
        let (mut store, _) = LayoutStore::open(path.clone());
        store.reconcile("local", &keys(&["s1", "s2"]));
        store
            .move_session(&key("s1"), INBOX_ID, Placement::End)
            .expect("moves");
        assert!(store.save_if_changed().expect("saves"));
        let (reloaded, error) = LayoutStore::open(path);
        assert!(error.is_none());
        assert_eq!(reloaded.layout(), store.layout());
        assert!(reloaded.layout().items().iter().any(|i| i.manual));
        let _ = fs::remove_dir_all(&dir);
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
        layout.reconcile("local", &keys(&["s1", "s2", "s3"]));
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
        store.reconcile("local", &keys(&["s1", "s2"]));
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
        reloaded.reconcile("local", &keys(&["s1", "s2"]));
        assert!(!reloaded.save_if_changed().expect("no rewrite"));

        let annotated = format!(
            "# hand-written note\n{}",
            fs::read_to_string(&path).expect("read")
        );
        fs::write(&path, &annotated).expect("write");
        let (mut edited, error) = LayoutStore::open(path.clone());
        assert!(error.is_none());
        edited.reconcile("local", &keys(&["s1", "s2"]));
        assert!(!edited.save_if_changed().expect("no rewrite"));
        assert_eq!(fs::read_to_string(&path).expect("read"), annotated);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn each_views_filter_persists_separately() {
        let dir = scratch("filters");
        let path = dir.join("layout.toml");
        let (mut store, error) = LayoutStore::open(path.clone());
        assert!(error.is_none());
        let mut filter = SessionFilter::default();
        filter.toggle_state(OwnerState::Ended);
        filter.set_project(ProjectSelection::NoProject);
        store.set_filter(View::ByState, filter.to_stored());
        store.save_if_changed().expect("saves");
        let (reloaded, error) = LayoutStore::open(path);
        assert!(error.is_none());
        let restored = SessionFilter::from_stored(&reloaded.layout().filter(View::ByState));
        assert_eq!(restored.states(), filter.states());
        assert_eq!(restored.project(), &ProjectSelection::NoProject);
        assert_eq!(
            reloaded.layout().filter(View::ByProject),
            StoredFilter::default()
        );
    }

    #[test]
    fn an_unknown_stored_view_loads_as_my_groups_and_a_selected_view_persists() {
        let dir = scratch("view");
        fs::create_dir_all(&dir).expect("dir");
        let path = dir.join(LAYOUT_FILE);
        fs::write(
            &path,
            "version = 1\nview = \"by-banana\"\n[[groups]]\nid = \"inbox\"\nname = \"Inbox\"\norder = \"n\"\n",
        )
        .expect("write");
        let (mut store, error) = LayoutStore::open(path.clone());
        assert!(error.is_none());
        assert_eq!(store.layout().view(), View::MyGroups);

        store.set_view(View::ByState);
        assert!(store.save_if_changed().expect("saves"));
        let (reloaded, error) = LayoutStore::open(path);
        assert!(error.is_none());
        assert_eq!(reloaded.layout().view(), View::ByState);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_session_in_an_unknown_group_is_reported_and_the_file_kept() {
        let dir = scratch("unknown-group");
        fs::create_dir_all(&dir).expect("dir");
        let path = dir.join(LAYOUT_FILE);
        let typo = "version = 1\n[[groups]]\nid = \"inbox\"\nname = \"Inbox\"\norder = \"n\"\n\
            [[items]]\nhost = \"local\"\nsession_id = \"s1\"\ngroup = \"wrok\"\norder = \"n\"\n";
        fs::write(&path, typo).expect("write");
        let (mut store, error) = LayoutStore::open(path.clone());
        assert!(matches!(error, Some(LayoutError::Invalid { .. })));
        store.reconcile("local", &keys(&["s1"]));
        assert!(!store.save_if_changed().expect("skips"));
        assert_eq!(fs::read_to_string(&path).expect("read"), typo);
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
        store.reconcile("local", &keys(&["s1"]));
        assert_eq!(store.layout().arrange(&[]).len(), 1);
        assert!(!store.save_if_changed().expect("skips"));
        assert_eq!(fs::read_to_string(&path).expect("read"), damaged);
        let _ = fs::remove_dir_all(&dir);
    }

    fn rule(group: &str, patch: impl FnOnce(&mut PlacementRule)) -> PlacementRule {
        let mut rule = PlacementRule {
            group: group.to_owned(),
            project: None,
            agent: None,
            work_provider: None,
            work_kind: None,
            work_role: None,
        };
        patch(&mut rule);
        rule
    }

    fn session(id: &str, project: Option<&str>, role: Option<&str>) -> SessionRow {
        SessionRow {
            key: key(id),
            display_name: id.to_owned(),
            agent: "claude".to_owned(),
            state: "running".to_owned(),
            owner_state: crate::owner_state::OwnerState::Working,
            waiting_since: None,
            badge: None,
            project: project.map(|p| crate::session::ProjectRef {
                id: format!("id-{p}"),
                label: p.to_owned(),
            }),
            work: role.map(|r| crate::badge::WorkFacts {
                provider: Some("github".to_owned()),
                kind: Some("pr".to_owned()),
                id: None,
                role: Some(r.to_owned()),
            }),
            attach_refusal: None,
            can_stop: false,
            can_remove: false,
            runtime_detail: String::new(),
            open: crate::open::OpenTargets::default(),
            host_label: None,
            host_unreachable: false,
            details: Default::default(),
        }
    }

    fn placed(layout: &mut Layout, rows: &[SessionRow], rules: &[PlacementRule]) {
        let keys: Vec<SessionKey> = rows.iter().map(|r| r.key.clone()).collect();
        layout.reconcile("local", &keys);
        layout.apply_rules(rows, rules);
    }

    fn group_of(layout: &Layout, id: &str) -> String {
        let item = layout
            .items()
            .iter()
            .find(|i| i.session_id == id)
            .expect("item");
        layout
            .groups()
            .iter()
            .find(|g| g.id == item.group)
            .expect("group")
            .name
            .clone()
    }

    #[test]
    fn returning_to_rule_clears_manual_and_places_at_the_end_or_in_inbox() {
        let rules = [rule("Pohunek", |r| r.project = Some("pohunek".into()))];
        let ruled = session("a", Some("pohunek"), None);
        let other = session("b", Some("pohunek"), None);
        let plain = session("c", None, None);
        let rows = [ruled.clone(), other.clone(), plain.clone()];
        let mut layout = Layout::default();
        placed(&mut layout, &rows, &rules);
        layout.create_group("Mine").expect("group");
        let mine = layout
            .groups()
            .iter()
            .find(|g| g.name == "Mine")
            .expect("mine")
            .id
            .clone();
        for row in [&ruled, &plain] {
            layout
                .move_session(&row.key, &mine, Placement::End)
                .expect("move");
        }

        assert_eq!(layout.return_to_rule(&ruled, &rules), Ok(true));
        assert_eq!(group_of(&layout, "a"), "Pohunek");
        let section = layout
            .arrange(&rows)
            .into_iter()
            .find(|s| s.name == "Pohunek")
            .expect("section");
        let ids: Vec<&str> = section
            .rows
            .iter()
            .map(|r| r.key.session_id.as_str())
            .collect();
        assert_eq!(ids, ["b", "a"], "appended at the end");

        assert_eq!(layout.return_to_rule(&plain, &rules), Ok(true));
        assert_eq!(group_of(&layout, "c"), "Inbox");
        assert!(layout.items().iter().all(|i| !i.manual));
        assert_eq!(layout.return_to_rule(&plain, &rules), Ok(false));
        assert!(matches!(
            layout.return_to_rule(&session("zz", None, None), &rules),
            Err(MoveError::UnknownSession(_))
        ));
    }

    #[test]
    fn rules_need_all_conditions_and_the_first_match_wins() {
        let by_fields = [rule("Gh", |r| {
            r.work_provider = Some("github".into());
            r.work_kind = Some("pr".into());
        })];
        let mut probe = Layout::default();
        placed(&mut probe, &[session("a", None, Some("x"))], &by_fields);
        assert_eq!(group_of(&probe, "a"), "Gh");
        let rules = [
            rule("Reviews", |r| {
                r.project = Some("pohunek".into());
                r.work_role = Some("reviewer".into());
            }),
            rule("Pohunek", |r| r.project = Some("id-pohunek".into())),
            rule("Other", |r| r.agent = Some("claude".into())),
        ];
        let rows = [
            session("a", Some("pohunek"), Some("reviewer")),
            session("b", Some("pohunek"), Some("implementer")),
            session("c", Some("else"), None),
            session("d", None, Some("reviewer")),
        ];
        let mut layout = Layout::default();
        placed(&mut layout, &rows, &rules);
        assert_eq!(group_of(&layout, "a"), "Reviews");
        assert_eq!(group_of(&layout, "b"), "Pohunek");
        assert_eq!(group_of(&layout, "c"), "Other");
        let ids: HashSet<&str> = layout.groups().iter().map(|g| g.id.as_str()).collect();
        assert_eq!(ids.len(), 4, "each missing group is created once");
        let inbox = [rule("X", |r| r.work_kind = Some("issue".into()))];
        let mut layout = Layout::default();
        placed(&mut layout, &rows, &inbox);
        assert!(
            rows.iter()
                .all(|r| group_of(&layout, &r.key.session_id) == "Inbox")
        );
    }

    #[test]
    fn metadata_changes_move_unplaced_sessions_and_append_to_the_group_end() {
        let rules = [rule("Reviews", |r| r.work_role = Some("reviewer".into()))];
        let mut layout = Layout::default();
        placed(
            &mut layout,
            &[
                session("a", None, None),
                session("b", None, Some("reviewer")),
            ],
            &rules,
        );
        assert_eq!(group_of(&layout, "a"), "Inbox");
        let reviews = layout
            .groups()
            .iter()
            .find(|g| g.name == "Reviews")
            .unwrap()
            .id
            .clone();
        assert_eq!(order_of(&layout, &reviews), ["b"]);

        let rows = [
            session("a", None, Some("reviewer")),
            session("b", None, Some("reviewer")),
        ];
        placed(&mut layout, &rows, &rules);
        assert_eq!(order_of(&layout, &reviews), ["b", "a"]);
        assert!(
            layout.apply_rules(&rows, &rules).is_empty(),
            "positions are kept"
        );

        let rows = [
            session("a", None, None),
            session("b", None, Some("reviewer")),
        ];
        placed(&mut layout, &rows, &rules);
        assert_eq!(group_of(&layout, "a"), "Inbox");
    }

    #[test]
    fn a_dragged_session_is_not_moved_by_rules_and_group_names_ignore_case() {
        let rules = [rule("work", |r| r.agent = Some("claude".into()))];
        let rows = [session("a", None, None), session("b", None, None)];
        let mut layout: Layout = toml::from_str(THREE_GROUPS).expect("parses");
        placed(&mut layout, &rows, &rules);
        assert_eq!(order_of(&layout, "work"), ["a", "b"]);

        layout
            .move_session(&key("a"), "empty", Placement::End)
            .expect("move");
        layout.apply_rules(&rows, &rules);
        assert_eq!(group_of(&layout, "a"), "Empty");
        assert_eq!(layout.groups().len(), 3);
    }

    #[test]
    fn dropping_a_rule_placed_session_on_its_own_slot_pins_it() {
        let rules = [rule("work", |r| r.agent = Some("claude".into()))];
        let rows = [session("a", None, None), session("b", None, None)];
        let mut layout: Layout = toml::from_str(THREE_GROUPS).expect("parses");
        placed(&mut layout, &rows, &rules);
        assert_eq!(order_of(&layout, "work"), ["a", "b"]);

        assert_eq!(
            layout.move_session(&key("a"), "work", Placement::Before(&key("b"))),
            Ok(true)
        );
        assert_eq!(
            layout.move_session(&key("a"), "work", Placement::Before(&key("b"))),
            Ok(false)
        );
        assert_eq!(order_of(&layout, "work"), ["a", "b"]);

        let other = [rule("empty", |r| r.agent = Some("claude".into()))];
        layout.apply_rules(&rows, &other);
        assert_eq!(group_of(&layout, "a"), "Work");
    }
}
