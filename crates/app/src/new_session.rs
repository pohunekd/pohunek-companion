//! The `new-session` subcommand: a standalone dialog that creates one session
//! through the daemon. It runs as its own process, without a tray icon or the
//! session stream.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use pohunek_companion_core::{
    Choices, Config, CreateError, DaemonConfig, Generation, NewSessionConfig, NewSessionForm,
    Recipe, SessionNewParams, build_params, create_session, load_choices, load_recipe,
    load_templates,
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

/// Label of the chip that means "no template".
const BLANK_LABEL: &str = "Blank";

#[derive(Default)]
struct TemplateState {
    /// Template names of the selected project, without Blank.
    names: Vec<String>,
    /// The applied recipe of the chosen template; `None` for Blank.
    recipe: Option<Recipe>,
    loading: bool,
}

/// Template loading for the selected project. Every project or template
/// change advances `generation`, and a response that no longer matches it is
/// dropped.
#[derive(Clone)]
struct Templates {
    daemon: DaemonConfig,
    choices: Arc<Mutex<Choices>>,
    generation: Arc<Generation>,
    state: Arc<Mutex<TemplateState>>,
}

fn selected_project(window: &NewSessionWindow, choices: &Mutex<Choices>) -> Option<String> {
    usize::try_from(window.get_project_index())
        .ok()
        .and_then(|index| lock(choices).projects.get(index).map(|p| p.id.clone()))
}

fn show_template_names(window: &NewSessionWindow, names: &[String]) {
    let labels = std::iter::once(BLANK_LABEL)
        .chain(names.iter().map(String::as_str))
        .map(SharedString::from)
        .collect::<Vec<_>>();
    window.set_template_names(ModelRc::new(VecModel::from(labels)));
}

impl Templates {
    fn new(daemon: DaemonConfig, choices: Arc<Mutex<Choices>>) -> Self {
        Self {
            daemon,
            choices,
            generation: Arc::new(Generation::default()),
            state: Arc::new(Mutex::new(TemplateState::default())),
        }
    }

    /// Resets to Blank and reads the templates of the selected project.
    fn project_changed(&self, window: &NewSessionWindow) {
        let token = self.generation.advance();
        *lock(&self.state) = TemplateState::default();
        show_template_names(window, &[]);
        window.set_template_index(0);
        window.set_template_loading(false);
        let Some(project) = selected_project(window, &self.choices) else {
            return;
        };
        let this = self.clone();
        let weak = window.as_weak();
        std::thread::spawn(move || {
            let outcome = block_on(load_templates(&this.daemon, &project))
                .map_err(|err| format!("cannot start the background runtime: {err}"))
                .and_then(|loaded| loaded.map_err(|err| err.to_string()));
            if let Err(message) = &outcome {
                error!(error = %message, "load project templates");
            }
            let delivered = weak.upgrade_in_event_loop(move |window| {
                if !this.generation.is_current(token) {
                    return;
                }
                match outcome {
                    Ok(names) => {
                        show_template_names(&window, &names);
                        lock(&this.state).names = names;
                    }
                    Err(message) => window.set_error(message.into()),
                }
            });
            if let Err(err) = delivered {
                error!(error = %err, "deliver the project templates to the UI");
            }
        });
    }

    /// Applies the chip at `index` (0 is Blank): reads the recipe and fills
    /// the prompt and agent from it.
    fn chosen(&self, window: &NewSessionWindow, index: i32) {
        let token = self.generation.advance();
        {
            let mut state = lock(&self.state);
            state.recipe = None;
            state.loading = false;
        }
        window.set_template_loading(false);
        window.set_error("".into());
        let name = usize::try_from(index)
            .ok()
            .and_then(|index| index.checked_sub(1))
            .and_then(|index| lock(&self.state).names.get(index).cloned());
        let (Some(name), Some(project)) = (name, selected_project(window, &self.choices)) else {
            return;
        };
        info!(template = %name, "template chosen");
        lock(&self.state).loading = true;
        window.set_template_loading(true);
        let this = self.clone();
        let weak = window.as_weak();
        std::thread::spawn(move || {
            let outcome = block_on(load_recipe(&this.daemon, &project, &name))
                .map_err(|err| format!("cannot start the background runtime: {err}"))
                .and_then(|loaded| loaded.map_err(|err| err.to_string()));
            if let Err(message) = &outcome {
                error!(template = %name, error = %message, "load template");
            }
            let delivered = weak.upgrade_in_event_loop(move |window| {
                if !this.generation.is_current(token) {
                    return;
                }
                lock(&this.state).loading = false;
                window.set_template_loading(false);
                match outcome {
                    Ok(recipe) => this.apply(&window, recipe),
                    Err(message) => {
                        window.set_template_index(0);
                        window.set_error(message.into());
                    }
                }
            });
            if let Err(err) = delivered {
                error!(error = %err, "deliver the template to the UI");
            }
        });
    }

    fn apply(&self, window: &NewSessionWindow, recipe: Recipe) {
        let agent = lock(&self.choices)
            .agents
            .iter()
            .position(|agent| *agent == recipe.agent)
            .and_then(|index| i32::try_from(index).ok());
        let Some(agent) = agent else {
            window.set_template_index(0);
            window.set_error(
                format!(
                    "The template's agent `{}` is not launchable on this host.",
                    recipe.agent
                )
                .into(),
            );
            return;
        };
        window.set_agent_index(agent);
        window.set_prompt(recipe.prompt.as_str().into());
        lock(&self.state).recipe = Some(recipe);
    }
}

fn load_in_background(
    window: slint::Weak<NewSessionWindow>,
    daemon: DaemonConfig,
    preselect: Option<String>,
    choices: Arc<Mutex<Choices>>,
    templates: Templates,
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
                    templates.project_changed(&window);
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
                // An unknown outcome or an unconfirmed prompt keeps the window
                // busy: Create stays disabled so the request is not repeated.
                window.set_busy(matches!(
                    err,
                    CreateError::Unconfirmed | CreateError::PromptUnconfirmed
                ));
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
    show_template_names(&window, &[]);
    match Config::load() {
        Ok(config) => {
            let templates = Templates::new(config.daemon.clone(), choices.clone());
            load_in_background(
                window.as_weak(),
                config.daemon.clone(),
                args.project,
                choices.clone(),
                templates.clone(),
            );
            let on_project = templates.clone();
            let weak = window.as_weak();
            window.on_project_changed(move |_| {
                if let Some(window) = weak.upgrade() {
                    on_project.project_changed(&window);
                }
            });
            let on_template = templates.clone();
            let weak = window.as_weak();
            window.on_template_chosen(move |index| {
                if let Some(window) = weak.upgrade() {
                    on_template.chosen(&window, index);
                }
            });
            let weak = window.as_weak();
            let submit_templates = templates.clone();
            window.on_submit(move || {
                let templates = &submit_templates;
                let Some(window) = weak.upgrade() else { return };
                let project = selected_project(&window, &choices);
                let agent = usize::try_from(window.get_agent_index())
                    .ok()
                    .and_then(|index| lock(&choices).agents.get(index).cloned());
                let (recipe, template_loading) = {
                    let state = lock(&templates.state);
                    (state.recipe.clone(), state.loading)
                };
                let form = NewSessionForm {
                    project,
                    agent,
                    name: window.get_session_name().into(),
                    prompt: window.get_prompt().into(),
                    recipe,
                    template_loading,
                    branch: window.get_branch().into(),
                    base_branch: window.get_base_branch().into(),
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
