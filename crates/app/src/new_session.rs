//! The `new-session` subcommand: a standalone dialog that creates one session
//! through the daemon. It runs as its own process, without a tray icon or the
//! session stream.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use pohunek_companion_core::{
    Choices, Config, CreateError, DaemonConfig, NewSessionConfig, NewSessionForm, SessionNewParams,
    build_params, create_session, load_choices,
};
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};
use tracing::{error, info, warn};

use crate::NewSessionWindow;

/// Wayland app id of the dialog; distinct from the sidebar's so compositor
/// rules can float it separately.
const APP_ID: &str = "pohunek-companion-new-session";
const PROJECT_FLAG: &str = "--project";

/// Parsed `new-session` arguments.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Args {
    /// Project id to preselect.
    pub project: Option<String>,
}

pub fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Args, String> {
    let mut parsed = Args::default();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            PROJECT_FLAG => {
                let id = args
                    .next()
                    .filter(|id| !id.is_empty())
                    .ok_or_else(|| format!("{PROJECT_FLAG} needs a project id"))?;
                parsed.project = Some(id);
            }
            other => return Err(format!("unknown argument `{other}`")),
        }
    }
    Ok(parsed)
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn block_on<F: std::future::Future>(future: F) -> Result<F::Output, std::io::Error> {
    Ok(tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(future))
}

fn show_choices(window: &NewSessionWindow, choices: &Choices, preselect: Option<&str>) {
    window.set_project_names(ModelRc::new(VecModel::from(
        choices
            .projects
            .iter()
            .map(|project| SharedString::from(project.label.as_str()))
            .collect::<Vec<_>>(),
    )));
    window.set_agent_names(ModelRc::new(VecModel::from(
        choices
            .agents
            .iter()
            .map(|agent| SharedString::from(agent.as_str()))
            .collect::<Vec<_>>(),
    )));
    // An unknown id stays unselected.
    let project = preselect
        .and_then(|id| choices.projects.iter().position(|p| p.id == id))
        .and_then(|index| i32::try_from(index).ok())
        .unwrap_or(-1);
    window.set_project_index(project);
    window.set_agent_index(if choices.agents.is_empty() { -1 } else { 0 });
}

fn load_in_background(
    window: slint::Weak<NewSessionWindow>,
    daemon: DaemonConfig,
    preselect: Option<String>,
    choices: Arc<Mutex<Choices>>,
) {
    std::thread::spawn(move || {
        let outcome = block_on(load_choices(&daemon))
            .map_err(|err| format!("cannot start the background runtime: {err}"))
            .and_then(|loaded| loaded.map_err(|err| err.to_string()));
        if let Err(message) = &outcome {
            error!(error = %message, "load new-session choices");
        }
        let delivered = window.upgrade_in_event_loop(move |window| {
            window.set_loading(false);
            match outcome {
                Ok(loaded) => {
                    show_choices(&window, &loaded, preselect.as_deref());
                    if loaded.projects.is_empty() {
                        window.set_error("The daemon has no projects.".into());
                    } else if loaded.agents.is_empty() {
                        window.set_error("The daemon reports no launchable agent.".into());
                    }
                    *lock(&choices) = loaded;
                }
                Err(message) => window.set_error(message.into()),
            }
        });
        if let Err(err) = delivered {
            error!(error = %err, "deliver the new-session choices to the UI");
        }
    });
}

fn submit_in_background(
    window: slint::Weak<NewSessionWindow>,
    daemon: DaemonConfig,
    config: NewSessionConfig,
    params: SessionNewParams,
) {
    std::thread::spawn(move || {
        let outcome = match block_on(create_session(&daemon, &config, params)) {
            Ok(outcome) => outcome,
            Err(err) => Err(CreateError::NotSent(format!(
                "cannot start the background runtime: {err}"
            ))),
        };
        match &outcome {
            Ok(id) => info!(session = %id, "create succeeded"),
            Err(err) => warn!(code = err.code(), "create failed"),
        }
        let delivered = window.upgrade_in_event_loop(move |window| match outcome {
            Ok(_) => {
                if let Err(err) = window.hide() {
                    error!(error = %err, "hide the new-session window");
                }
            }
            Err(err) => {
                // An unknown outcome keeps the window busy: Create stays
                // disabled so the request cannot be repeated from here.
                window.set_busy(matches!(err, CreateError::Unconfirmed));
                window.set_error(err.to_string().into());
            }
        });
        if let Err(err) = delivered {
            error!(error = %err, "deliver the create outcome to the UI");
        }
    });
}

pub fn run(args: Args) -> Result<(), slint::PlatformError> {
    info!("new-session dialog started");
    let window = NewSessionWindow::new()?;
    slint::set_xdg_app_id(APP_ID)?;

    let choices = Arc::new(Mutex::new(Choices::default()));
    match Config::load() {
        Ok(config) => {
            load_in_background(
                window.as_weak(),
                config.daemon.clone(),
                args.project,
                choices.clone(),
            );
            let weak = window.as_weak();
            window.on_submit(move || {
                let Some(window) = weak.upgrade() else { return };
                let project = usize::try_from(window.get_project_index())
                    .ok()
                    .and_then(|index| lock(&choices).projects.get(index).map(|p| p.id.clone()));
                let agent = usize::try_from(window.get_agent_index())
                    .ok()
                    .and_then(|index| lock(&choices).agents.get(index).cloned());
                let form = NewSessionForm {
                    project,
                    agent,
                    name: window.get_session_name().into(),
                    prompt: window.get_prompt().into(),
                };
                let agents = lock(&choices).agents.clone();
                match build_params(&form, &agents, &config.new_session) {
                    Ok(params) => {
                        info!(agent = %params.agent, "create requested");
                        window.set_error("".into());
                        window.set_busy(true);
                        submit_in_background(
                            weak.clone(),
                            config.daemon.clone(),
                            config.new_session.clone(),
                            params,
                        );
                    }
                    Err(err) => window.set_error(err.to_string().into()),
                }
            });
        }
        Err(err) => {
            error!(error = %err, "load companion.toml");
            window.set_loading(false);
            window.set_error(err.to_string().into());
        }
    }
    let weak = window.as_weak();
    window.on_cancel(move || {
        if let Some(window) = weak.upgrade()
            && let Err(err) = window.hide()
        {
            error!(error = %err, "hide the new-session window");
        }
    });
    window.run()
}
