use std::cell::RefCell;
use std::path::PathBuf;
use std::time::Duration;

mod logging;
mod tray;

use pohunek_companion_core::{
    AttachConfig, AttachError, Config, DaemonConfig, LiveUpdate, OwnerState, SessionKey,
    SessionRow, TraySummary, attach, clip_display, run_live,
};
use slint::{ComponentHandle, Model, ModelRc, VecModel};
use tracing::{error, info};

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

/// Brings `model` to `rows` touching only what differs: unchanged rows are not
/// rewritten, removed ones are dropped, new ones inserted in place.
fn sync_rows(model: &VecModel<SessionRowData>, rows: Vec<SessionRowData>) {
    let mut i = model.row_count();
    while i > 0 {
        i -= 1;
        if let Some(row) = model.row_data(i)
            && !rows.iter().any(|new| new.id == row.id)
        {
            model.remove(i);
        }
    }
    for (i, new) in rows.iter().enumerate() {
        match model.row_data(i) {
            Some(current) if current.id == new.id => {
                if current != *new {
                    model.set_row_data(i, new.clone());
                }
            }
            _ => {
                let later = (i + 1..model.row_count())
                    .find(|&j| model.row_data(j).is_some_and(|row| row.id == new.id));
                if let Some(j) = later {
                    model.remove(j);
                }
                model.insert(i, new.clone());
            }
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
            let tray = tray.clone();
            let applied = window.upgrade_in_event_loop(move |window| match update {
                LiveUpdate::Connected => {
                    window.set_error("".into());
                    window.set_status("".into());
                }
                LiveUpdate::Sessions(sessions) => {
                    if let (Some(tray), Some(summary)) = (tray.upgrade(), summary) {
                        tray::apply(&tray, &summary);
                    }
                    let rows = session_rows(sessions);
                    let model = window.get_sessions();
                    match model.as_any().downcast_ref::<VecModel<SessionRowData>>() {
                        Some(model) => sync_rows(model, rows),
                        None => window.set_sessions(ModelRc::new(VecModel::from(rows))),
                    }
                }
                LiveUpdate::Reconnecting { attempt, reason } => {
                    window.set_status(format!("Reconnecting (attempt {attempt}): {reason}").into());
                }
            });
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
/// window) and shows a failure in the window.
fn start_attach(
    window: slint::Weak<AppWindow>,
    context: AttachContext,
    key: SessionKey,
    refusal: String,
) {
    std::thread::spawn(move || {
        let outcome = attach_outcome(&context, &key, &refusal);
        let applied = window.upgrade_in_event_loop(move |window| {
            window.set_attach_error(outcome.err().unwrap_or_default().into());
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
            start_attach(weak.clone(), context.clone(), key, refusal.into());
        });
    }
    {
        let weak = window.as_weak();
        window.on_attach(move |host, id, refusal| {
            let key = SessionKey {
                host: host.into(),
                session_id: id.into(),
            };
            start_attach(weak.clone(), attach_context.clone(), key, refusal.into());
        });
    }

    tray::apply(&tray, &TraySummary::from_rows(&[]));
    tray.set_empty_label("No sessions need you".into());

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
