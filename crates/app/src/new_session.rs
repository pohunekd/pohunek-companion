//! The `new-session` subcommand: a standalone dialog that creates one session
//! through the daemon. It runs as its own process, without a tray icon or the
//! session stream.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use pohunek_companion_core::{
    Activity, AttachConfig, AttachError, Choices, Config, CreateError, DaemonConfig, FormError,
    Generation, LOCAL_HOST, NewSessionConfig, NewSessionForm, Recipe, SessionKey, SessionNewParams,
    attach, build_params, create_session, load_choices, load_recipe, load_templates,
    submit_blocker,
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
            .map(|agent| SharedString::from(agent.name.as_str()))
            .collect::<Vec<_>>(),
    )));
    window.set_agent_disabled(ModelRc::new(VecModel::from(
        choices
            .agents
            .iter()
            .map(|agent| !agent.is_launchable())
            .collect::<Vec<_>>(),
    )));
    window.set_agent_notes(
        choices
            .agents
            .iter()
            .filter_map(|agent| {
                agent
                    .unlaunchable
                    .as_ref()
                    .map(|reason| format!("{}: {reason}", agent.name))
            })
            .collect::<Vec<_>>()
            .join("\n")
            .into(),
    );
    // An unknown id stays unselected.
    let project = preselect
        .and_then(|id| choices.projects.iter().position(|p| p.id == id))
        .and_then(|index| i32::try_from(index).ok())
        .unwrap_or(-1);
    window.set_project_index(project);
    // Launchable agents are listed first, so index 0 is selectable if any is.
    window.set_agent_index(if choices.has_launchable_agent() {
        0
    } else {
        -1
    });
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
    /// Project id from the command line.
    requested_project: Option<String>,
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
    fn new(
        daemon: DaemonConfig,
        choices: Arc<Mutex<Choices>>,
        requested_project: Option<String>,
    ) -> Self {
        Self {
            daemon,
            choices,
            requested_project,
            generation: Arc::new(Generation::default()),
            state: Arc::new(Mutex::new(TemplateState::default())),
        }
    }

    /// The form as it stands in the window.
    fn form(&self, window: &NewSessionWindow) -> NewSessionForm {
        let agent = usize::try_from(window.get_agent_index())
            .ok()
            .and_then(|index| {
                lock(&self.choices)
                    .agents
                    .get(index)
                    .map(|a| a.name.clone())
            });
        let (recipe, template_loading) = {
            let state = lock(&self.state);
            (state.recipe.clone(), state.loading)
        };
        NewSessionForm {
            project: selected_project(window, &self.choices),
            agent,
            name: window.get_session_name().into(),
            prompt: window.get_prompt().into(),
            recipe,
            template_loading,
            branch: window.get_branch().into(),
            base_branch: window.get_base_branch().into(),
        }
    }

    fn activity(window: &NewSessionWindow) -> Activity {
        Activity {
            loading: window.get_loading(),
            in_flight: window.get_busy(),
        }
    }

    /// Recomputes the line that says why Create is disabled.
    fn refresh_blocker(&self, window: &NewSessionWindow) {
        let blocker = submit_blocker(
            &self.form(window),
            &lock(&self.choices),
            self.requested_project.as_deref(),
            Self::activity(window),
        );
        window.set_blocker(blocker.map(|b| b.to_string()).unwrap_or_default().into());
    }

    /// Resets to Blank and reads the templates of the selected project.
    fn project_changed(&self, window: &NewSessionWindow) {
        let token = self.generation.advance();
        *lock(&self.state) = TemplateState::default();
        show_template_names(window, &[]);
        window.set_template_index(0);
        window.set_template_loading(false);
        self.refresh_blocker(window);
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
        self.refresh_blocker(window);
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
        self.refresh_blocker(window);
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
                this.refresh_blocker(&window);
            });
            if let Err(err) = delivered {
                error!(error = %err, "deliver the template to the UI");
            }
        });
    }

    fn apply(&self, window: &NewSessionWindow, recipe: Recipe) {
        let agent = lock(&self.choices)
            .launchable_position(&recipe.agent)
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
        self.refresh_blocker(window);
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

/// How to attach the created session, when the owner asked for it.
struct AttachAfter {
    settings: AttachConfig,
    config_path: Result<PathBuf, String>,
}

/// Attaches `session`; the error text reports that the session already exists.
fn attach_created(after: &AttachAfter, session: &str) -> Result<(), String> {
    let config_path = after.config_path.as_ref().map_err(|err| {
        format!("Session {session} was created, but it could not be attached: {err}")
    })?;
    let key = SessionKey {
        host: LOCAL_HOST.to_owned(),
        session_id: session.to_owned(),
    };
    attach(&after.settings, config_path, &key).map_err(|err| match err {
        AttachError::NotConfigured { .. } => {
            format!("Session {session} was created, but it was not attached: {err}")
        }
        other => format!("Session {session} was created, but it could not be attached: {other}"),
    })
}

fn submit_in_background(
    window: slint::Weak<NewSessionWindow>,
    templates: Templates,
    daemon: DaemonConfig,
    config: NewSessionConfig,
    params: SessionNewParams,
    attach_after: Option<AttachAfter>,
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
        let attached = match (&outcome, &attach_after) {
            (Ok(id), Some(after)) => attach_created(after, id),
            _ => Ok(()),
        };
        if let Err(message) = &attached {
            warn!(error = %message, "attach after create failed");
        }
        let delivered = window.upgrade_in_event_loop(move |window| match (outcome, attached) {
            (Ok(_), Ok(())) => {
                if let Err(err) = window.hide() {
                    error!(error = %err, "hide the new-session window");
                }
            }
            (Ok(_), Err(message)) => {
                window.set_busy(false);
                window.set_created(true);
                window.set_error(message.into());
            }
            (Err(err), _) => {
                // An unknown outcome or an unconfirmed prompt keeps the window
                // busy: Create stays disabled so the request is not repeated.
                window.set_busy(matches!(
                    err,
                    CreateError::Unconfirmed | CreateError::PromptUnconfirmed
                ));
                window.set_error(err.to_string().into());
                templates.refresh_blocker(&window);
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
    window.set_blocker(FormError::Loading.to_string().into());
    match Config::load() {
        Ok(config) => {
            let templates =
                Templates::new(config.daemon.clone(), choices.clone(), args.project.clone());
            window.set_attach_after_create(config.new_session.attach_after_create);
            let config_path = Config::path().map_err(|err| err.to_string());
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
            let on_agent = templates.clone();
            let weak = window.as_weak();
            window.on_agent_changed(move |_| {
                if let Some(window) = weak.upgrade() {
                    on_agent.refresh_blocker(&window);
                }
            });
            let on_overrides = templates.clone();
            let weak = window.as_weak();
            window.on_overrides_edited(move || {
                if let Some(window) = weak.upgrade() {
                    on_overrides.refresh_blocker(&window);
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
                let form = templates.form(&window);
                let choices = lock(&choices).clone();
                match build_params(
                    &form,
                    &choices,
                    templates.requested_project.as_deref(),
                    Templates::activity(&window),
                    &config.new_session,
                ) {
                    Ok(params) => {
                        info!(agent = %params.agent, "create requested");
                        window.set_error("".into());
                        window.set_busy(true);
                        templates.refresh_blocker(&window);
                        let attach_after = window.get_attach_after_create().then(|| AttachAfter {
                            settings: config.attach(),
                            config_path: config_path.clone(),
                        });
                        submit_in_background(
                            weak.clone(),
                            templates.clone(),
                            config.daemon.clone(),
                            config.new_session.clone(),
                            params,
                            attach_after,
                        );
                    }
                    Err(err) => window.set_error(err.to_string().into()),
                }
            });
        }
        Err(err) => {
            error!(error = %err, "load companion.toml");
            window.set_loading(false);
            window.set_blocker("".into());
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
