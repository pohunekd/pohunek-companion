use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, mpsc};
use std::time::{Duration, Instant};

mod confirm;
mod launch;
mod logging;
mod new_session;
mod tray;

use pohunek_companion_core::{
    AttachConfig, AttachError, Config, DaemonConfig, GroupError, GroupPlacement, GroupSection,
    INBOX_ID, LOCAL_HOST, LayoutStore, LiveUpdate, MoveError, NEEDS_YOU_ID, OpenConfig, OpenKind,
    OwnerState, PendingStore, Placement, PlacementRule, ProjectChip, ProjectSelection,
    SessionFilter, SessionKey, SessionRow, StateChip, TraySummary, View, ViewConfig, attach,
    clip_display, filter_sections, layout_path, name_from_input, open, project_chips,
    rename_session, run_live, state_chips, state_from_key,
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
                id: s.key.session_id.into(),
                can_stop: s.can_stop,
                can_remove: s.can_remove,
                open_folder: s.open.existing_folder().is_some(),
                open_link: s.open.link.is_some(),
                attach_refusal: s
                    .attach_refusal
                    .map(|reason| reason.to_string())
                    .unwrap_or_default()
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
    state_chips: Vec<StateChip>,
}

/// The identifier the window sends back for a project chip; stable across
/// session list changes, unlike the chip's position.
fn chip_key(selection: &ProjectSelection) -> String {
    match selection {
        ProjectSelection::All => "all".to_owned(),
        ProjectSelection::NoProject => "none".to_owned(),
        ProjectSelection::Project(project) => format!("project:{}", project.id),
    }
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
    let (project_chips, project_index) = project_chips(&state.rows, state.filter.project());
    Arranged {
        filter_empty: state.filter.is_active()
            && !state.rows.is_empty()
            && sections.iter().all(|section| section.rows.is_empty()),
        state_chips: state_chips(&state.rows, state.filter.states()),
        sections,
        project_chips,
        project_index,
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
        state_chips,
    } = arranged;
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
    window.set_filter_project_id(
        match project_chips.get(project_index).map(|chip| &chip.selection) {
            Some(ProjectSelection::Project(project)) => project.id.as_str(),
            _ => "",
        }
        .into(),
    );
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
    load_error: Option<String>,
    /// Placement rules from `companion.toml`, applied to sessions the owner
    /// hands back with "Return to rule placement".
    rules: Vec<PlacementRule>,
    /// Longest title of a derived section, from `view.section_title_max_chars`.
    title_max_chars: usize,
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
        LayoutRequest::Drop { key, group, anchor } => {
            let placement = match &anchor {
                Some((anchor, false)) => Placement::Before(anchor),
                Some((anchor, true)) => Placement::After(anchor),
                None => Placement::End,
            };
            Refusal {
                drop: state
                    .store
                    .move_session(&key, &group, placement)
                    .err()
                    .map(|err| {
                        warn!(session = %key.session_id, group = %group, error = %err, "drop refused");
                        format!("Cannot move {}: {err}", key.session_id)
                    }),
                group: None,
            }
        }
        LayoutRequest::CreateGroup(name) => group(
            state
                .store
                .create_group(&name)
                .err()
                .and_then(|err| group_refusal("create the group", "new", &err)),
        ),
        LayoutRequest::CreateGroupAndMove { name, key } => {
            let outcome = state.store.create_group_and_move(&name, &key);
            group(outcome.err().map(|err| {
                warn!(session = %key.session_id, reason = err.kind(), "group operation refused");
                format!("Cannot create the group: {err}")
            }))
        }
        LayoutRequest::RenameGroup { id, name } => group(
            state
                .store
                .rename_group(&id, &name)
                .err()
                .and_then(|err| group_refusal("rename the group", &id, &err)),
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
        LayoutRequest::ReturnToRule(key) => {
            let LayoutState {
                store, rows, rules, ..
            } = state;
            let outcome = match rows.iter().find(|row| row.key == key) {
                Some(row) => store.return_to_rule(row, rules),
                None => Err(MoveError::UnknownSession(key.session_id.clone())),
            };
            Refusal {
                drop: outcome.err().map(|err| {
                    warn!(session = %key.session_id, error = %err, "return to rule refused");
                    format!("Cannot return {} to rule placement: {err}", key.session_id)
                }),
                group: None,
            }
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
        LayoutRequest::SetProject(key) => {
            let (chips, _) = project_chips(&state.rows, state.filter.project());
            match chips
                .into_iter()
                .find(|chip| chip_key(&chip.selection) == key)
            {
                Some(chip) => {
                    state.filter.set_project(chip.selection);
                    store_filter(state);
                }
                None => warn!(chip = %key, "project chip is no longer offered"),
            }
            Refusal::default()
        }
        LayoutRequest::DeleteGroup(id) => group(
            state
                .store
                .delete_group(&id)
                .err()
                .and_then(|err| group_refusal("delete the group", &id, &err)),
        ),
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
        for request in receiver {
            let mut state = lock(&layout);
            let Refusal {
                drop,
                group: group_error,
            } = apply_request(&mut state, request);
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

/// Follows the daemon on a background thread and hands every update to the UI
/// thread; the UI thread never waits on the daemon.
fn follow_daemon(
    window: slint::Weak<AppWindow>,
    tray: slint::Weak<Tray>,
    daemon: DaemonConfig,
    idle_threshold: Duration,
    rules: Vec<PlacementRule>,
    pending: Option<(PendingStore, Duration)>,
    layout: SharedLayout,
) {
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
        runtime.block_on(run_live(&daemon, idle_threshold, |update| {
            let summary = match &update {
                LiveUpdate::Sessions(sessions) => Some(TraySummary::from_rows(sessions)),
                _ => None,
            };
            // Reconciled and saved here so the UI thread never waits on disk;
            // the lock is held until the update is queued.
            let mut guard = None;
            let arranged = match &update {
                LiveUpdate::Sessions(sessions) => {
                    let mut state = lock(&layout);
                    let keys: Vec<SessionKey> = sessions.iter().map(|s| s.key.clone()).collect();
                    state.store.reconcile(&keys);
                    if let Some((pending, max_age)) = &pending {
                        apply_pending(&mut state.store, pending, &keys, *max_age);
                    }
                    state.store.apply_rules(sessions, &rules);
                    let layout_error = save_layout(&mut state);
                    state.rows = sessions.clone();
                    let arranged = arrange(&state);
                    guard = Some(state);
                    Some((arranged, layout_error))
                }
                _ => None,
            };
            let tray = tray.clone();
            let applied = window.upgrade_in_event_loop(move |window| match update {
                LiveUpdate::Connected => {
                    window.set_error("".into());
                    window.set_status("".into());
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
                LiveUpdate::Reconnecting { attempt, reason } => {
                    if let Some(tray) = tray.upgrade() {
                        tray::apply_offline(&tray, "Not connected to the daemon");
                    }
                    window.set_status(format!("Reconnecting (attempt {attempt}): {reason}").into());
                }
            });
            drop(guard);
            if let Err(err) = applied {
                error!(error = %err, "deliver a daemon update to the UI");
            }
        }));
    });
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
        let outcome = attach_outcome(&context, &key, &refusal);
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
    session_id: String,
    input: String,
) {
    std::thread::spawn(move || {
        let outcome = rename_outcome(&daemon, &host, &session_id, &input);
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
    session_id: &str,
    input: &str,
) -> Result<(), String> {
    if host != LOCAL_HOST {
        return Err(format!(
            "Cannot rename {session_id}: only the local daemon is supported"
        ));
    }
    let daemon = daemon.as_ref().map_err(Clone::clone)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|err| format!("cannot start the background runtime: {err}"))?;
    runtime
        .block_on(rename_session(daemon, session_id, name_from_input(input)))
        .map_err(|err| format!("Cannot rename {session_id}: {err}"))
}

fn attach_outcome(context: &AttachContext, key: &SessionKey, refusal: &str) -> Result<(), String> {
    if !refusal.is_empty() {
        return Err(format!("Cannot attach: {refusal}"));
    }
    let settings = context.settings.as_ref().map_err(Clone::clone)?;
    let config_path = context.config_path.as_ref().map_err(Clone::clone)?;
    attach(settings, config_path, key).map_err(|err| match err {
        AttachError::NotConfigured { .. } => err.to_string(),
        other => format!("Cannot attach {}: {other}", key.session_id),
    })
}

fn main() -> Result<(), slint::PlatformError> {
    use launch::NEW_SESSION_COMMAND;
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        None => run_sidebar(),
        Some(NEW_SESSION_COMMAND) => {
            let parsed = new_session::parse_args(args).unwrap_or_else(|message| {
                eprintln!(
                    "{message}\nusage: pohunek-companion [{NEW_SESSION_COMMAND} [--project ID] [--group ID]]"
                );
                std::process::exit(2);
            });
            if let Err(err) = logging::init() {
                eprintln!("logging disabled: {err}");
            }
            new_session::run(parsed)
        }
        Some(other) => {
            eprintln!(
                "unknown command `{other}`\nusage: pohunek-companion [{NEW_SESSION_COMMAND} [--project ID] [--group ID]]"
            );
            std::process::exit(2);
        }
    }
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
        load_error: layout_error.clone(),
        rules: Vec::new(),
        title_max_chars: ViewConfig::default().section_title_max_chars,
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

    let config_path = Config::path().map_err(|err| err.to_string());
    let (attach_context, rename_daemon, open_settings) = match Config::load() {
        Ok(config) => {
            let open_settings = Ok(config.open());
            let rename_daemon = Ok(config.daemon.clone());
            {
                let mut state = lock(&layout);
                state.title_max_chars = config.view.section_title_max_chars;
                state.rules = config.layout.rules.clone();
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
            follow_daemon(
                window.as_weak(),
                tray.as_weak(),
                config.daemon,
                config.tray.idle_threshold(),
                config.layout.rules,
                pending,
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
    confirm::install(&window, rename_daemon.clone(), layout.clone());
    {
        let weak = window.as_weak();
        window.on_rename_session(move |host, id, name| {
            info!(host = %host, session = %id, "rename requested from the context menu");
            start_rename(
                weak.clone(),
                rename_daemon.clone(),
                host.into(),
                id.into(),
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
        window.on_attach(move |host, id, refusal| {
            let key = SessionKey {
                host: host.into(),
                session_id: id.into(),
            };
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
        tray.on_new_session(move || {
            let outcome = launch::spawn_new_session(None, None);
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
        window.on_new_session(move |project, group| {
            let outcome = launch::spawn_new_session(
                Some(project.as_str()).filter(|id| !id.is_empty()),
                Some(group.as_str()).filter(|id| !id.is_empty()),
            );
            if let Err(message) = &outcome {
                error!(error = %message, "start the new-session dialog from the sidebar");
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
