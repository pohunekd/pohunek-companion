//! The `confirm` subcommand: a standalone dialog that confirms one stop or
//! remove of a session, performs it through the daemon, and shows the
//! outcome. It runs as its own process, without a tray icon or the session
//! stream.

use std::sync::{Arc, Mutex, PoisonError};

use pohunek_companion_core::{
    Config, DaemonConfig, LOCAL_HOST, LifecycleAction, run_lifecycle_action,
};
use slint::ComponentHandle;
use tracing::{error, info, warn};

use crate::ConfirmWindow;

/// Wayland app id of the dialog; distinct from the sidebar's so compositor
/// rules can float it separately.
const APP_ID: &str = "pohunek-companion-confirm";
const SESSION_FLAG: &str = "--session";
const NAME_FLAG: &str = "--name";
const HOST_FLAG: &str = "--host";

/// Usage line shared by the argument errors.
pub const USAGE: &str = "confirm <stop|remove> --session ID [--name NAME] [--host HOST]";

/// Parsed `confirm` arguments.
#[derive(Debug, PartialEq, Eq)]
pub struct Args {
    pub action: LifecycleAction,
    pub host: String,
    pub session_id: String,
    /// Name shown in the dialog; the session id when none is given.
    pub name: String,
}

pub fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Args, String> {
    let mut args = args.into_iter();
    let action = match args.next().as_deref() {
        Some("stop") => LifecycleAction::Stop,
        Some("remove") => LifecycleAction::Remove,
        Some(other) => return Err(format!("unknown action `{other}`")),
        None => return Err("an action (stop or remove) is required".to_owned()),
    };
    let mut host = LOCAL_HOST.to_owned();
    let mut session_id = None;
    let mut name = None;
    while let Some(arg) = args.next() {
        let slot = match arg.as_str() {
            SESSION_FLAG => &mut session_id,
            NAME_FLAG => &mut name,
            HOST_FLAG => {
                host = args
                    .next()
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| format!("{HOST_FLAG} needs a host"))?;
                continue;
            }
            other => return Err(format!("unknown argument `{other}`")),
        };
        *slot = Some(
            args.next()
                .filter(|value| !value.is_empty())
                .ok_or_else(|| format!("{arg} needs a value"))?,
        );
    }
    let session_id = session_id.ok_or_else(|| format!("{SESSION_FLAG} is required"))?;
    Ok(Args {
        action,
        host,
        name: name.unwrap_or_else(|| session_id.clone()),
        session_id,
    })
}

/// Dialog wording for one action.
struct Wording {
    kind: &'static str,
    heading: &'static str,
    confirm_label: &'static str,
    consequence: &'static str,
}

fn wording(action: LifecycleAction) -> Wording {
    match action {
        LifecycleAction::Stop => Wording {
            kind: "stop",
            heading: "Stop",
            confirm_label: "Stop",
            consequence: "The agent ends; the session stays in the list as ended.",
        },
        LifecycleAction::Remove => Wording {
            kind: "remove",
            heading: "Remove",
            confirm_label: "Remove",
            consequence: "The session is deleted from the daemon (stopped first if live), together with any worktree checkout Pohunek created for it.",
        },
        LifecycleAction::RemoveAcceptingUnconfirmed => Wording {
            kind: "remove-anyway",
            heading: "Remove anyway",
            confirm_label: "Remove anyway",
            consequence: "The daemon cannot confirm that all of its processes ended. They are not signalled and may keep running unsupervised after the session, its logs and its worktree are deleted.",
        },
    }
}

fn show_wording(window: &ConfirmWindow, action: LifecycleAction) {
    let wording = wording(action);
    window.set_kind(wording.kind.into());
    window.set_heading(wording.heading.into());
    window.set_confirm_label(wording.confirm_label.into());
    window.set_consequence(wording.consequence.into());
    window.set_confirm_selected(false);
}

/// What the dialog shows after an attempt: a failure, or a note on a
/// successful removal that left something for the owner to handle.
#[derive(Default)]
struct ActionReport {
    error: String,
    notice: String,
    /// The refusal was only an unconfirmed runtime sweep: offer "Remove anyway".
    offer_unconfirmed: bool,
}

fn lifecycle_report(
    daemon: &Result<DaemonConfig, String>,
    host: &str,
    session_id: &str,
    action: LifecycleAction,
) -> ActionReport {
    let failure = |error: String| ActionReport {
        error,
        ..Default::default()
    };
    if host != LOCAL_HOST {
        return failure(format!(
            "Cannot {}: only the local daemon is supported",
            action_label(action, session_id)
        ));
    }
    let daemon = match daemon {
        Ok(daemon) => daemon,
        Err(message) => return failure(message.clone()),
    };
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => return failure(format!("cannot start the background runtime: {err}")),
    };
    match runtime.block_on(run_lifecycle_action(daemon, session_id, action)) {
        Err(err) => ActionReport {
            error: format!("Cannot {}: {err}", action_label(action, session_id)),
            offer_unconfirmed: action == LifecycleAction::Remove && err.is_unconfirmed(),
            ..Default::default()
        },
        Ok(outcome) => {
            let mut parts = Vec::new();
            if outcome.worktrees_failed > 0 {
                parts.push(format!(
                    "{} worktree checkout(s) could not be deleted and need manual cleanup",
                    outcome.worktrees_failed
                ));
            }
            if !outcome.unconfirmed_processes.is_empty() {
                parts.push(format!(
                    "these processes could not be confirmed ended and were not signalled; they may still be running: {}",
                    outcome.unconfirmed_processes.join(", ")
                ));
            }
            if parts.is_empty() {
                ActionReport::default()
            } else {
                ActionReport {
                    notice: format!("Removed {session_id}, but {}", parts.join("; ")),
                    ..Default::default()
                }
            }
        }
    }
}

fn action_label(action: LifecycleAction, session_id: &str) -> String {
    let verb = match action {
        LifecycleAction::Stop => "stop",
        LifecycleAction::Remove | LifecycleAction::RemoveAcceptingUnconfirmed => "remove",
    };
    format!("{verb} {session_id}")
}

fn show_report(window: &ConfirmWindow, report: ActionReport) {
    window.set_busy(false);
    if report.offer_unconfirmed {
        show_wording(window, LifecycleAction::RemoveAcceptingUnconfirmed);
        window.set_error(report.error.into());
    } else if !report.error.is_empty() {
        window.set_confirm_selected(false);
        window.set_error(report.error.into());
    } else if !report.notice.is_empty() {
        window.set_notice(report.notice.into());
    } else {
        close(window);
    }
}

fn close(window: &ConfirmWindow) {
    if let Err(err) = window.hide() {
        error!(error = %err, "hide the confirm dialog");
    }
}

pub fn run(args: Args) -> Result<(), slint::PlatformError> {
    info!(host = %args.host, session = %args.session_id, action = args.action.verb(), "confirm dialog started");
    let window = ConfirmWindow::new()?;
    slint::set_xdg_app_id(APP_ID)?;
    window.set_name(args.name.as_str().into());
    show_wording(&window, args.action);

    let daemon = Config::load()
        .map(|config| config.daemon)
        .map_err(|err| err.to_string());
    if let Err(message) = &daemon {
        error!(error = %message, "load companion.toml");
    }

    // The action the next confirmation sends; becomes `RemoveAcceptingUnconfirmed`
    // only after the daemon refused a safe removal and the owner saw the risk.
    let current = Arc::new(Mutex::new(args.action));
    let host = args.host;
    let session_id = args.session_id;
    let weak = window.as_weak();
    window.on_confirm(move || {
        let Some(window) = weak.upgrade() else { return };
        if window.get_busy() {
            return;
        }
        let action = *current.lock().unwrap_or_else(PoisonError::into_inner);
        info!(host = %host, session = %session_id, action = action.verb(), "session action confirmed in the dialog");
        window.set_busy(true);
        window.set_error("".into());
        let weak = window.as_weak();
        let current = current.clone();
        let daemon = daemon.clone();
        let host = host.clone();
        let session_id = session_id.clone();
        std::thread::spawn(move || {
            let report = lifecycle_report(&daemon, &host, &session_id, action);
            if !report.error.is_empty() {
                warn!(session = %session_id, action = action.verb(), error = %report.error, "session action failed");
            }
            if report.offer_unconfirmed {
                *current.lock().unwrap_or_else(PoisonError::into_inner) =
                    LifecycleAction::RemoveAcceptingUnconfirmed;
            }
            let applied = weak.upgrade_in_event_loop(move |window| show_report(&window, report));
            if let Err(err) = applied {
                error!(error = %err, "deliver session action outcome to the UI");
            }
        });
    });
    let weak = window.as_weak();
    window.on_cancel(move || {
        let Some(window) = weak.upgrade() else { return };
        // A running request is not abandoned: its outcome is shown.
        if !window.get_busy() {
            info!("confirm dialog closed without a further action");
            close(&window);
        }
    });
    let weak = window.as_weak();
    window.window().on_close_requested(move || {
        let busy = weak.upgrade().is_some_and(|window| window.get_busy());
        if busy {
            slint::CloseRequestResponse::KeepWindowShown
        } else {
            slint::CloseRequestResponse::HideWindow
        }
    });
    window.run()
}
