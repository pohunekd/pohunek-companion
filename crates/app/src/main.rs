use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, mpsc};
use std::time::Duration;

mod logging;
mod tray;

use pohunek_companion_core::{
    AttachConfig, AttachError, Config, DaemonConfig, GroupSection, LayoutStore, LiveUpdate,
    NEEDS_YOU_ID, OwnerState, Placement, SessionKey, SessionRow, TraySummary, attach,
    clip_display, layout_path, run_live, with_needs_you,
};
use slint::{ComponentHandle, ModelRc, VecModel};
use tracing::{error, info, warn};

slint::include_modules!();

const APP_ID: &str = "pohunek-companion";
/// Longest badge link or role text shown in a row.
const BADGE_MAX_CHARS: usize = 32;

fn owner_state_kind(state: OwnerState) -> OwnerStateKind {
    match state {
        OwnerState::NeedsOwner => OwnerStateKind::NeedsOwner,
        OwnerState::Working => OwnerStateKind::Working,
        OwnerState::Idle => OwnerStateKind::Idle,
        OwnerState::Ended => OwnerStateKind::Ended,
        OwnerState::NoAgentState => OwnerStateKind::NoAgentState,
    }
}

fn session_rows(sessions: Vec<SessionRow>) -> Vec<SessionRowData> {
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
            SessionRowData {
                name: s.display_name.into(),
                agent: s.agent.into(),
                state: s.state.into(),
                owner_state: owner_state_kind(s.owner_state),
                owner_label: s.owner_state.label().into(),
                badge: badge.into(),
                badge_role: badge_role.into(),
                host: s.key.host.into(),
                id: s.key.session_id.into(),
                attach_refusal: s
                    .attach_refusal
                    .map(|reason| reason.to_string())
                    .unwrap_or_default()
                    .into(),
            }
        })
        .collect()
}

fn group_data(sections: Vec<GroupSection>) -> Vec<GroupData> {
    sections
        .into_iter()
        .map(|section| GroupData {
            needs_you: section.id == NEEDS_YOU_ID,
            id: section.id.into(),
            name: section.name.into(),
            count: i32::try_from(section.rows.len()).unwrap_or(i32::MAX),
            rows: ModelRc::new(VecModel::from(session_rows(section.rows))),
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

/// A drop to apply to the layout: the dragged session, the target group, and
/// the anchor session with whether the drop lands after it (none: group end).
struct DropRequest {
    key: SessionKey,
    group: String,
    anchor: Option<(SessionKey, bool)>,
}

/// Applies drops one at a time, in the order they were made, off the UI thread
/// because the save waits on disk. Sessions are addressed by key, so daemon
/// updates that arrived during the drag do not matter.
fn spawn_drop_worker(
    window: slint::Weak<AppWindow>,
    layout: SharedLayout,
) -> mpsc::Sender<DropRequest> {
    let (sender, receiver) = mpsc::channel::<DropRequest>();
    std::thread::spawn(move || {
        for DropRequest { key, group, anchor } in receiver {
            let mut state = lock(&layout);
            let placement = match &anchor {
                Some((anchor, false)) => Placement::Before(anchor),
                Some((anchor, true)) => Placement::After(anchor),
                None => Placement::End,
            };
            let move_error = state
                .store
                .move_session(&key, &group, placement)
                .err()
                .map(|err| {
                    warn!(session = %key.session_id, group = %group, error = %err, "drop refused");
                    format!("Cannot move {}: {err}", key.session_id)
                });
            let layout_error = match (move_error, save_layout(&mut state)) {
                (Some(moved), Some(saved)) => Some(format!("{moved}\n{saved}")),
                (moved, saved) => moved.or(saved),
            };
            let sections = state.store.layout().arrange(&state.rows);
            if let Err(err) = window.upgrade_in_event_loop(move |window| {
                window.set_groups(ModelRc::new(VecModel::from(group_data(sections))));
                window.set_layout_error(layout_error.unwrap_or_default().into());
            }) {
                error!(error = %err, "deliver the moved layout to the UI");
            }
        }
    });
    sender
}

/// Follows the daemon on a background thread and hands every update to the UI
/// thread; the UI thread never waits on the daemon.
fn follow_daemon(
    window: slint::Weak<AppWindow>,
    tray: slint::Weak<Tray>,
    daemon: DaemonConfig,
    idle_threshold: Duration,
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
                    let layout_error = save_layout(&mut state);
                    state.rows = sessions.clone();
                    let sections = with_needs_you(state.store.layout().arrange(&state.rows));
                    guard = Some(state);
                    Some((sections, layout_error))
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
                    if let Some((sections, layout_error)) = arranged {
                        window.set_groups(ModelRc::new(VecModel::from(group_data(sections))));
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
    }));
    window.set_layout_error(layout_error.clone().unwrap_or_default().into());

    let config_path = Config::path().map_err(|err| err.to_string());
    let attach_context = match Config::load() {
        Ok(config) => {
            let context = AttachContext {
                config_path,
                settings: Ok(config.attach()),
            };
            follow_daemon(
                window.as_weak(),
                tray.as_weak(),
                config.daemon,
                config.tray.idle_threshold(),
                layout.clone(),
            );
            context
        }
        Err(err) => {
            error!(error = %err, "load companion.toml");
            window.set_error(err.to_string().into());
            AttachContext {
                config_path,
                settings: Err(err.to_string()),
            }
        }
    };
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

    window.on_payload(|row| {
        let mut transfer = slint::DataTransfer::default();
        transfer.set_user_data(Rc::new(SessionKey {
            host: row.host.into(),
            session_id: row.id.into(),
        }));
        transfer
    });
    {
        let drops = spawn_drop_worker(window.as_weak(), layout.clone());
        window.on_drop(move |transfer, group, anchor_host, anchor_id, after| {
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
            // The dragged row is destroyed by the model change, which arrives
            // from the worker thread after this callback has returned.
            let request = DropRequest {
                key,
                group: group.into(),
                anchor,
            };
            if drops.send(request).is_err() {
                error!("the drop worker has stopped");
            }
        });
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
    tray.on_quit(|| {
        if let Err(err) = slint::quit_event_loop() {
            error!(error = %err, "quit the event loop");
        }
    });

    window.show()?;
    slint::run_event_loop_until_quit()?;
    Ok(())
}
