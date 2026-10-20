//! The in-app confirmation modal for stopping or removing a session: the
//! wording per action, the daemon call, and the outcome the modal shows.

use std::sync::{Arc, Mutex, PoisonError};

use pohunek_companion_core::{DaemonConfig, LifecycleAction, SessionKey, run_lifecycle_action};
use slint::ComponentHandle;
use tracing::{error, info, warn};

use crate::{AppWindow, SharedLayout, lock, remote_host_name};

/// Modal wording for one action.
struct Wording {
    heading: &'static str,
    confirm_label: &'static str,
    consequence: &'static str,
}

fn wording(action: LifecycleAction) -> Wording {
    match action {
        LifecycleAction::Stop => Wording {
            heading: "Stop",
            confirm_label: "Stop",
            consequence: "The agent ends; the session stays in the list as ended.",
        },
        LifecycleAction::Remove => Wording {
            heading: "Remove",
            confirm_label: "Remove",
            consequence: "The session is deleted from the daemon (stopped first if live), together with any worktree checkout Pohunek created for it.",
        },
        LifecycleAction::RemoveAcceptingUnconfirmed => Wording {
            heading: "Remove anyway",
            confirm_label: "Remove anyway",
            consequence: "The daemon cannot confirm that all of its processes ended. They are not signalled and may keep running unsupervised after the session, its logs and its worktree are deleted.",
        },
    }
}

/// Sentence added to the consequence for a session with a runtime problem.
const RUNTIME_PROBLEM_NOTE: &str = " The session's runtime is in a conflicting or incompatible state, so the daemon may refuse; its message is shown here.";

/// Where a remote removal deletes the worktree checkout.
const REMOTE_WORKTREE_NOTE: &str = " The worktree checkout is deleted on that host.";

fn show_wording(window: &AppWindow, action: LifecycleAction, runtime_problem: bool, remote: bool) {
    let wording = wording(action);
    window.set_confirm_heading(wording.heading.into());
    window.set_confirm_label(wording.confirm_label.into());
    let note = if runtime_problem && action != LifecycleAction::RemoveAcceptingUnconfirmed {
        RUNTIME_PROBLEM_NOTE
    } else {
        ""
    };
    let location = if remote && action != LifecycleAction::Stop {
        REMOTE_WORKTREE_NOTE
    } else {
        ""
    };
    window.set_confirm_consequence(format!("{}{location}{note}", wording.consequence).into());
    window.set_confirm_selected(false);
}

/// What the modal shows after an attempt: a failure, or a note on a
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
    host_name: Option<&str>,
    session_id: &str,
    action: LifecycleAction,
) -> ActionReport {
    let failure = |error: String| ActionReport {
        error,
        ..Default::default()
    };
    let label = action_label(action, session_id, host_name);
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
    match runtime.block_on(run_lifecycle_action(daemon, host, session_id, action)) {
        Err(err) => ActionReport {
            error: format!("Cannot {label}: {err}"),
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
                    notice: format!(
                        "Removed {}, but {}",
                        subject(session_id, host_name),
                        parts.join("; ")
                    ),
                    ..Default::default()
                }
            }
        }
    }
}

/// The session id, followed by its host for a remote session.
fn subject(session_id: &str, host_name: Option<&str>) -> String {
    match host_name {
        Some(name) => format!("{session_id} on {name}"),
        None => session_id.to_owned(),
    }
}

fn action_label(action: LifecycleAction, session_id: &str, host_name: Option<&str>) -> String {
    let verb = match action {
        LifecycleAction::Stop => "stop",
        LifecycleAction::Remove | LifecycleAction::RemoveAcceptingUnconfirmed => "remove",
    };
    format!("{verb} {}", subject(session_id, host_name))
}

fn show_report(window: &AppWindow, report: ActionReport, remote: bool) {
    window.set_confirm_busy(false);
    if report.offer_unconfirmed {
        show_wording(
            window,
            LifecycleAction::RemoveAcceptingUnconfirmed,
            false,
            remote,
        );
        window.set_confirm_error(report.error.into());
    } else if !report.error.is_empty() {
        window.set_confirm_selected(false);
        window.set_confirm_error(report.error.into());
    } else if !report.notice.is_empty() {
        window.set_confirm_notice(report.notice.into());
    } else {
        window.set_confirm_open(false);
    }
}

/// The session the open modal acts on and the call its confirmation sends.
struct Pending {
    host: String,
    /// The host's display name for a remote session.
    host_name: Option<String>,
    session_id: String,
    /// Becomes `RemoveAcceptingUnconfirmed` only after the daemon refused a
    /// safe removal and the owner saw the risk.
    action: LifecycleAction,
}

/// Wires the sidebar's stop and remove requests to the modal. `daemon` is the
/// loaded daemon configuration, or why it could not be loaded.
pub fn install(window: &AppWindow, daemon: Result<DaemonConfig, String>, layout: SharedLayout) {
    let pending: Arc<Mutex<Option<Pending>>> = Arc::new(Mutex::new(None));

    let weak = window.as_weak();
    let slot = pending.clone();
    window.on_request_action(move |kind, host, id, name| {
        let Some(window) = weak.upgrade() else { return };
        // A running request keeps the modal that sent it.
        if window.get_confirm_busy() {
            return;
        }
        let action = match kind.as_str() {
            "stop" => LifecycleAction::Stop,
            "remove" => LifecycleAction::Remove,
            other => {
                error!(kind = other, "unknown session action requested");
                return;
            }
        };
        let key = SessionKey {
            host: host.to_string(),
            session_id: id.to_string(),
        };
        let runtime_problem = lock(&layout)
            .rows
            .iter()
            .any(|row| row.key == key && !row.runtime_detail.is_empty());
        let host_name = remote_host_name(&layout, &key);
        info!(host = %host, session = %id, action = action.verb(), "confirmation requested from the sidebar");
        *slot.lock().unwrap_or_else(PoisonError::into_inner) = Some(Pending {
            host: host.into(),
            host_name: host_name.clone(),
            session_id: id.into(),
            action,
        });
        window.set_confirm_name(subject(name.as_str(), host_name.as_deref()).into());
        window.set_confirm_error("".into());
        window.set_confirm_notice("".into());
        show_wording(&window, action, runtime_problem, host_name.is_some());
        window.set_confirm_open(true);
    });

    let weak = window.as_weak();
    let slot = pending.clone();
    window.on_confirm_accepted(move || {
        let Some(window) = weak.upgrade() else { return };
        if window.get_confirm_busy() {
            return;
        }
        let Some((host, host_name, session_id, action)) = slot
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .map(|p| (
                p.host.clone(),
                p.host_name.clone(),
                p.session_id.clone(),
                p.action,
            ))
        else {
            return;
        };
        info!(host = %host, session = %session_id, action = action.verb(), "session action confirmed in the modal");
        window.set_confirm_busy(true);
        window.set_confirm_error("".into());
        let weak = window.as_weak();
        let slot = slot.clone();
        let daemon = daemon.clone();
        std::thread::spawn(move || {
            let report = lifecycle_report(&daemon, &host, host_name.as_deref(), &session_id, action);
            if !report.error.is_empty() {
                warn!(session = %session_id, action = action.verb(), error = %report.error, "session action failed");
            }
            if report.offer_unconfirmed
                && let Some(pending) = slot.lock().unwrap_or_else(PoisonError::into_inner).as_mut()
            {
                pending.action = LifecycleAction::RemoveAcceptingUnconfirmed;
            }
            let applied = weak.upgrade_in_event_loop(move |window| show_report(&window, report, host_name.is_some()));
            if let Err(err) = applied {
                error!(error = %err, "deliver session action outcome to the UI");
            }
        });
    });

    let weak = window.as_weak();
    window.on_confirm_cancelled(move || {
        let Some(window) = weak.upgrade() else { return };
        // A running request is not abandoned: its outcome is shown.
        if !window.get_confirm_busy() {
            info!("confirmation closed without a further action");
            pending
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take();
            window.set_confirm_open(false);
        }
    });
}
