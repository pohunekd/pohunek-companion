//! The `new-session` subcommand: a standalone dialog that creates one session
//! through the daemon. It runs as its own process, without a tray icon or the
//! session stream.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use pohunek_companion_core::{
    Activity, AttachConfig, AttachError, Choices, Config, CreateError, DaemonConfig, DialogHost,
    DiscoveryPlan, FormError, Generation, HostConfig, LOCAL_HOST, NewSessionConfig, NewSessionForm,
    PendingStore, ProjectChoice, Recipe, SessionKey, SessionNewParams, attach, build_params,
    create_session, decode_dialog_hosts, dialog_hosts, discover_dialog_hosts, fuzzy_matches,
    load_choices_staged, load_recipe, load_templates, register_dialog_routes, submit_blocker,
};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};
use tracing::{error, info, warn};

use crate::{Fuzzy, NewSessionWindow};

/// Wayland app id of the dialog; distinct from the sidebar's so compositor
/// rules can float it separately.
const APP_ID: &str = "pohunek-companion-new-session";
const HOST_FLAG: &str = "--host";
const PROJECT_FLAG: &str = "--project";
const GROUP_FLAG: &str = "--group";
const KNOWN_HOSTS_FLAG: &str = "--known-hosts";

/// Parsed `new-session` arguments.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Args {
    /// Host target to preselect; `local`, a configured `[[hosts]]` target or a
    /// discovered host's key.
    pub host: Option<String>,
    /// Project id to preselect.
    pub project: Option<String>,
    /// Id of the My groups group the created session is placed in.
    pub group: Option<String>,
    /// The remote hosts the sidebar already resolved, with their routes. The
    /// dialog lists them at once and skips its own discovery.
    pub known_hosts: Option<Vec<DialogHost>>,
}

pub fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Args, String> {
    let mut parsed = Args::default();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            HOST_FLAG => {
                let target = args
                    .next()
                    .filter(|target| !target.is_empty())
                    .ok_or_else(|| format!("{HOST_FLAG} needs a host target"))?;
                parsed.host = Some(target);
            }
            PROJECT_FLAG => {
                let id = args
                    .next()
                    .filter(|id| !id.is_empty())
                    .ok_or_else(|| format!("{PROJECT_FLAG} needs a project id"))?;
                parsed.project = Some(id);
            }
            GROUP_FLAG => {
                let id = args
                    .next()
                    .filter(|id| !id.is_empty())
                    .ok_or_else(|| format!("{GROUP_FLAG} needs a group id"))?;
                parsed.group = Some(id);
            }
            KNOWN_HOSTS_FLAG => {
                let json = args
                    .next()
                    .ok_or_else(|| format!("{KNOWN_HOSTS_FLAG} needs the hosts"))?;
                parsed.known_hosts =
                    Some(decode_dialog_hosts(&json).map_err(|err| err.to_string())?);
            }
            other => return Err(format!("unknown argument `{other}`")),
        }
    }
    Ok(parsed)
}

pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

pub(crate) fn block_on<F: std::future::Future>(future: F) -> Result<F::Output, std::io::Error> {
    Ok(tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(future))
}

/// Host targets offered by the dialog: Local, then every remote host.
pub(crate) fn host_targets(hosts: &[DialogHost]) -> Vec<String> {
    std::iter::once(LOCAL_HOST.to_owned())
        .chain(hosts.iter().map(|host| host.target.clone()))
        .collect()
}

/// What the `--host` argument means for the hosts known so far.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Requested {
    Local,
    /// Index in [`host_targets`].
    Host(usize),
    /// Not listed yet; discovery may still add it.
    Pending(String),
    /// Unknown after discovery, or listed but unpickable: the message to show.
    Refused(String),
}

pub(crate) fn resolve_host(
    requested: Option<&str>,
    hosts: &[DialogHost],
    discovered: bool,
) -> Requested {
    let Some(target) = requested.filter(|target| *target != LOCAL_HOST) else {
        return Requested::Local;
    };
    let Some(host) = hosts.iter().position(|host| host.target == target) else {
        return if discovered {
            Requested::Refused(format!(
                "unknown host `{target}`: it is not `local`, not a `[[hosts]]` target in companion.toml and not a reachable host the local daemon discovers"
            ))
        } else {
            Requested::Pending(target.to_owned())
        };
    };
    match &hosts[host].unavailable {
        Some(reason) => Requested::Refused(format!("host `{target}` cannot be used: {reason}")),
        None => Requested::Host(host + 1),
    }
}

/// Releases remote loads once discovery has registered its routes; Local
/// never waits for it.
#[derive(Default)]
pub(crate) struct DiscoveryGate {
    done: Mutex<bool>,
    ready: Condvar,
}

impl DiscoveryGate {
    pub(crate) fn open() -> Self {
        Self {
            done: Mutex::new(true),
            ready: Condvar::new(),
        }
    }

    pub(crate) fn finish(&self) {
        *lock(&self.done) = true;
        self.ready.notify_all();
    }

    pub(crate) fn wait(&self) {
        let mut done = lock(&self.done);
        while !*done {
            done = self
                .ready
                .wait(done)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }
}

/// Opens the gate when dropped, so a discovery thread that ends early cannot
/// leave a remote load waiting.
pub(crate) struct FinishOnDrop(pub(crate) Arc<DiscoveryGate>);

impl Drop for FinishOnDrop {
    fn drop(&mut self) {
        self.0.finish();
    }
}

fn show_choices(
    window: &NewSessionWindow,
    choices: &Choices,
    preselect: Option<&str>,
    agent: Option<&str>,
) {
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
    let agent = agent
        .and_then(|name| choices.launchable_position(name))
        .and_then(|index| i32::try_from(index).ok())
        .unwrap_or(if choices.has_launchable_agent() {
            0
        } else {
            -1
        });
    window.set_agent_index(agent);
}

/// Why Create is disabled when the host's projects and agents could not be read.
const HOST_UNREADABLE: &str = "This host's projects and agents could not be read.";

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
    /// Targets of the host select, Local first.
    hosts: Arc<Mutex<Vec<String>>>,
    /// Remote loads wait for it.
    discovery: Arc<DiscoveryGate>,
    /// Why the requested host cannot be used; Create stays disabled until
    /// another host is chosen.
    host_failure: Arc<Mutex<Option<String>>>,
    /// Target of the host the dialog works against.
    host: Arc<Mutex<String>>,
    /// Reading the host's choices failed; Create stays disabled.
    load_failed: Arc<AtomicBool>,
    /// Advances on every host change so a late choices response is dropped.
    host_generation: Arc<Generation>,
    daemon: DaemonConfig,
    choices: Arc<Mutex<Choices>>,
    generation: Arc<Generation>,
    state: Arc<Mutex<TemplateState>>,
    /// Project id from the command line.
    requested_project: Option<String>,
    /// When the dialog process entered `run`; timestamps the load logs.
    started: Instant,
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
        hosts: Vec<String>,
        discovery: Arc<DiscoveryGate>,
        host: String,
        daemon: DaemonConfig,
        choices: Arc<Mutex<Choices>>,
        requested_project: Option<String>,
        started: Instant,
    ) -> Self {
        Self {
            started,
            hosts: Arc::new(Mutex::new(hosts)),
            discovery,
            host_failure: Arc::new(Mutex::new(None)),
            host: Arc::new(Mutex::new(host)),
            load_failed: Arc::new(AtomicBool::new(false)),
            host_generation: Arc::new(Generation::default()),
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

    fn host(&self) -> String {
        lock(&self.host).clone()
    }

    /// Switches to the host at `index` of the host select: drops the current
    /// choices and reads the new host's. The chosen project and agent stay
    /// selected when the new host has them.
    fn host_changed(&self, window: &NewSessionWindow, index: i32) {
        let Some(target) = usize::try_from(index)
            .ok()
            .and_then(|index| lock(&self.hosts).get(index).cloned())
        else {
            return;
        };
        // The create goes to the host it was submitted for; the select snaps
        // back while a request is in flight or the session already exists.
        if window.get_busy() || window.get_created() {
            let current = lock(&self.hosts)
                .iter()
                .position(|host| *host == self.host());
            window.set_host_index(current.and_then(|i| i32::try_from(i).ok()).unwrap_or(0));
            return;
        }
        let project = selected_project(window, &self.choices);
        let agent = usize::try_from(window.get_agent_index())
            .ok()
            .and_then(|index| {
                lock(&self.choices)
                    .agents
                    .get(index)
                    .map(|a| a.name.clone())
            });
        info!(host = %target, "host chosen");
        *lock(&self.host) = target;
        *lock(&self.host_failure) = None;
        self.load_failed.store(false, Ordering::SeqCst);
        // Pending template responses belong to the previous host.
        self.generation.advance();
        let token = self.host_generation.advance();
        *lock(&self.choices) = Choices::default();
        *lock(&self.state) = TemplateState::default();
        window.set_error("".into());
        window.set_loading(true);
        window.set_projects_ready(false);
        window.set_template_loading(false);
        show_choices(window, &Choices::default(), None, None);
        show_template_names(window, &[]);
        window.set_template_index(0);
        self.refresh_blocker(window);
        load_in_background(
            self.started,
            window.as_weak(),
            self.clone(),
            token,
            project,
            agent,
        );
    }

    fn activity(window: &NewSessionWindow) -> Activity {
        Activity {
            loading: window.get_loading(),
            in_flight: window.get_busy(),
        }
    }

    /// Recomputes the line that says why Create is disabled.
    fn refresh_blocker(&self, window: &NewSessionWindow) {
        if let Some(message) = lock(&self.host_failure).clone() {
            window.set_blocker(message.into());
            return;
        }
        if self.load_failed.load(Ordering::SeqCst) {
            window.set_blocker(HOST_UNREADABLE.into());
            return;
        }
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
        let host = self.host();
        let weak = window.as_weak();
        std::thread::spawn(move || {
            let outcome = block_on(load_templates(&host, &this.daemon, &project))
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
        let host = self.host();
        let weak = window.as_weak();
        std::thread::spawn(move || {
            let outcome = block_on(load_recipe(&host, &this.daemon, &project, &name))
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

/// Reads the choices of the current host and shows them, selecting `project`
/// and `agent` when the host has them. The projects are shown as soon as the
/// daemon lists them, the agents when it has probed its runtimes. A failure
/// shows the error and leaves Create disabled. A remote host is read only
/// after host discovery has registered its route.
fn load_in_background(
    started: Instant,
    window: slint::Weak<NewSessionWindow>,
    templates: Templates,
    token: u64,
    project: Option<String>,
    agent: Option<String>,
) {
    std::thread::spawn(move || {
        let host = templates.host();
        if host != LOCAL_HOST {
            templates.discovery.wait();
        }
        let early = {
            let (window, templates, project) = (window.clone(), templates.clone(), project.clone());
            move |projects: &[ProjectChoice]| {
                let projects = projects.to_vec();
                let delivered = window.upgrade_in_event_loop(move |window| {
                    if !templates.host_generation.is_current(token) {
                        return;
                    }
                    info!(
                        elapsed_ms = elapsed_ms(started),
                        projects = projects.len(),
                        "projects loaded"
                    );
                    let preselected = project.as_deref().and_then(|id| {
                        projects
                            .iter()
                            .position(|candidate| candidate.id == id)
                            .and_then(|index| i32::try_from(index).ok())
                    });
                    window.set_project_names(ModelRc::new(VecModel::from(
                        projects
                            .iter()
                            .map(|project| SharedString::from(project.label.as_str()))
                            .collect::<Vec<_>>(),
                    )));
                    window.set_project_index(preselected.unwrap_or(-1));
                    lock(&templates.choices).projects = projects;
                    window.set_projects_ready(true);
                    templates.project_changed(&window);
                });
                if let Err(err) = delivered {
                    error!(error = %err, "deliver the new-session projects to the UI");
                }
            }
        };
        let outcome = block_on(load_choices_staged(&host, &templates.daemon, early))
            .map_err(|err| format!("cannot start the background runtime: {err}"))
            .and_then(|loaded| loaded.map_err(|err| err.to_string()));
        if let Err(message) = &outcome {
            error!(host = %host, error = %message, "load new-session choices");
        }
        let delivered = window.upgrade_in_event_loop(move |window| {
            if !templates.host_generation.is_current(token) {
                return;
            }
            window.set_loading(false);
            info!(
                elapsed_ms = elapsed_ms(started),
                ok = outcome.is_ok(),
                "choices loaded"
            );
            match outcome {
                Ok(loaded) => {
                    // The project may have been picked while the agents loaded.
                    let chosen = selected_project(&window, &templates.choices).or(project);
                    show_choices(&window, &loaded, chosen.as_deref(), agent.as_deref());
                    *lock(&templates.choices) = loaded;
                    templates.refresh_blocker(&window);
                }
                Err(message) => {
                    templates.load_failed.store(true, Ordering::SeqCst);
                    window.set_error(message.into());
                    templates.refresh_blocker(&window);
                }
            }
        });
        if let Err(err) = delivered {
            error!(error = %err, "deliver the new-session choices to the UI");
        }
    });
}

pub(crate) fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// How to attach the created session, when the owner asked for it.
pub(crate) struct AttachAfter {
    pub(crate) settings: AttachConfig,
    pub(crate) config_path: Result<PathBuf, String>,
}

/// Attaches `session`; the error text reports that the session already exists.
pub(crate) fn attach_created(after: &AttachAfter, host: &str, session: &str) -> Result<(), String> {
    let config_path = after.config_path.as_ref().map_err(|err| {
        format!("Session {session} was created, but it could not be attached: {err}")
    })?;
    let key = SessionKey {
        host: host.to_owned(),
        session_id: session.to_owned(),
    };
    attach(&after.settings, config_path, &key).map_err(|err| match err {
        AttachError::NotConfigured { .. } => {
            format!("Session {session} was created, but it was not attached: {err}")
        }
        other => format!("Session {session} was created, but it could not be attached: {other}"),
    })
}

/// Where the created session goes in My groups.
#[derive(Clone)]
struct PlaceAfter {
    group: String,
    max_age: Duration,
}

/// Records the placement for the sidebar to apply; the error text reports
/// that the session already exists.
fn record_placement(place: &PlaceAfter, host: &str, session: &str) -> Result<(), String> {
    let key = SessionKey {
        host: host.to_owned(),
        session_id: session.to_owned(),
    };
    PendingStore::open()
        .and_then(|store| store.record(&key, &place.group, place.max_age))
        .map_err(|err| {
            format!("Session {session} was created, but its group could not be recorded: {err}")
        })
}

fn submit_in_background(
    window: slint::Weak<NewSessionWindow>,
    templates: Templates,
    host: String,
    config: NewSessionConfig,
    params: SessionNewParams,
    attach_after: Option<AttachAfter>,
    place_after: Option<PlaceAfter>,
) {
    std::thread::spawn(move || {
        let outcome = match block_on(create_session(&host, &templates.daemon, &config, params)) {
            Ok(outcome) => outcome,
            Err(err) => Err(CreateError::NotSent(format!(
                "cannot start the background runtime: {err}"
            ))),
        };
        match &outcome {
            Ok(id) => info!(host = %host, session = %id, "create succeeded"),
            Err(err) => warn!(host = %host, code = err.code(), "create failed"),
        }
        let placed = match (&outcome, &place_after) {
            (Ok(id), Some(place)) => record_placement(place, &host, id),
            _ => Ok(()),
        };
        if let Err(message) = &placed {
            warn!(error = %message, "record placement after create failed");
        }
        let attached = match (&outcome, &attach_after) {
            (Ok(id), Some(after)) => attach_created(after, &host, id),
            _ => Ok(()),
        };
        if let Err(message) = &attached {
            warn!(error = %message, "attach after create failed");
        }
        // The first failure after a successful create is the one shown.
        let attached = placed.and(attached);
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

/// What the host select shows for Local followed by `hosts`: the labels, which
/// entries cannot be picked, and the notes saying why.
pub(crate) struct HostRows {
    pub(crate) names: ModelRc<SharedString>,
    pub(crate) disabled: ModelRc<bool>,
    pub(crate) notes: SharedString,
}

pub(crate) fn host_rows(hosts: &[DialogHost]) -> HostRows {
    HostRows {
        names: ModelRc::new(VecModel::from(
            std::iter::once(SharedString::from("Local"))
                .chain(
                    hosts
                        .iter()
                        .map(|host| SharedString::from(host.label.as_str())),
                )
                .collect::<Vec<_>>(),
        )),
        disabled: ModelRc::new(VecModel::from(
            std::iter::once(false)
                .chain(hosts.iter().map(|host| host.unavailable.is_some()))
                .collect::<Vec<_>>(),
        )),
        notes: hosts
            .iter()
            .filter_map(|host| host.unavailable.as_deref())
            .collect::<Vec<_>>()
            .join("\n")
            .into(),
    }
}

/// Fills the host select with Local followed by `hosts`.
fn show_hosts(window: &NewSessionWindow, hosts: &[DialogHost]) {
    let rows = host_rows(hosts);
    window.set_host_names(rows.names);
    window.set_host_disabled(rows.disabled);
    window.set_host_notes(rows.notes);
}

/// Discovers the remote hosts without holding the window back: the host select
/// is filled when the local daemon answers. `requested` is a `--host` target
/// that was not listed yet; it is selected, and its choices loaded, once
/// discovery lists it and the owner has not picked another host meanwhile.
fn discover_in_background(
    started: Instant,
    window: slint::Weak<NewSessionWindow>,
    templates: Templates,
    configured: Vec<HostConfig>,
    requested: Option<String>,
    launch_token: u64,
    project: Option<String>,
) {
    std::thread::spawn(move || {
        let _open_gate = FinishOnDrop(templates.discovery.clone());
        let (hosts, failure) = block_on(discover_dialog_hosts(&templates.daemon, &configured))
            .unwrap_or_else(|err| {
                (
                    dialog_hosts(&configured, &DiscoveryPlan::default()),
                    Some(format!("cannot start the background runtime: {err}")),
                )
            });
        // Routes are registered; remote loads may proceed.
        templates.discovery.finish();
        if let Some(message) = failure {
            warn!(error = %message, "host discovery failed; the dialog lists configured hosts only");
        }
        let delivered = window.upgrade_in_event_loop(move |window| {
            info!(
                elapsed_ms = elapsed_ms(started),
                hosts = hosts.len(),
                "hosts listed"
            );
            let picked_since = !templates.host_generation.is_current(launch_token);
            let awaited = requested.is_some() && !picked_since;
            let target = requested.filter(|_| !picked_since);
            let resolved = resolve_host(target.as_deref(), &hosts, true);
            let mut index = None;
            match &resolved {
                Requested::Host(position) => index = Some(*position),
                Requested::Refused(message) => {
                    error!(error = %message, "resolve the requested host");
                    *lock(&templates.host_failure) = Some(message.clone());
                    index = Some(0);
                }
                Requested::Local | Requested::Pending(_) => {}
            }
            let targets = host_targets(&hosts);
            if index.is_none() {
                // Keep what the owner has selected, wherever it moved to.
                let current = templates.host();
                index = targets.iter().position(|target| *target == current);
            }
            *lock(&templates.hosts) = targets.clone();
            show_hosts(&window, &hosts);
            window.set_hosts_loading(false);
            let index = index.unwrap_or(0);
            window.set_host_index(i32::try_from(index).unwrap_or(0));
            if awaited {
                *lock(&templates.host) = targets[index].clone();
                let token = templates.host_generation.advance();
                *lock(&templates.choices) = Choices::default();
                window.set_projects_ready(false);
                window.set_loading(true);
                templates.refresh_blocker(&window);
                load_in_background(
                    started,
                    window.as_weak(),
                    templates.clone(),
                    token,
                    project,
                    None,
                );
            }
        });
        if let Err(err) = delivered {
            error!(error = %err, "deliver the discovered hosts to the UI");
        }
    });
}

pub fn run(args: Args) -> Result<(), slint::PlatformError> {
    let started = Instant::now();
    info!("new-session dialog started");
    let window = NewSessionWindow::new()?;
    window.global::<Fuzzy>().on_matches(|query, items| {
        let items: Vec<SharedString> = items.iter().collect();
        let matched: Vec<i32> = fuzzy_matches(query.as_str(), &items)
            .into_iter()
            .filter_map(|index| i32::try_from(index).ok())
            .collect();
        ModelRc::new(VecModel::from(matched))
    });
    slint::set_xdg_app_id(APP_ID)?;

    let choices = Arc::new(Mutex::new(Choices::default()));
    show_template_names(&window, &[]);
    window.set_blocker(FormError::Loading.to_string().into());
    match Config::load() {
        Ok(config) => {
            let discovered = args.known_hosts.is_some();
            let remote_hosts = match &args.known_hosts {
                Some(known) => {
                    register_dialog_routes(known);
                    known.clone()
                }
                None => dialog_hosts(&config.hosts, &DiscoveryPlan::default()),
            };
            let requested = resolve_host(args.host.as_deref(), &remote_hosts, discovered);
            let mut targets = host_targets(&remote_hosts);
            let mut host_index = 0;
            let mut shown_hosts = remote_hosts.clone();
            let mut waiting_for = None;
            let mut host_failure = None;
            match &requested {
                Requested::Local => {}
                Requested::Host(index) => host_index = *index,
                Requested::Pending(target) => {
                    // A placeholder keeps the requested host selected, unpickable,
                    // until discovery lists it.
                    shown_hosts.push(DialogHost {
                        target: target.clone(),
                        label: target.clone(),
                        unavailable: Some(format!("{target}: looking for this host...")),
                        route: None,
                    });
                    targets.push(target.clone());
                    host_index = targets.len() - 1;
                    waiting_for = Some(target.clone());
                }
                Requested::Refused(message) => {
                    error!(error = %message, "resolve the requested host");
                    host_failure = Some(message.clone());
                }
            }
            let gate = Arc::new(if discovered {
                DiscoveryGate::open()
            } else {
                DiscoveryGate::default()
            });
            show_hosts(&window, &shown_hosts);
            window.set_hosts_loading(!discovered);
            window.set_host_index(i32::try_from(host_index).unwrap_or(0));
            if discovered {
                info!(
                    elapsed_ms = elapsed_ms(started),
                    hosts = remote_hosts.len(),
                    "hosts listed"
                );
            }
            let templates = Templates::new(
                targets.clone(),
                gate.clone(),
                targets[host_index].clone(),
                config.daemon.clone(),
                choices.clone(),
                args.project.clone(),
                started,
            );
            *lock(&templates.host_failure) = host_failure;
            window.set_attach_after_create(config.new_session.attach_after_create);
            let config_path = Config::path().map_err(|err| err.to_string());
            let load_token = templates.host_generation.advance();
            if waiting_for.is_none() {
                load_in_background(
                    started,
                    window.as_weak(),
                    templates.clone(),
                    load_token,
                    args.project.clone(),
                    None,
                );
            }
            if !discovered {
                discover_in_background(
                    started,
                    window.as_weak(),
                    templates.clone(),
                    config.hosts.clone(),
                    args.host.clone().filter(|_| waiting_for.is_some()),
                    load_token,
                    args.project.clone(),
                );
            }
            let on_host = templates.clone();
            let weak = window.as_weak();
            window.on_host_changed(move |index| {
                if let Some(window) = weak.upgrade() {
                    on_host.host_changed(&window, index);
                }
            });
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
            let group = args.group;
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
                        let place_after = group.clone().map(|group| PlaceAfter {
                            group,
                            max_age: config.new_session.pending_placement_max_age(),
                        });
                        submit_in_background(
                            weak.clone(),
                            templates.clone(),
                            templates.host(),
                            config.new_session.clone(),
                            params,
                            attach_after,
                            place_after,
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
    info!(
        elapsed_ms = started.elapsed().as_millis() as u64,
        "window shown"
    );
    window.run()
}
