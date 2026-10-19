use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

mod logging;

use pohunek_companion_core::{
    AttachConfig, AttachError, Config, DaemonConfig, LiveUpdate, SessionKey, SessionRow, attach,
    clip_display, run_live,
};
use slint::{ComponentHandle, Image, Model, ModelRc, Rgba8Pixel, SharedPixelBuffer, VecModel};
use tracing::{error, info};

slint::include_modules!();

// Spike-only tunables; the real app reads them from companion.toml.
const APP_ID: &str = "pohunek-companion";
const TRAY_TICK: Duration = Duration::from_secs(2);
const TRAY_ICON_SIZE: u32 = 32;
const TRAY_STATES: i32 = 4;
/// Longest badge link or role text shown in a row.
const BADGE_MAX_CHARS: usize = 32;

fn tray_icon(count: i32) -> Image {
    let color = match count {
        0 => [0x6c, 0x75, 0x7d],
        1 => [0xf0, 0xa0, 0x30],
        _ => [0xe0, 0x40, 0x40],
    };
    let size = TRAY_ICON_SIZE;
    let mut buffer = SharedPixelBuffer::<Rgba8Pixel>::new(size, size);
    let center = (size as f32 - 1.0) / 2.0;
    let radius = size as f32 / 2.0 - 2.0;
    for (i, pixel) in buffer.make_mut_slice().iter_mut().enumerate() {
        let (x, y) = ((i as u32 % size) as f32, (i as u32 / size) as f32);
        let inside = (x - center).powi(2) + (y - center).powi(2) <= radius.powi(2);
        // Extra white bars encode the count, so the icon changes shape as well as colour.
        let bar = count > 0
            && (size / 4..size * 3 / 4).contains(&(y as u32))
            && (x as u32 * (2 * count as u32 + 1) / size) % 2 == 1;
        *pixel = match (inside, bar) {
            (true, true) => Rgba8Pixel::new(255, 255, 255, 255),
            (true, false) => Rgba8Pixel::new(color[0], color[1], color[2], 255),
            _ => Rgba8Pixel::new(0, 0, 0, 0),
        };
    }
    Image::from_rgba8(buffer)
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
fn follow_daemon(window: slint::Weak<AppWindow>, daemon: DaemonConfig) {
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
        runtime.block_on(run_live(&daemon, |update| {
            let applied = window.upgrade_in_event_loop(move |window| match update {
                LiveUpdate::Connected => {
                    window.set_error("".into());
                    window.set_status("".into());
                }
                LiveUpdate::Sessions(sessions) => {
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
            follow_daemon(window.as_weak(), config.daemon);
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
        window.on_attach(move |host, id, refusal| {
            let key = SessionKey {
                host: host.into(),
                session_id: id.into(),
            };
            start_attach(weak.clone(), attach_context.clone(), key, refusal.into());
        });
    }

    let count = Rc::new(RefCell::new(0i32));
    let refresh_tray = {
        let tray = tray.as_weak();
        move |count: i32| {
            let Some(tray) = tray.upgrade() else {
                return;
            };
            tray.set_tray_icon(tray_icon(count));
            tray.set_tray_tooltip(format!("{count} sessions need you").into());
            tray.set_needs_label(format!("Needs you: {count}").into());
            tray.set_ticks_label("Bump counter".into());
        }
    };
    refresh_tray(0);

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
        let count = count.clone();
        let refresh = refresh_tray.clone();
        tray.on_bump(move || {
            let mut count = count.borrow_mut();
            *count = (*count + 1) % TRAY_STATES;
            refresh(*count);
        });
    }
    tray.on_quit(|| {
        if let Err(err) = slint::quit_event_loop() {
            error!(error = %err, "quit the event loop");
        }
    });

    let timer = slint::Timer::default();
    {
        let count = count.clone();
        timer.start(slint::TimerMode::Repeated, TRAY_TICK, move || {
            let mut count = count.borrow_mut();
            *count = (*count + 1) % TRAY_STATES;
            refresh_tray(*count);
        });
    }

    window.show()?;
    slint::run_event_loop_until_quit()?;
    Ok(())
}
