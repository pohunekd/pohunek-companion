//! Resuming an ended session from the sidebar: `session.resume`, then the
//! attach once the session runs again. A profile change the owner has not
//! approved yet goes through the confirmation modal.

use pohunek_companion_core::{
    DaemonConfig, LOCAL_HOST, LifecycleAction, Resumed, SessionKey, run_lifecycle_action,
};
use tracing::{error, info, warn};

use crate::confirm::ConfirmHandle;
use crate::{AppWindow, AttachContext, SharedLayout, attach_outcome, remote_host_name};

/// What one resume attempt left for the window to show.
pub enum Step {
    /// Resumed and attached, or resumed with the attach left to the owner;
    /// the text is the status line (empty when the attach launched).
    Done(String),
    /// The daemon refused only because the agent profile changed; local hosts
    /// can retry after the owner approves.
    NeedsProfileApproval,
    Failed(String),
}

/// Everything a resume needs from startup.
#[derive(Clone)]
pub struct ResumeContext {
    pub daemon: Result<DaemonConfig, String>,
    pub attach: AttachContext,
}

fn subject(session_id: &str, host_name: Option<&str>) -> String {
    match host_name {
        Some(name) => format!("{session_id} on {name}"),
        None => session_id.to_owned(),
    }
}

/// Resumes `key` and attaches to it. Blocks; call it off the UI thread.
/// `accept_profile_change` is the owner's approval to relaunch under the
/// current agent profile.
pub fn resume_and_attach(
    context: &ResumeContext,
    key: &SessionKey,
    host_name: Option<&str>,
    accept_profile_change: bool,
) -> Step {
    let label = subject(&key.session_id, host_name);
    let daemon = match &context.daemon {
        Ok(daemon) => daemon,
        Err(message) => return Step::Failed(message.clone()),
    };
    let action = if accept_profile_change {
        LifecycleAction::ResumeAcceptingProfileChange
    } else {
        LifecycleAction::Resume
    };
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => return Step::Failed(format!("cannot start the background runtime: {err}")),
    };
    match runtime.block_on(run_lifecycle_action(
        daemon,
        &key.host,
        &key.session_id,
        action,
    )) {
        Err(err)
            if err.is_profile_changed() && key.host == LOCAL_HOST && !accept_profile_change =>
        {
            Step::NeedsProfileApproval
        }
        Err(err) => Step::Failed(format!("Cannot resume {label}: {err}")),
        Ok(outcome) if outcome.resumed == Some(Resumed::Running) => {
            match attach_outcome(&context.attach, key, "") {
                Ok(()) => Step::Done(String::new()),
                Err(message) => Step::Failed(message),
            }
        }
        Ok(_) => Step::Done(format!("Resumed {label}; attach once it is running.")),
    }
}

/// Shows a finished attempt in the status line; a profile change asks the
/// owner first.
fn show_step(
    window: &AppWindow,
    confirm: &ConfirmHandle,
    key: &SessionKey,
    name: &str,
    host_name: Option<String>,
    step: Step,
) {
    match step {
        Step::Done(text) | Step::Failed(text) => window.set_attach_error(text.into()),
        Step::NeedsProfileApproval => {
            confirm.offer_resume_with_profile_change(window, key, name, host_name)
        }
    }
}

/// Resumes on a background thread and reports into the window.
pub fn start_resume(
    window: slint::Weak<AppWindow>,
    context: ResumeContext,
    confirm: ConfirmHandle,
    layout: &SharedLayout,
    key: SessionKey,
    name: String,
) {
    let host_name = remote_host_name(layout, &key);
    info!(host = %key.host, session = %key.session_id, "resume requested");
    if let Some(window) = window.upgrade() {
        window.set_attach_error("".into());
    }
    std::thread::spawn(move || {
        let step = resume_and_attach(&context, &key, host_name.as_deref(), false);
        match &step {
            Step::Failed(message) => {
                warn!(session = %key.session_id, error = %message, "resume failed")
            }
            Step::NeedsProfileApproval => {
                info!(session = %key.session_id, "resume needs approval for a changed agent profile")
            }
            Step::Done(_) => {}
        }
        let applied = window.upgrade_in_event_loop(move |window| {
            show_step(&window, &confirm, &key, &name, host_name, step);
        });
        if let Err(err) = applied {
            error!(error = %err, "deliver resume outcome to the UI");
        }
    });
}
