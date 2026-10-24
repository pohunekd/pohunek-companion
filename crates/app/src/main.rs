use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, mpsc};
use std::time::{Duration, Instant};

mod assistant;
mod cli;
mod completion;
mod confirm;
#[cfg(target_os = "linux")]
mod desktop;
mod detail;
mod group_edit;
mod launch;
mod logging;
mod new_session;
mod resume;
mod setup;
mod tray;

use clap::Parser;
use cli::{Cli, Command, DesktopAction};

use pohunek_companion_core::{
    AttachConfig, AttachError, Config, DaemonConfig, DialogHost, GroupError, GroupPlacement,
    GroupSection, HostConfig, HostConnection, HostEntry, HostOption, INBOX_ID, LOCAL_HOST,
    LayoutConfig, LayoutStore, LiveHost, LiveUpdate, NEEDS_YOU_ID, OpenConfig, OpenKind,
    OwnerState, PendingStore, Placement, PlacementRule, ProjectChip, ProjectSelection,
    SessionFilter, SessionKey, SessionRow, StateChip, TraySummary, View, ViewConfig, WriteOutcome,
    attach, clip_display, dialog_hosts, discover_records, filter_sections,
    host_naming_this_machine, host_options, layout_path, name_from_input, open, plan_discovery,
    project_chips, rename_session, run_live, set_route, state_chips, state_from_key, write_group,
};
use slint::{ComponentHandle, ModelRc, VecModel};
use tracing::{debug, error, info, warn};

slint::include_modules!();

const APP_ID: &str = "pohunek-companion";
/// Longest badge link or role text shown in a row.
const BADGE_MAX_CHARS: usize = 32;

fn owner_state_kind(state: OwnerState) -> OwnerStateKind {
    match state {
        OwnerState::NeedsOwner => OwnerStateKind::NeedsOwner,
        OwnerState::Working => OwnerStateKind::Working,
        OwnerState::Idle => OwnerStateKind::Idle,
        OwnerState::RuntimeProblem => OwnerStateKind::RuntimeProblem,
        OwnerState::Ended => OwnerStateKind::Ended,
        OwnerState::NoAgentState => OwnerStateKind::NoAgentState,
    }
}

/// Detail text and attach refusal of a row whose host is not connected.
const HOST_UNREACHABLE: &str = "host unreachable";

fn session_rows(sessions: Vec<SessionRow>, placed: &Placements) -> Vec<SessionRowData> {
    sessions
        .into_iter()
        .map(|s| {
            let (badge, badge_role) = match s.badge {
                Some(b) => (
                    clip_display(&b.link, BADGE_MAX_CHARS),
                    clip_display(&b.role.unwrap_or_default(), BADGE_MAX_CHARS),
                ),
                None => Default::default(),
            };
            let (owner_group, manual) = placed.get(&s.key).cloned().unwrap_or_default();
            // A group key is the record of the placement like a manual one.
            let manual = manual || s.group.is_some() || s.group_problem.is_some();
            let (project_id, project_label) = s
                .project
                .map(|project| (project.id, project.label))
                .unwrap_or_default();
            SessionRowData {
                project_id: project_id.into(),
                project_label: project_label.into(),
                owner_group: owner_group.into(),
                manual,
                name: s.display_name.into(),
                agent: s.agent.into(),
                state: s.state.into(),
                runtime_detail: s.runtime_detail.into(),
                owner_state: owner_state_kind(s.owner_state),
                owner_label: s.owner_state.label().into(),
                badge: badge.into(),
                badge_role: badge_role.into(),
                host: s.key.host.into(),
                host_label: s.host_label.unwrap_or_default().into(),
                id: s.key.session_id.into(),
                host_unreachable: s.host_unreachable,
                // Actions need a connected host.
                can_stop: s.can_stop && !s.host_unreachable,
                can_remove: s.can_remove && !s.host_unreachable,
                can_resume: s.resume.is_available() && !s.host_unreachable,
                open_folder: s.open.existing_folder().is_some(),
                open_link: s.open.link.is_some() && !s.host_unreachable,
                attach_refusal: if s.host_unreachable {
                    HOST_UNREACHABLE.to_owned()
                } else {
                    s.attach_refusal
                        .map(|reason| reason.to_string())
                        .unwrap_or_default()
                }
                .into(),
            }
        })
        .collect()
}

/// Each session's owner group id and whether the owner placed it by hand.
type Placements = HashMap<SessionKey, (String, bool)>;

/// What the window shows of the layout: the sections of the selected view,
/// each session's placement, and the owner groups in display order as
/// (id, name).
struct Arranged {
    /// A filter is active and no session matches it (empty sections may remain).
    filter_empty: bool,
    sections: Vec<GroupSection>,
    placed: Placements,
    owner_groups: Vec<(String, String)>,
    project_chips: Vec<ProjectChip>,
    project_index: usize,
    /// Id of a project with the selected label, for the New session button;
    /// the selected host's project wins, else the first seen.
    filter_project_id: String,
    state_chips: Vec<StateChip>,
    host_options: Vec<HostOption>,
}

/// The identifier the window sends back for a Hosts dropdown entry; empty for
/// "All hosts".
fn host_key(option: &HostOption) -> String {
    option.target.clone().unwrap_or_default()
}

/// The identifier the window sends back for a project chip; stable across
/// session list changes, unlike the chip's position.
fn chip_key(selection: &ProjectSelection) -> String {
    match selection {
        ProjectSelection::All => "all".to_owned(),
        ProjectSelection::NoProject => "none".to_owned(),
        ProjectSelection::Project(project) => format!("project:{project}"),
    }
}

/// The sessions of the selected host, or all of them; the project and state
/// chips offer only what exists there.
fn host_rows(state: &LayoutState) -> Vec<SessionRow> {
    match state.filter.host() {
        Some(host) => state
            .rows
            .iter()
            .filter(|row| row.key.host == host)
            .cloned()
            .collect(),
        None => state.rows.clone(),
    }
}

fn selected_project_id(state: &LayoutState) -> String {
    let ProjectSelection::Project(label) = state.filter.project() else {
        return String::new();
    };
    host_rows(state)
        .iter()
        .filter_map(|row| row.project.as_ref())
        .find(|project| project.label == *label)
        .map(|project| project.id.clone())
        .unwrap_or_default()
}

fn arrange(state: &LayoutState) -> Arranged {
    let started = Instant::now();
    let layout = state.store.layout();
    let sections = filter_sections(
        layout.present(&state.rows, state.title_max_chars),
        &state.filter,
    );
    debug!(
        elapsed_us = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX),
        filtered = state.filter.is_active(),
        sessions = state.rows.len(),
        "filter and present"
    );
    let (project_chips, project_index) = project_chips(&host_rows(state), state.filter.project());
    Arranged {
        filter_empty: state.filter.is_active()
            && !state.rows.is_empty()
            && sections.iter().all(|section| section.rows.is_empty()),
        state_chips: state_chips(&host_rows(state), state.filter.states()),
        host_options: host_options(&state.hosts, state.filter.host()),
        sections,
        project_chips,
        project_index,
        filter_project_id: selected_project_id(state),
        placed: layout
            .items()
            .iter()
            .map(|item| {
                let key = SessionKey {
                    host: item.host.clone(),
                    session_id: item.session_id.clone(),
                };
                (key, (item.group.clone(), item.manual))
            })
            .collect(),
        owner_groups: layout
            .ordered_groups()
            .into_iter()
            .map(|group| (group.id.clone(), group.name.clone()))
            .collect(),
    }
}

fn show_arranged(window: &AppWindow, arranged: Arranged) {
    let Arranged {
        filter_empty,
        sections,
        placed,
        owner_groups,
        project_chips,
        project_index,
        filter_project_id,
        state_chips,
        host_options,
    } = arranged;
    window.set_host_keys(ModelRc::new(VecModel::from(
        host_options
            .iter()
            .map(|option| slint::SharedString::from(host_key(option)))
            .collect::<Vec<_>>(),
    )));
    window.set_host_checked(ModelRc::new(VecModel::from(
        host_options
            .iter()
            .map(|option| option.selected)
            .collect::<Vec<_>>(),
    )));
    window.set_host_active_name(
        host_options
            .iter()
            .find(|option| option.selected)
            .and_then(|option| option.name.clone())
            .unwrap_or_default()
            .into(),
    );
    window.set_host_names(ModelRc::new(VecModel::from(
        host_options
            .into_iter()
            .map(|option| slint::SharedString::from(option.label))
            .collect::<Vec<_>>(),
    )));
    window.set_state_keys(ModelRc::new(VecModel::from(
        state_chips
            .iter()
            .map(|chip| slint::SharedString::from(chip.key))
            .collect::<Vec<_>>(),
    )));
    window.set_state_checked(ModelRc::new(VecModel::from(
        state_chips
            .iter()
            .map(|chip| chip.selected)
            .collect::<Vec<_>>(),
    )));
    window.set_state_names(ModelRc::new(VecModel::from(
        state_chips
            .iter()
            .map(|chip| slint::SharedString::from(chip.label))
            .collect::<Vec<_>>(),
    )));
    window.set_project_index(i32::try_from(project_index).unwrap_or_default());
    window.set_filter_project_id(filter_project_id.into());
    window.set_project_keys(ModelRc::new(VecModel::from(
        project_chips
            .iter()
            .map(|chip| slint::SharedString::from(chip_key(&chip.selection)))
            .collect::<Vec<_>>(),
    )));
    window.set_project_names(ModelRc::new(VecModel::from(
        project_chips
            .into_iter()
            .map(|chip| slint::SharedString::from(chip.label))
            .collect::<Vec<_>>(),
    )));
    window.set_groups(ModelRc::new(VecModel::from(group_data(sections, &placed))));
    window.set_filter_empty(filter_empty);
    window.set_owner_groups(ModelRc::new(VecModel::from(
        owner_groups
            .into_iter()
            .map(|(id, name)| OwnerGroupData {
                id: id.into(),
                name: name.into(),
            })
            .collect::<Vec<_>>(),
    )));
}

fn group_data(sections: Vec<GroupSection>, placed: &Placements) -> Vec<GroupData> {
    sections
        .into_iter()
        .map(|section| GroupData {
            needs_you: section.id == NEEDS_YOU_ID,
            fixed: section.id == INBOX_ID,
            collapsed: section.collapsed,
            id: section.id.into(),
            name: section.name.into(),
            count: i32::try_from(section.rows.len()).unwrap_or(i32::MAX),
            rows: ModelRc::new(VecModel::from(session_rows(section.rows, placed))),
        })
        .collect()
}

/// The layout store with the session list it arranges. The daemon thread and
/// the drop handler both change it, always under the lock and always queueing
/// the resulting window update before releasing it, so updates reach the UI in
/// the order the layout changed.
struct LayoutState {
    store: LayoutStore,
    rows: Vec<SessionRow>,
    /// Every followed host with its connection state, local first.
    hosts: Vec<HostEntry>,
    load_error: Option<String>,
    /// Placement rules from `companion.toml`, applied to sessions the owner
    /// hands back with "Return to rule placement".
    rules: Vec<PlacementRule>,
    /// Longest title of a derived section, from `view.section_title_max_chars`.
    title_max_chars: usize,
    /// `layout.group_name_max_bytes`: bounds a group name written as a
    /// `companion.group` value.
    group_name_max_bytes: usize,
    /// The daemon settings group writes use; an error when `companion.toml`
    /// is unusable.
    daemon: Result<DaemonConfig, String>,
    /// The filter of the selected view: its stored states and project, and the
    /// query typed into the filter field.
    filter: SessionFilter,
    /// The query text of each view by view id; kept while the app runs, never
    /// persisted.
    queries: HashMap<&'static str, String>,
}

type SharedLayout = Arc<Mutex<LayoutState>>;

fn lock(layout: &SharedLayout) -> MutexGuard<'_, LayoutState> {
    // The state stays consistent across a panic elsewhere: every change is a
    // single store call.
    layout.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The name a remote session's host is shown under, or `None` for a local
/// session. Falls back to the host route when the row is no longer listed.
pub(crate) fn remote_host_name(layout: &SharedLayout, key: &SessionKey) -> Option<String> {
    if key.host == LOCAL_HOST {
        return None;
    }
    let label = lock(layout)
        .rows
        .iter()
        .find(|row| row.key == *key)
        .and_then(|row| row.host_label.clone());
    Some(label.unwrap_or_else(|| key.host.clone()))
}

/// Saves the layout and returns the text for the window's layout error line.
fn save_layout(state: &mut LayoutState) -> Option<String> {
    state
        .store
        .save_if_changed()
        .err()
        .map(|err| {
            error!(error = %err, "save the layout");
            err.to_string()
        })
        .or_else(|| state.load_error.clone())
}

/// A layout change the owner made in the window. Sessions and groups are
/// addressed by key and id, so daemon updates that arrive in the meantime do
/// not matter.
enum LayoutRequest {
    /// The dragged session, the target group, and the anchor session with
    /// whether the drop lands after it (none: group end).
    Drop {
        key: SessionKey,
        group: String,
        anchor: Option<(SessionKey, bool)>,
    },
    CreateGroup(String),
    /// Creates a group and moves the session to its end in one change.
    CreateGroupAndMove {
        name: String,
        key: SessionKey,
    },
    RenameGroup {
        id: String,
        name: String,
    },
    SetCollapsed {
        id: String,
        collapsed: bool,
    },
    MoveGroup {
        id: String,
        anchor: String,
        after: bool,
    },
    DeleteGroup(String),
    ReturnToRule(SessionKey),
    /// The text of the filter field.
    SetFilter(String),
    /// The key (`chip_key`) of the project chip the owner picked.
    SetProject(String),
    /// The key (`state_key`) of the state chip the owner toggled.
    ToggleState(String),
    /// The target of the host the owner picked; empty for all hosts.
    SetHost(String),
}

/// Selects the view on a background thread (the save waits on disk) and shows
/// it with the latest session list.
fn spawn_view_change(window: slint::Weak<AppWindow>, layout: SharedLayout, view: View) {
    std::thread::spawn(move || {
        let mut state = lock(&layout);
        state.store.set_view(view);
        state.filter = SessionFilter::from_stored(&state.store.layout().filter(view));
        let query = state.queries.get(view.id()).cloned().unwrap_or_default();
        state.filter.set_query(&query);
        let layout_error = save_layout(&mut state);
        let arranged = arrange(&state);
        let applied = window.upgrade_in_event_loop(move |window| {
            window.set_filter_text(query.into());
            window.set_view_index(i32::try_from(view.index()).unwrap_or_default());
            window.set_editable(view.is_editable());
            show_arranged(&window, arranged);
            window.set_layout_error(layout_error.unwrap_or_default().into());
        });
        drop(state);
        if let Err(err) = applied {
            error!(error = %err, "deliver the view change to the UI");
        }
    });
}

/// Why a request was refused, for the owner; a drop and a group operation
/// each have their own message line.
#[derive(Default)]
struct Refusal {
    drop: Option<String>,
    group: Option<String>,
}

fn group_refusal(operation: &str, id: &str, err: &GroupError) -> Option<String> {
    warn!(group = %id, operation, reason = err.kind(), "group operation refused");
    Some(format!("Cannot {operation}: {err}"))
}

/// Stores the project and states of the current filter for the selected view.
fn store_filter(state: &mut LayoutState) {
    let view = state.store.layout().view();
    state.store.set_filter(view, state.filter.to_stored());
}

fn apply_request(state: &mut LayoutState, request: LayoutRequest) -> Refusal {
    let group = |refusal| Refusal {
        group: refusal,
        ..Refusal::default()
    };
    match request {
        LayoutRequest::CreateGroup(name) => group(
            group_edit::checked_name(state, &name)
                .and_then(|name| state.store.create_group(&name))
                .err()
                .and_then(|err| group_refusal("create the group", "new", &err)),
        ),
        LayoutRequest::SetCollapsed { id, collapsed } => group(
            state
                .store
                .set_collapsed(&id, collapsed)
                .err()
                .and_then(|err| group_refusal("collapse the group", &id, &err)),
        ),
        LayoutRequest::MoveGroup { id, anchor, after } => {
            let placement = if after {
                GroupPlacement::After(&anchor)
            } else {
                GroupPlacement::Before(&anchor)
            };
            group(
                state
                    .store
                    .move_group(&id, placement)
                    .err()
                    .and_then(|err| group_refusal("move the group", &id, &err)),
            )
        }
        LayoutRequest::SetFilter(query) => {
            state.filter.set_query(&query);
            let view = state.store.layout().view();
            state.queries.insert(view.id(), query);
            Refusal::default()
        }
        LayoutRequest::ToggleState(key) => {
            match state_from_key(&key) {
                Some(owner_state) => {
                    state.filter.toggle_state(owner_state);
                    store_filter(state);
                }
                None => warn!(chip = %key, "unknown state chip"),
            }
            Refusal::default()
        }
        LayoutRequest::SetHost(target) => {
            let options = host_options(&state.hosts, state.filter.host());
            match options.into_iter().find(|o| host_key(o) == target) {
                Some(option) => {
                    state.filter.toggle_host(option.target);
                    store_filter(state);
                }
                None => warn!(host = %target, "host entry is no longer offered"),
            }
            Refusal::default()
        }
        LayoutRequest::SetProject(key) => {
            let (chips, _) = project_chips(&host_rows(state), state.filter.project());
            match chips
                .into_iter()
                .find(|chip| chip_key(&chip.selection) == key)
            {
                Some(chip) => {
                    state.filter.toggle_project(chip.selection);
                    store_filter(state);
                }
                None => warn!(chip = %key, "project chip is no longer offered"),
            }
            Refusal::default()
        }
        LayoutRequest::Drop { .. }
        | LayoutRequest::CreateGroupAndMove { .. }
        | LayoutRequest::RenameGroup { .. }
        | LayoutRequest::ReturnToRule(_)
        | LayoutRequest::DeleteGroup(_) => {
            error!("a request that writes metadata reached the local layout handler");
            Refusal::default()
        }
    }
}

/// Payload of a dragged group header: the group's id.
#[derive(Clone)]
struct GroupDrag(String);

/// Applies layout changes one at a time, in the order they were made, off the
/// UI thread because the save waits on disk.
fn spawn_layout_worker(
    window: slint::Weak<AppWindow>,
    layout: SharedLayout,
) -> mpsc::Sender<LayoutRequest> {
    let (sender, receiver) = mpsc::channel::<LayoutRequest>();
    std::thread::spawn(move || {
        let writer = group_edit::Writer::new();
        for request in receiver {
            // A request that writes metadata locks the layout only around its
            // local changes, never across the daemon calls.
            let Refusal {
                drop,
                group: group_error,
            } = if group_edit::writes_metadata(&request) {
                group_edit::apply(&layout, &writer, request)
            } else {
                apply_request(&mut lock(&layout), request)
            };
            let mut state = lock(&layout);
            let layout_error = match (drop, save_layout(&mut state)) {
                (Some(moved), Some(saved)) => Some(format!("{moved}\n{saved}")),
                (moved, saved) => moved.or(saved),
            };
            let arranged = arrange(&state);
            if let Err(err) = window.upgrade_in_event_loop(move |window| {
                show_arranged(&window, arranged);
                window.set_layout_error(layout_error.unwrap_or_default().into());
                window.set_group_error(group_error.unwrap_or_default().into());
            }) {
                error!(error = %err, "deliver the changed layout to the UI");
            }
        }
    });
    sender
}

/// Moves sessions the new-session dialog recorded a group for into that group
/// (as a manual placement, so rules leave them alone) once they are in the
/// session list, and drops entries that expired.
fn apply_pending(
    store: &mut LayoutStore,
    pending: &PendingStore,
    present: &[SessionKey],
    max_age: Duration,
) {
    let present: HashSet<SessionKey> = present.iter().cloned().collect();
    let settled = match pending.take(&present, max_age) {
        Ok(settled) => settled,
        Err(err) => {
            warn!(error = %err, "read the pending placements");
            return;
        }
    };
    for key in &settled.expired {
        warn!(session = %key.session_id, "pending placement expired before the session appeared");
    }
    for (key, group) in settled.apply {
        if let Err(err) = store.move_session(&key, &group, Placement::End) {
            warn!(session = %key.session_id, group = %group, error = %err, "pending placement refused");
        }
    }
}

/// Everything a host's loop needs to publish an update to the window.
struct Publisher {
    window: slint::Weak<AppWindow>,
    tray: slint::Weak<Tray>,
    rules: Vec<PlacementRule>,
    /// `layout.group_name_max_bytes`: bounds a usable `companion.group` value.
    group_name_max_bytes: usize,
    /// The daemon settings the migration writes use.
    daemon: DaemonConfig,
    migrations: Rc<RefCell<Migrations>>,
    pending: Option<(PendingStore, Duration)>,
    layout: SharedLayout,
    hosts: Rc<RefCell<HostsView>>,
    /// The setup screen; absent when `companion.toml` is unusable.
    setup: Option<Arc<setup::Setup>>,
}

/// The writes of one migration and the host's rows they address.
type MigrationRun = (Vec<(SessionKey, String)>, Vec<SessionRow>);

/// Per host state of the migration of manual placements into session
/// metadata (`LayoutStore::migration_plan`).
#[derive(Default)]
struct Migrations {
    /// Hosts that connected and have not run the migration since.
    armed: HashSet<String>,
    /// Hosts whose migration writes are in flight.
    running: HashSet<String>,
}

/// The latest session list of every followed host and the hosts that are
/// currently not connected; shared by the loops of all hosts, which run on one
/// thread.
struct HostsView {
    /// Hosts in display order (local first); `None` rows until first seeded.
    rows: Vec<(String, Option<Vec<SessionRow>>)>,
    /// Status line per host that is not connected, by target.
    down: Vec<(String, String)>,
    /// Display name per host target.
    labels: Vec<(String, String)>,
    /// Status line per discovered peer whose daemon is incompatible; these
    /// peers have no rows and are not connected.
    incompatible: Vec<String>,
}

impl HostsView {
    fn new(hosts: &[LiveHost]) -> Self {
        Self {
            rows: hosts.iter().map(|h| (h.target.clone(), None)).collect(),
            down: Vec::new(),
            labels: hosts
                .iter()
                .map(|h| {
                    let label = h.label.clone().unwrap_or_else(|| "Local".to_owned());
                    (h.target.clone(), label)
                })
                .collect(),
            incompatible: Vec::new(),
        }
    }

    /// Starts tracking a host that was not known at startup.
    fn add_host(&mut self, host: &LiveHost) {
        if self.rows.iter().any(|(target, _)| *target == host.target) {
            return;
        }
        self.rows.push((host.target.clone(), None));
        let label = host.label.clone().unwrap_or_else(|| host.target.clone());
        self.labels.push((host.target.clone(), label));
    }

    /// Stores `sessions` as `host`'s list.
    fn set_rows(&mut self, host: &str, sessions: Vec<SessionRow>) {
        if let Some((_, slot)) = self.rows.iter_mut().find(|(target, _)| target == host) {
            *slot = Some(sessions);
        }
    }

    /// The union of all hosts' lists; rows of a remote host that is not
    /// connected keep their last-known facts and are marked unreachable.
    fn union(&self) -> Vec<SessionRow> {
        self.rows
            .iter()
            .filter_map(|(target, rows)| Some((self.is_unreachable(target), rows.as_deref()?)))
            .flat_map(|(unreachable, rows)| {
                rows.iter().cloned().map(move |mut row| {
                    row.host_unreachable = unreachable;
                    row
                })
            })
            .collect()
    }

    /// A remote host that is not connected. The local daemon's outage keeps
    /// its own handling (offline tray, unmarked rows).
    fn is_unreachable(&self, host: &str) -> bool {
        host != LOCAL_HOST && self.is_down(host)
    }

    /// How many remote hosts are not connected.
    fn unreachable_count(&self) -> usize {
        self.down
            .iter()
            .filter(|(target, _)| target != LOCAL_HOST)
            .count()
    }

    /// Every host has delivered a list at least once; derived collapsed
    /// sections are pruned only then, because an unseeded host's sections are
    /// not yet known.
    fn all_seeded(&self) -> bool {
        self.rows.iter().all(|(_, rows)| rows.is_some())
    }

    fn is_down(&self, host: &str) -> bool {
        self.down.iter().any(|(target, _)| target == host)
    }

    fn set_down(&mut self, host: &str, line: Option<String>) {
        self.down.retain(|(target, _)| target != host);
        if let Some(line) = line {
            self.down.push((host.to_owned(), line));
        }
    }

    /// Every followed host with its connection state, for the Hosts dropdown.
    /// A remote host that is not connected is unreachable; the local daemon's
    /// outage is shown by the tray and status line, not by this entry.
    fn entries(&self) -> Vec<HostEntry> {
        self.rows
            .iter()
            .map(|(target, _)| HostEntry {
                target: target.clone(),
                label: self
                    .labels
                    .iter()
                    .find(|(t, _)| t == target)
                    .map_or_else(|| target.clone(), |(_, label)| label.clone()),
                connection: if self.is_unreachable(target) {
                    HostConnection::Unreachable
                } else {
                    HostConnection::Connected
                },
            })
            .collect()
    }

    fn status(&self) -> String {
        let order = |target: &str| self.rows.iter().position(|(t, _)| t == target);
        let mut down: Vec<_> = self.down.iter().collect();
        down.sort_by_key(|(target, _)| order(target));
        down.into_iter()
            .map(|(_, line)| line.as_str())
            .chain(self.incompatible.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join("; ")
    }
}

impl Publisher {
    /// Shows why `host` is not followed; its rows stay out of the list.
    fn refuse(&self, host: &LiveHost, message: &str) {
        let label = host.label.as_deref().unwrap_or(&host.target);
        let status = {
            let mut hosts = self.hosts.borrow_mut();
            hosts.set_down(&host.target, Some(format!("{label}: {message}")));
            hosts.status()
        };
        if let Err(err) = self.window.upgrade_in_event_loop(move |window| {
            window.set_status(status.into());
        }) {
            error!(error = %err, "deliver the hosts error to the UI");
        }
    }

    /// Starts tracking `host` before its loop runs.
    fn add_host(&self, host: &LiveHost) {
        self.hosts.borrow_mut().add_host(host);
    }

    /// Replaces the incompatible-peer status lines.
    fn set_incompatible(&self, lines: Vec<String>) {
        let status = {
            let mut hosts = self.hosts.borrow_mut();
            if hosts.incompatible == lines {
                return;
            }
            hosts.incompatible = lines;
            hosts.status()
        };
        if let Err(err) = self.window.upgrade_in_event_loop(move |window| {
            window.set_status(status.into());
        }) {
            error!(error = %err, "deliver the incompatible hosts to the UI");
        }
    }

    /// Applies one update of `host` to the layout and queues the window update.
    fn publish(&self, host: &LiveHost, update: LiveUpdate) {
        let local = host.target == LOCAL_HOST;
        if matches!(update, LiveUpdate::Connected) {
            self.migrations
                .borrow_mut()
                .armed
                .insert(host.target.clone());
        }
        if let LiveUpdate::Sessions(sessions) = &update {
            self.hosts
                .borrow_mut()
                .set_rows(&host.target, sessions.clone());
        }
        if let Some(setup) = &self.setup {
            match &update {
                LiveUpdate::Connected if local => setup.connected(),
                LiveUpdate::Reconnecting { reason, code, .. } if local => {
                    setup.down(reason.clone(), code.clone());
                }
                LiveUpdate::Sessions(_) => {
                    setup.rows_elsewhere(!self.hosts.borrow().union().is_empty());
                }
                _ => {}
            }
        }
        let status = match &update {
            LiveUpdate::Connected => {
                let mut hosts = self.hosts.borrow_mut();
                hosts.set_down(&host.target, None);
                Some(hosts.status())
            }
            LiveUpdate::Reconnecting {
                attempt,
                reason,
                code,
            } => {
                let line = match code {
                    Some(code) => format!("Connection refused ({code}): {reason}"),
                    None => format!("Reconnecting (attempt {attempt}): {reason}"),
                };
                let line = match &host.label {
                    Some(label) => format!("{label}: {line}"),
                    None => line,
                };
                let mut hosts = self.hosts.borrow_mut();
                hosts.set_down(&host.target, Some(line));
                Some(hosts.status())
            }
            LiveUpdate::Lost { reason } => {
                let line = match &host.label {
                    Some(label) => format!("{label}: {reason}"),
                    None => reason.clone(),
                };
                let mut hosts = self.hosts.borrow_mut();
                hosts.set_down(&host.target, Some(line));
                Some(hosts.status())
            }
            LiveUpdate::Sessions(_) => None,
        };
        // The list changes with a host's sessions and when a remote host
        // becomes unreachable; a reconnect is followed by its seed.
        let union = (matches!(update, LiveUpdate::Sessions(_))
            || (matches!(
                update,
                LiveUpdate::Reconnecting { .. } | LiveUpdate::Lost { .. }
            ) && !local))
            .then(|| self.hosts.borrow().union());
        // The tray reflects the local daemon: while it is down the tray stays
        // offline whatever a remote host reports.
        let summary = union
            .as_deref()
            .filter(|_| !self.hosts.borrow().is_down(LOCAL_HOST))
            .map(|rows| TraySummary::from_rows(rows, self.hosts.borrow().unreachable_count()));
        let all_seeded = self.hosts.borrow().all_seeded();
        // Reconciled and saved here so the UI thread never waits on disk;
        // the lock is held until the update is queued.
        let mut guard = None;
        let mut migration = None;
        let arranged = match &union {
            Some(all) => {
                let mut state = lock(&self.layout);
                // A host that only became unreachable keeps its placements.
                if let LiveUpdate::Sessions(sessions) = &update {
                    let keys: Vec<SessionKey> = sessions.iter().map(|s| s.key.clone()).collect();
                    state.store.reconcile(&host.target, &keys);
                    if let Some((pending, max_age)) = &self.pending {
                        let all_keys: Vec<SessionKey> = all.iter().map(|s| s.key.clone()).collect();
                        apply_pending(&mut state.store, pending, &all_keys, *max_age);
                    }
                    state.store.apply_group_values(all);
                    state.store.apply_rules(all, &self.rules);
                    let title_max_chars = state.title_max_chars;
                    if all_seeded {
                        // After the rules: a session whose key went away is
                        // placed by them first, so its learned group is empty.
                        state.store.drop_empty_learned_groups();
                        state.store.prune_collapsed_sections(all, title_max_chars);
                    }
                    migration = self.plan_migration(&mut state, &host.target, sessions);
                }
                let layout_error = save_layout(&mut state);
                state.rows = all.clone();
                state.hosts = self.hosts.borrow().entries();
                let arranged = arrange(&state);
                guard = Some(state);
                Some((arranged, layout_error))
            }
            _ => None,
        };
        let tray = self.tray.clone();
        let applied = self
            .window
            .upgrade_in_event_loop(move |window| match update {
                LiveUpdate::Connected => {
                    window.set_error("".into());
                    window.set_status(status.unwrap_or_default().into());
                }
                LiveUpdate::Sessions(_) => {
                    if let (Some(tray), Some(summary)) = (tray.upgrade(), summary) {
                        tray::apply(&tray, &summary);
                    }
                    if let Some((arranged, layout_error)) = arranged {
                        show_arranged(&window, arranged);
                        window.set_layout_error(layout_error.unwrap_or_default().into());
                    }
                }
                LiveUpdate::Reconnecting { .. } | LiveUpdate::Lost { .. } => {
                    // The tray goes offline with the local daemon only; a lost
                    // remote host's rows stay, marked unreachable, and leave
                    // the tray counts.
                    if local {
                        if let Some(tray) = tray.upgrade() {
                            tray::apply_offline(&tray, "Not connected to the daemon");
                        }
                    } else {
                        if let (Some(tray), Some(summary)) = (tray.upgrade(), summary) {
                            tray::apply(&tray, &summary);
                        }
                        if let Some((arranged, layout_error)) = arranged {
                            show_arranged(&window, arranged);
                            window.set_layout_error(layout_error.unwrap_or_default().into());
                        }
                    }
                    window.set_status(status.unwrap_or_default().into());
                }
            });
        drop(guard);
        if let Err(err) = applied {
            error!(error = %err, "deliver a daemon update to the UI");
        }
        if let Some((writes, rows)) = migration {
            self.spawn_migration(&host.target, writes, rows);
        }
    }

    /// Plans the migration of `host`'s manual placements after its first
    /// seed of a connection. Placements the daemon cannot hold are marked not
    /// shared; a host with nothing to write is recorded as migrated at once.
    /// Returns the writes to run, with the host's rows for their targets.
    fn plan_migration(
        &self,
        state: &mut LayoutState,
        host: &str,
        rows: &[SessionRow],
    ) -> Option<MigrationRun> {
        let mut migrations = self.migrations.borrow_mut();
        if migrations.running.contains(host) || !migrations.armed.remove(host) {
            return None;
        }
        let plan = state
            .store
            .migration_plan(host, rows, state.group_name_max_bytes);
        state.store.mark_not_shared(plan.unshared);
        if plan.writes.is_empty() {
            state.store.mark_migrated(host);
            return None;
        }
        migrations.running.insert(host.to_owned());
        Some((plan.writes, rows.to_vec()))
    }

    /// Writes the planned keys on this thread's runtime. The host is recorded
    /// as migrated only when no write failed; a refused session keeps its
    /// local placement and is not retried, a failed one is retried at the
    /// host's next connection.
    fn spawn_migration(
        &self,
        host: &str,
        writes: Vec<(SessionKey, String)>,
        rows: Vec<SessionRow>,
    ) {
        let (layout, window, daemon) = (
            self.layout.clone(),
            self.window.clone(),
            self.daemon.clone(),
        );
        let (migrations, host) = (self.migrations.clone(), host.to_owned());
        let max_bytes = self.group_name_max_bytes;
        tokio::task::spawn_local(async move {
            let mut unwritable = Vec::new();
            let mut failures = Vec::new();
            for (key, name) in &writes {
                let Some(row) = rows.iter().find(|row| row.key == *key) else {
                    continue;
                };
                let target = group_edit::target_of(row);
                match write_group(&daemon, &target, Some(name), max_bytes).await {
                    WriteOutcome::Written => {}
                    WriteOutcome::Unwritable => unwritable.push(key.clone()),
                    WriteOutcome::Failed { host_label, reason } => {
                        failures.push((host_label, reason));
                    }
                }
            }
            info!(
                host = %host,
                written = writes.len() - unwritable.len() - failures.len(),
                unwritable = unwritable.len(),
                failed = failures.len(),
                "group migration run"
            );
            let message = {
                let mut state = lock(&layout);
                state.store.mark_not_shared(unwritable);
                if failures.is_empty() {
                    state.store.mark_migrated(&host);
                }
                let saved = save_layout(&mut state);
                match failures.first() {
                    Some((host_label, reason)) => Some(format!(
                        "Cannot move {} of {} group placements to the daemon; first failure on {host_label}: {reason}",
                        failures.len(),
                        writes.len()
                    )),
                    None => saved,
                }
            };
            migrations.borrow_mut().running.remove(&host);
            if let Some(message) = message
                && let Err(err) = window.upgrade_in_event_loop(move |window| {
                    window.set_layout_error(message.into());
                })
            {
                error!(error = %err, "deliver the migration result to the UI");
            }
        });
    }
}

/// The remote hosts the sidebar follows, as the New session dialog lists them.
/// `None` until the first discovery answered; the dialog then discovers on
/// its own.
type SharedDialogHosts = Arc<Mutex<Option<Vec<DialogHost>>>>;

fn known_hosts_snapshot(hosts: &SharedDialogHosts) -> Option<Vec<DialogHost>> {
    hosts.lock().unwrap_or_else(PoisonError::into_inner).clone()
}

/// What the daemon loops take from `companion.toml` and the pending store.
struct FollowSettings {
    dialog_hosts: SharedDialogHosts,
    daemon: DaemonConfig,
    remotes: Vec<HostConfig>,
    idle_threshold: Duration,
    rules: Vec<PlacementRule>,
    group_name_max_bytes: usize,
    pending: Option<(PendingStore, Duration)>,
    setup: Option<Arc<setup::Setup>>,
}

/// Follows the local daemon and every configured remote host on a background
/// thread and hands every update to the UI thread; the UI thread never waits
/// on a daemon.
fn follow_daemons(
    window: slint::Weak<AppWindow>,
    tray: slint::Weak<Tray>,
    settings: FollowSettings,
    layout: SharedLayout,
) {
    let FollowSettings {
        dialog_hosts,
        daemon,
        remotes,
        idle_threshold,
        rules,
        group_name_max_bytes,
        pending,
        setup,
    } = settings;
    std::thread::spawn(move || {
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(err) => {
                error!(error = %err, "cannot start the background runtime");
                let message = format!("cannot start the background runtime: {err}");
                if let Err(err) = window.upgrade_in_event_loop(move |window| {
                    window.set_error(message.into());
                }) {
                    error!(error = %err, "deliver the runtime error to the UI");
                }
                return;
            }
        };
        let hosts: Vec<LiveHost> = std::iter::once(LiveHost::local())
            .chain(remotes.iter().map(LiveHost::remote))
            .collect();
        let publisher = Rc::new(Publisher {
            window,
            tray,
            rules,
            group_name_max_bytes,
            daemon: daemon.clone(),
            migrations: Rc::default(),
            pending,
            layout,
            hosts: Rc::new(RefCell::new(HostsView::new(&hosts))),
            setup,
        });
        let local = tokio::task::LocalSet::new();
        local.block_on(&runtime, async {
            tokio::task::spawn_local(follow_discovered(
                publisher.clone(),
                daemon.clone(),
                remotes.clone(),
                idle_threshold,
                dialog_hosts,
            ));
            let loops: Vec<_> = hosts
                .into_iter()
                .map(|host| {
                    let (daemon, publisher) = (daemon.clone(), publisher.clone());
                    tokio::task::spawn_local(async move {
                        if host.label.is_some() {
                            let target = std::slice::from_ref(&host.target);
                            let refusal = match host_naming_this_machine(target, &daemon).await {
                                Ok(None) => None,
                                Ok(Some(_)) => Some(
                                    "invalid `hosts` entry: it names this machine, whose \
                                     sessions are already listed as local"
                                        .to_owned(),
                                ),
                                Err(err) => Some(format!("cannot check the `hosts` entry: {err}")),
                            };
                            if let Some(message) = refusal {
                                error!(host = %host.target, error = %message, "refusing the configured host");
                                publisher.refuse(&host, &message);
                                return;
                            }
                        }
                        let max_bytes = publisher.group_name_max_bytes;
                        run_live(&host, &daemon, idle_threshold, max_bytes, |update| {
                            publisher.publish(&host, update);
                        })
                        .await;
                    })
                })
                .collect();
            for handle in loops {
                if let Err(err) = handle.await {
                    error!(error = %err, "a host loop ended");
                }
            }
        });
    });
}

/// Asks the local daemon for its peers every `daemon.discover_interval_secs`
/// and follows each reachable one. A peer that leaves the answer is treated
/// like a lost host: its loop stops and its rows stay, marked unreachable.
async fn follow_discovered(
    publisher: Rc<Publisher>,
    daemon: DaemonConfig,
    configured: Vec<HostConfig>,
    idle_threshold: Duration,
    dialog_hosts_out: SharedDialogHosts,
) {
    let mut followed: HashMap<String, (LiveHost, Option<tokio::task::JoinHandle<()>>)> =
        HashMap::new();
    let mut last_failure: Option<String> = None;
    loop {
        match discover_records(&daemon).await {
            Ok(records) => {
                if last_failure.take().is_some() {
                    info!("host discovery works again");
                }
                let plan = plan_discovery(&records, &configured);
                debug!(
                    records = records.len(),
                    connect = plan.connect.len(),
                    incompatible = plan.incompatible.len(),
                    "host discovery answered"
                );
                for (key, (host, handle)) in &mut followed {
                    if handle.is_some() && !plan.connect.iter().any(|found| &found.key == key) {
                        if let Some(handle) = handle.take() {
                            handle.abort();
                        }
                        info!(host = %key, "host no longer discovered");
                        publisher.publish(
                            host,
                            LiveUpdate::Lost {
                                reason: "no longer discovered".to_owned(),
                            },
                        );
                    }
                }
                for (target, route) in &plan.configured_routes {
                    set_route(target, route);
                }
                for found in &plan.connect {
                    // The route follows the peer's current port even while its
                    // loop runs; the next reconnect dials it.
                    let host = LiveHost::discovered(found);
                    if followed
                        .get(&found.key)
                        .is_some_and(|(_, handle)| handle.is_some())
                    {
                        continue;
                    }
                    match host_naming_this_machine(std::slice::from_ref(&found.key), &daemon).await
                    {
                        Ok(None) => {}
                        Ok(Some(_)) => {
                            debug!(host = %found.key, "discovered peer is this machine");
                            continue;
                        }
                        Err(err) => {
                            warn!(host = %found.key, error = %err, "cannot check the discovered host");
                            continue;
                        }
                    }
                    info!(host = %found.key, label = %found.label, route = %found.route, "following discovered host");
                    publisher.add_host(&host);
                    let handle = {
                        let (host, daemon, publisher) =
                            (host.clone(), daemon.clone(), publisher.clone());
                        tokio::task::spawn_local(async move {
                            let max_bytes = publisher.group_name_max_bytes;
                            run_live(&host, &daemon, idle_threshold, max_bytes, |update| {
                                publisher.publish(&host, update);
                            })
                            .await;
                        })
                    };
                    followed.insert(found.key.clone(), (host, Some(handle)));
                }
                let mut followed_plan = plan.clone();
                followed_plan.connect.retain(|found| {
                    followed
                        .get(&found.key)
                        .is_some_and(|(_, handle)| handle.is_some())
                });
                *dialog_hosts_out
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner) =
                    Some(dialog_hosts(&configured, &followed_plan));
                publisher.set_incompatible(
                    plan.incompatible
                        .iter()
                        .map(|peer| {
                            format!(
                                "{}: incompatible daemon (protocol {})",
                                peer.label, peer.protocol_version
                            )
                        })
                        .collect(),
                );
            }
            Err(err) => {
                // One warning per distinct failure; the retry every interval
                // stays quiet until the cause changes or discovery recovers.
                let message = err.to_string();
                if last_failure.as_deref() == Some(message.as_str()) {
                    debug!(error = %message, "host discovery still failing");
                } else {
                    warn!(error = %message, "host discovery failed; local and configured hosts keep working");
                    last_failure = Some(message);
                }
            }
        }
        tokio::time::sleep(daemon.discover_interval()).await;
    }
}

/// What an attach click needs from startup: the config file location and
/// either the parsed attach settings or why the config is unusable.
#[derive(Clone)]
struct AttachContext {
    config_path: Result<PathBuf, String>,
    settings: Result<AttachConfig, String>,
}

/// Attaches on a background thread (the launcher waits out its observation
/// window) and shows a failure in the window; `reveal` also opens the window
/// on failure, for attaches started from the tray while it is hidden.
fn start_attach(
    window: slint::Weak<AppWindow>,
    context: AttachContext,
    key: SessionKey,
    refusal: String,
    reveal: bool,
) {
    std::thread::spawn(move || {
        let outcome = attach_outcome(&context, &key, &key.session_id, &refusal);
        if let Err(message) = &outcome {
            warn!(session = %key.session_id, error = %message, "attach failed");
        }
        let applied = window.upgrade_in_event_loop(move |window| {
            let failed = outcome.is_err();
            window.set_attach_error(outcome.err().unwrap_or_default().into());
            if reveal
                && failed
                && let Err(err) = window.show()
            {
                error!(error = %err, "show window after a failed attach");
            }
        });
        if let Err(err) = applied {
            error!(error = %err, "deliver attach outcome to the UI");
        }
    });
}

/// The folder path or link URL to open for `key`, from the session's current
/// snapshot; an error text when it is no longer offered.
fn open_target(rows: &[SessionRow], key: &SessionKey, kind: OpenKind) -> Result<String, String> {
    let row = rows.iter().find(|row| &row.key == key).ok_or_else(|| {
        format!(
            "Cannot open {} of {}: the session is gone",
            kind.label(),
            key.session_id
        )
    })?;
    let target = match kind {
        OpenKind::Folder => row
            .open
            .existing_folder()
            .map(|path| path.to_string_lossy().into_owned()),
        OpenKind::Link => row.open.link.clone(),
    };
    target.ok_or_else(|| {
        format!(
            "Cannot open {} of {}: {}",
            kind.label(),
            key.session_id,
            match kind {
                OpenKind::Folder => "the folder does not exist",
                OpenKind::Link => "the session has no http(s) link",
            }
        )
    })
}

/// Runs the opener on a background thread (the launcher waits out its
/// observation window) and shows a failure in the window.
fn start_open(
    window: slint::Weak<AppWindow>,
    settings: Result<OpenConfig, String>,
    kind: OpenKind,
    session_id: String,
    target: Result<String, String>,
) {
    std::thread::spawn(move || {
        let outcome = target.and_then(|target| {
            let settings = settings?;
            open(&settings, kind, &target)
                .map_err(|err| format!("Cannot open {} of {session_id}: {err}", kind.label()))
        });
        if let Err(message) = &outcome {
            warn!(session = %session_id, kind = kind.label(), error = %message, "open failed");
        }
        let applied = window.upgrade_in_event_loop(move |window| {
            if let Err(message) = outcome {
                window.set_open_error(message.into());
            }
        });
        if let Err(err) = applied {
            error!(error = %err, "deliver open outcome to the UI");
        }
    });
}

/// Renames on a background thread and shows a failure in the window. The new
/// name appears through the daemon's session event, not from this outcome.
fn start_rename(
    window: slint::Weak<AppWindow>,
    daemon: Result<DaemonConfig, String>,
    host: String,
    host_name: Option<String>,
    session_id: String,
    input: String,
) {
    std::thread::spawn(move || {
        let outcome = rename_outcome(&daemon, &host, host_name.as_deref(), &session_id, &input);
        if let Err(message) = &outcome {
            warn!(session = %session_id, error = %message, "rename failed");
        }
        let applied = window.upgrade_in_event_loop(move |window| {
            window.set_rename_error(outcome.err().unwrap_or_default().into());
        });
        if let Err(err) = applied {
            error!(error = %err, "deliver rename outcome to the UI");
        }
    });
}

fn rename_outcome(
    daemon: &Result<DaemonConfig, String>,
    host: &str,
    host_name: Option<&str>,
    session_id: &str,
    input: &str,
) -> Result<(), String> {
    let daemon = daemon.as_ref().map_err(Clone::clone)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|err| format!("cannot start the background runtime: {err}"))?;
    runtime
        .block_on(rename_session(
            daemon,
            host,
            session_id,
            name_from_input(input),
        ))
        .map_err(|err| match host_name {
            Some(name) => format!("Cannot rename {session_id} on {name}: {err}"),
            None => format!("Cannot rename {session_id}: {err}"),
        })
}

/// `label` names the session in the failure text.
fn attach_outcome(
    context: &AttachContext,
    key: &SessionKey,
    label: &str,
    refusal: &str,
) -> Result<(), String> {
    if !refusal.is_empty() {
        return Err(format!("Cannot attach: {refusal}"));
    }
    let settings = context.settings.as_ref().map_err(Clone::clone)?;
    let config_path = context.config_path.as_ref().map_err(Clone::clone)?;
    attach(settings, config_path, key).map_err(|err| match err {
        AttachError::NotConfigured { .. } => err.to_string(),
        other => format!("Cannot attach {label}: {other}"),
    })
}

/// `desktop install` and `desktop uninstall`; they never open a window.
#[cfg(target_os = "linux")]
fn run_desktop_command(action: DesktopAction) -> Result<(), slint::PlatformError> {
    let (verb, result) = match action {
        DesktopAction::Install => ("installed", desktop::install()),
        DesktopAction::Uninstall => ("uninstalled", desktop::uninstall()),
    };
    match result {
        Ok(root) => {
            println!("desktop entry and icons {verb} under {}:", root.display());
            print!("{}", desktop::summary(&root));
            Ok(())
        }
        Err(err) => {
            let name = match action {
                DesktopAction::Install => "install",
                DesktopAction::Uninstall => "uninstall",
            };
            eprintln!("desktop {name}: {err}");
            std::process::exit(1);
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn run_desktop_command(_action: DesktopAction) -> Result<(), slint::PlatformError> {
    use cli::DESKTOP_COMMAND;
    eprintln!("`{DESKTOP_COMMAND}` is Linux-only; macOS uses the app bundle");
    std::process::exit(2);
}

fn main() -> Result<(), slint::PlatformError> {
    completion::complete_env();
    match Cli::parse().command {
        None => run_sidebar(),
        Some(Command::NewSession(args)) => {
            if let Err(err) = logging::init() {
                eprintln!("logging disabled: {err}");
            }
            new_session::run(args)
        }
        Some(Command::Assistant(args)) => {
            if let Err(err) = logging::init() {
                eprintln!("logging disabled: {err}");
            }
            assistant::run(args)
        }
        Some(Command::Desktop { action }) => run_desktop_command(action),
        Some(Command::Completions { shell, dynamic }) => {
            if let Err(err) = completion::run(shell, dynamic) {
                eprintln!("completions: {err}");
                std::process::exit(1);
            }
            Ok(())
        }
    }
}

/// Runs the sidebar as a macOS accessory app (menu bar item and window, no
/// Dock icon). It is set per process rather than with `LSUIElement`, which
/// would also apply to the new-session dialog started from the same executable.
#[cfg(target_os = "macos")]
fn accessory_backend_applies() -> bool {
    // A SLINT_BACKEND other than winit (the headless UI agent) has no event
    // loop builder to configure and no Dock icon.
    std::env::var("SLINT_BACKEND").map_or(true, |name| name.to_lowercase().starts_with("winit"))
}

#[cfg(target_os = "macos")]
fn select_accessory_backend() -> Result<(), slint::PlatformError> {
    use slint::winit_030::winit::{
        event_loop::EventLoop,
        platform::macos::{ActivationPolicy, EventLoopBuilderExtMacOS},
    };
    let mut builder = EventLoop::with_user_event();
    builder.with_activation_policy(ActivationPolicy::Accessory);
    slint::BackendSelector::new()
        .with_winit_event_loop_builder(builder)
        .select()
}

fn run_sidebar() -> Result<(), slint::PlatformError> {
    let log_error = match logging::init() {
        Ok(path) => {
            info!(log = %path.display(), "companion started");
            String::new()
        }
        Err(err) => {
            eprintln!("logging disabled: {err}");
            format!("Logging disabled: {err}")
        }
    };
    #[cfg(target_os = "macos")]
    if accessory_backend_applies() {
        select_accessory_backend()?;
    }
    let window = AppWindow::new()?;
    window.set_log_error(log_error.into());
    // Compositor window rules match on this id (Wayland app_id); it needs the
    // platform, which the first component creates, and must precede show().
    slint::set_xdg_app_id(APP_ID)?;
    let tray = Tray::new()?;

    let (layout, layout_error) = match layout_path() {
        Ok(path) => LayoutStore::open(path),
        Err(err) => {
            error!(error = %err, "locate the layout file");
            (LayoutStore::unsaved(), Some(err))
        }
    };
    let layout_error = layout_error.map(|err| err.to_string());
    let layout: SharedLayout = Arc::new(Mutex::new(LayoutState {
        store: layout,
        rows: Vec::new(),
        hosts: Vec::new(),
        load_error: layout_error.clone(),
        rules: Vec::new(),
        title_max_chars: ViewConfig::default().section_title_max_chars,
        group_name_max_bytes: LayoutConfig::default().group_name_max_bytes,
        daemon: Err("companion.toml is not loaded".to_owned()),
        filter: SessionFilter::default(),
        queries: HashMap::new(),
    }));
    {
        let mut state = lock(&layout);
        let view = state.store.layout().view();
        state.filter = SessionFilter::from_stored(&state.store.layout().filter(view));
    }
    window.set_layout_error(layout_error.clone().unwrap_or_default().into());
    window.set_view_names(ModelRc::new(VecModel::from(
        View::ALL
            .iter()
            .map(|view| slint::SharedString::from(view.tab_label()))
            .collect::<Vec<_>>(),
    )));
    let view = lock(&layout).store.layout().view();
    window.set_view_index(i32::try_from(view.index()).unwrap_or_default());
    window.set_editable(view.is_editable());

    let known_hosts: SharedDialogHosts = Arc::default();
    let config_path = Config::path().map_err(|err| err.to_string());
    let (attach_context, rename_daemon, open_settings) = match Config::load() {
        Ok(config) => {
            let open_settings = Ok(config.open());
            let rename_daemon = Ok(config.daemon.clone());
            {
                let mut state = lock(&layout);
                state.title_max_chars = config.view.section_title_max_chars;
                state.rules = config.layout.rules.clone();
                state.group_name_max_bytes = config.layout.group_name_max_bytes;
                state.daemon = Ok(config.daemon.clone());
            }
            let context = AttachContext {
                config_path,
                settings: Ok(config.attach()),
            };
            let pending = match PendingStore::open() {
                Ok(store) => Some((store, config.new_session.pending_placement_max_age())),
                Err(err) => {
                    error!(error = %err, "locate the pending placements");
                    None
                }
            };
            let setup = setup::Setup::new(&window, &config);
            follow_daemons(
                window.as_weak(),
                tray.as_weak(),
                FollowSettings {
                    dialog_hosts: known_hosts.clone(),
                    daemon: config.daemon,
                    remotes: config.hosts,
                    idle_threshold: config.tray.idle_threshold(),
                    rules: config.layout.rules,
                    group_name_max_bytes: config.layout.group_name_max_bytes,
                    pending,
                    setup: Some(setup),
                },
                layout.clone(),
            );
            (context, rename_daemon, open_settings)
        }
        Err(err) => {
            error!(error = %err, "load companion.toml");
            window.set_error(err.to_string().into());
            let rename_daemon = Err(err.to_string());
            let context = AttachContext {
                config_path,
                settings: Err(err.to_string()),
            };
            (context, rename_daemon, Err(err.to_string()))
        }
    };
    for kind in [OpenKind::Folder, OpenKind::Link] {
        let weak = window.as_weak();
        let layout = layout.clone();
        let settings = open_settings.clone();
        let handler = move |host: slint::SharedString, id: slint::SharedString| {
            let key = SessionKey {
                host: host.into(),
                session_id: id.into(),
            };
            info!(host = %key.host, session = %key.session_id, kind = kind.label(), "open requested from the row");
            let target = open_target(&lock(&layout).rows, &key, kind);
            start_open(weak.clone(), settings.clone(), kind, key.session_id, target);
        };
        match kind {
            OpenKind::Folder => window.on_open_folder(handler),
            OpenKind::Link => window.on_open_link(handler),
        }
    }
    let resume_context = resume::ResumeContext {
        daemon: rename_daemon.clone(),
        attach: attach_context.clone(),
        in_flight: Arc::default(),
    };
    let confirm_handle = confirm::install(
        &window,
        rename_daemon.clone(),
        resume_context.clone(),
        layout.clone(),
    );
    detail::install(&window, layout.clone());
    {
        let weak = window.as_weak();
        let layout = layout.clone();
        window.on_rename_session(move |host, id, name| {
            info!(host = %host, session = %id, "rename requested from the context menu");
            let key = SessionKey {
                host: host.into(),
                session_id: id.into(),
            };
            let host_name = remote_host_name(&layout, &key);
            start_rename(
                weak.clone(),
                rename_daemon.clone(),
                key.host,
                host_name,
                key.session_id,
                name.into(),
            );
        });
    }
    {
        let weak = window.as_weak();
        let context = attach_context.clone();
        tray.on_attach_entry(move |host, id, refusal| {
            let key = SessionKey {
                host: host.into(),
                session_id: id.into(),
            };
            info!(host = %key.host, session = %key.session_id, source = "tray", "attach requested");
            start_attach(weak.clone(), context.clone(), key, refusal.into(), true);
        });
    }
    {
        let weak = window.as_weak();
        let layout = layout.clone();
        let context = resume_context.clone();
        let confirm = confirm_handle.clone();
        window.on_request_resume(move |host, id, name| {
            let key = SessionKey {
                host: host.into(),
                session_id: id.into(),
            };
            resume::start_resume(
                weak.clone(),
                context.clone(),
                confirm.clone(),
                &layout,
                key,
                name.into(),
            );
        });
    }
    {
        let weak = window.as_weak();
        let layout = layout.clone();
        window.on_attach(move |host, id, refusal| {
            let key = SessionKey {
                host: host.into(),
                session_id: id.into(),
            };
            // An ended session the daemon can resume is resumed, then attached.
            let resumable = lock(&layout)
                .rows
                .iter()
                .find(|row| row.key == key)
                .map(|row| {
                    (
                        row.resume.is_available() && !row.host_unreachable,
                        row.display_name.clone(),
                    )
                });
            if let Some((true, name)) = resumable {
                resume::start_resume(
                    weak.clone(),
                    resume_context.clone(),
                    confirm_handle.clone(),
                    &layout,
                    key,
                    name,
                );
                return;
            }
            start_attach(
                weak.clone(),
                attach_context.clone(),
                key,
                refusal.into(),
                false,
            );
        });
    }

    {
        let weak = window.as_weak();
        let layout = layout.clone();
        window.on_view_selected(move |index| {
            let Some(view) = usize::try_from(index)
                .ok()
                .and_then(|index| View::ALL.get(index).copied())
            else {
                warn!(index, "view selected outside the view list");
                return;
            };
            spawn_view_change(weak.clone(), layout.clone(), view);
        });
    }

    window.on_payload(|row| {
        let mut transfer = slint::DataTransfer::default();
        transfer.set_user_data(Rc::new(SessionKey {
            host: row.host.into(),
            session_id: row.id.into(),
        }));
        transfer
    });
    window.on_group_payload(|id| {
        let mut transfer = slint::DataTransfer::default();
        transfer.set_user_data(Rc::new(GroupDrag(id.into())));
        transfer
    });
    window.on_is_group(|transfer| {
        transfer
            .user_data()
            .is_some_and(|data| data.downcast_ref::<GroupDrag>().is_some())
    });
    {
        let requests = spawn_layout_worker(window.as_weak(), layout.clone());
        let send = {
            let requests = requests.clone();
            move |request: LayoutRequest| {
                // The dragged row or header is destroyed by the model change,
                // which arrives from the worker thread after the callback
                // that sent the request has returned.
                if requests.send(request).is_err() {
                    error!("the layout worker has stopped");
                }
            }
        };
        window.on_drop({
            let send = send.clone();
            move |transfer, group, anchor_host, anchor_id, after| {
                let Some(key) = transfer
                    .user_data()
                    .and_then(|data| data.downcast_ref::<SessionKey>().cloned())
                else {
                    warn!("drop without a session payload");
                    return;
                };
                let anchor = (!anchor_id.is_empty()).then(|| {
                    (
                        SessionKey {
                            host: anchor_host.into(),
                            session_id: anchor_id.into(),
                        },
                        after,
                    )
                });
                send(LayoutRequest::Drop {
                    key,
                    group: group.into(),
                    anchor,
                });
            }
        });
        window.on_drop_group({
            let send = send.clone();
            move |transfer, anchor, after| {
                let Some(GroupDrag(id)) = transfer
                    .user_data()
                    .and_then(|data| data.downcast_ref::<GroupDrag>().cloned())
                else {
                    warn!("group drop without a group payload");
                    return;
                };
                send(LayoutRequest::MoveGroup {
                    id,
                    anchor: anchor.into(),
                    after,
                });
            }
        });
        window.on_create_group({
            let send = send.clone();
            move |name| send(LayoutRequest::CreateGroup(name.into()))
        });
        window.on_create_group_and_move({
            let send = send.clone();
            move |name, host, id| {
                send(LayoutRequest::CreateGroupAndMove {
                    name: name.into(),
                    key: SessionKey {
                        host: host.into(),
                        session_id: id.into(),
                    },
                })
            }
        });
        window.on_rename_group({
            let send = send.clone();
            move |id, name| {
                send(LayoutRequest::RenameGroup {
                    id: id.into(),
                    name: name.into(),
                })
            }
        });
        window.on_toggle_group({
            let send = send.clone();
            move |id, collapsed| {
                send(LayoutRequest::SetCollapsed {
                    id: id.into(),
                    collapsed,
                })
            }
        });
        window.on_move_to_group({
            let send = send.clone();
            move |host, id, group| {
                info!(host = %host, session = %id, group = %group, "move to group from the context menu");
                send(LayoutRequest::Drop {
                    key: SessionKey {
                        host: host.into(),
                        session_id: id.into(),
                    },
                    group: group.into(),
                    anchor: None,
                })
            }
        });
        window.on_return_to_rule({
            let send = send.clone();
            move |host, id| {
                info!(host = %host, session = %id, "return to rule placement from the context menu");
                send(LayoutRequest::ReturnToRule(SessionKey {
                    host: host.into(),
                    session_id: id.into(),
                }))
            }
        });
        window.on_filter_changed({
            let send = send.clone();
            move |query| send(LayoutRequest::SetFilter(query.into()))
        });
        window.on_host_selected({
            let send = send.clone();
            move |target| send(LayoutRequest::SetHost(target.into()))
        });
        window.on_project_selected({
            let send = send.clone();
            move |key| send(LayoutRequest::SetProject(key.into()))
        });
        window.on_state_toggled({
            let send = send.clone();
            move |key| send(LayoutRequest::ToggleState(key.into()))
        });
        window.on_delete_group(move |id| send(LayoutRequest::DeleteGroup(id.into())));
    }

    tray::apply_offline(&tray, "Not connected to the daemon");

    {
        let window = window.as_weak();
        tray.on_show_window(move || {
            if let Some(window) = window.upgrade()
                && let Err(err) = window.show()
            {
                error!(error = %err, "show window");
            }
        });
    }
    {
        let about: RefCell<Option<AboutCompanionWindow>> = RefCell::new(None);
        tray.on_show_about_companion(move || {
            let mut about = about.borrow_mut();
            let shown = match about.as_ref() {
                Some(window) => Ok(window),
                None => AboutCompanionWindow::new().map(|window| {
                    window.set_version(env!("CARGO_PKG_VERSION").into());
                    about.insert(window) as &AboutCompanionWindow
                }),
            };
            if let Err(err) = shown.and_then(|window| window.show()) {
                error!(error = %err, "show the about window");
            }
        });
    }
    {
        let about: RefCell<Option<AboutWindow>> = RefCell::new(None);
        tray.on_show_about(move || {
            let mut about = about.borrow_mut();
            let shown = match about.as_ref() {
                Some(window) => Ok(window),
                None => AboutWindow::new().map(|window| about.insert(window) as &AboutWindow),
            };
            if let Err(err) = shown.and_then(|window| window.show()) {
                error!(error = %err, "show the about window");
            }
        });
    }
    {
        let weak = window.as_weak();
        let known_hosts = known_hosts.clone();
        tray.on_new_session(move || {
            let outcome = launch::spawn_new_session(
                None,
                None,
                None,
                known_hosts_snapshot(&known_hosts).as_deref(),
            );
            if let Err(message) = &outcome {
                error!(error = %message, "start the new-session dialog from the tray");
            }
            let Some(window) = weak.upgrade() else { return };
            let failed = outcome.is_err();
            window.set_launch_error(outcome.err().unwrap_or_default().into());
            // The sidebar may be hidden; reveal it so the failure is seen.
            if failed && let Err(err) = window.show() {
                error!(error = %err, "show window after a failed dialog start");
            }
        });
    }
    {
        let weak = window.as_weak();
        let known_hosts = known_hosts.clone();
        window.on_new_session(move |host, project, group| {
            let outcome = launch::spawn_new_session(
                Some(host.as_str()).filter(|host| !host.is_empty() && *host != LOCAL_HOST),
                Some(project.as_str()).filter(|id| !id.is_empty()),
                Some(group.as_str()).filter(|id| !id.is_empty()),
                known_hosts_snapshot(&known_hosts).as_deref(),
            );
            if let Err(message) = &outcome {
                error!(error = %message, "start the new-session dialog from the sidebar");
            }
            if let Some(window) = weak.upgrade() {
                window.set_launch_error(outcome.err().unwrap_or_default().into());
            }
        });
    }
    {
        let weak = window.as_weak();
        let known_hosts = known_hosts.clone();
        tray.on_assistant(move || {
            let outcome =
                launch::spawn_assistant(None, None, known_hosts_snapshot(&known_hosts).as_deref());
            if let Err(message) = &outcome {
                error!(error = %message, "start the assistant dialog from the tray");
            }
            let Some(window) = weak.upgrade() else { return };
            let failed = outcome.is_err();
            window.set_launch_error(outcome.err().unwrap_or_default().into());
            // The sidebar may be hidden; reveal it so the failure is seen.
            if failed && let Err(err) = window.show() {
                error!(error = %err, "show window after a failed dialog start");
            }
        });
    }
    {
        let weak = window.as_weak();
        let known_hosts = known_hosts.clone();
        window.on_assistant(move |project| {
            let outcome = launch::spawn_assistant(
                None,
                Some(project.as_str()).filter(|id| !id.is_empty()),
                known_hosts_snapshot(&known_hosts).as_deref(),
            );
            if let Err(message) = &outcome {
                error!(error = %message, "start the assistant dialog from the sidebar");
            }
            if let Some(window) = weak.upgrade() {
                window.set_launch_error(outcome.err().unwrap_or_default().into());
            }
        });
    }
    tray.on_quit(|| {
        if let Err(err) = slint::quit_event_loop() {
            error!(error = %err, "quit the event loop");
        }
    });

    window.show()?;
    slint::run_event_loop_until_quit()?;
    Ok(())
}
