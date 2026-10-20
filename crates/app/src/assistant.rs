//! The `assistant` subcommand: a standalone dialog that starts a Pohunek
//! assistant session through the `pohunek-assistant` launch orchestration. It
//! runs as its own process, without a tray icon or the session stream, and
//! shares the host handling of the new-session dialog.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use pohunek_companion_core::{
    Activity, AssistantForm, AssistantFormError, Choices, Config, CreateError, DaemonConfig,
    DialogHost, DiscoveryPlan, Generation, HostConfig, INTENTS, Intent, LOCAL_HOST,
    NewSessionConfig, ProjectChoice, assistant_blocker, decode_dialog_hosts, dialog_hosts,
    discover_dialog_hosts, fuzzy_matches, is_remote_host, launch_assistant, load_choices_staged,
    register_dialog_routes,
};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};
use tracing::{error, info, warn};

use crate::new_session::{
    AttachAfter, DiscoveryGate, FinishOnDrop, Requested, attach_created, block_on, elapsed_ms,
    host_rows, host_targets, lock, resolve_host,
};
use crate::{AssistantWindow, Fuzzy};

/// Wayland app id of the dialog; distinct from the sidebar's and the
/// new-session dialog's so compositor rules can float it separately.
const APP_ID: &str = "pohunek-companion-assistant";
const HOST_FLAG: &str = "--host";
const PROJECT_FLAG: &str = "--project";
const INTENT_FLAG: &str = "--intent";
const KNOWN_HOSTS_FLAG: &str = "--known-hosts";

/// Why Start is disabled when the host's projects and agents could not be read.
const HOST_UNREADABLE: &str = "This host's projects and agents could not be read.";
/// Why Start is disabled when `companion.toml` could not be loaded.
const CONFIG_UNREADABLE: &str = "companion.toml could not be loaded.";
/// First entries of the project and agent selects: no project, and the
/// launch's own agent choice.
const NO_PROJECT_LABEL: &str = "No project";
const AUTOMATIC_LABEL: &str = "Automatic";

/// Parsed `assistant` arguments.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Args {
    /// Host target to preselect; `local`, a configured `[[hosts]]` target or a
    /// discovered host's key.
    pub host: Option<String>,
    /// Project id to preselect.
    pub project: Option<String>,
    /// Intent to preselect; `help` when absent.
    pub intent: Option<Intent>,
    /// The remote hosts the sidebar already resolved, with their routes. The
    /// dialog lists them at once and skips its own discovery.
    pub known_hosts: Option<Vec<DialogHost>>,
}

fn flag_value(
    args: &mut impl Iterator<Item = String>,
    flag: &str,
    what: &str,
) -> Result<String, String> {
    args.next()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("{flag} needs {what}"))
}

pub fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Args, String> {
    let mut parsed = Args::default();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            HOST_FLAG => parsed.host = Some(flag_value(&mut args, HOST_FLAG, "a host target")?),
            PROJECT_FLAG => {
                parsed.project = Some(flag_value(&mut args, PROJECT_FLAG, "a project id")?);
            }
            INTENT_FLAG => {
                let name = flag_value(&mut args, INTENT_FLAG, "an intent")?;
                parsed.intent = Some(
                    INTENTS
                        .into_iter()
                        .find(|intent| intent.as_str() == name)
                        .ok_or_else(|| {
                            let known: Vec<_> = INTENTS.into_iter().map(Intent::as_str).collect();
                            format!(
                                "unknown intent `{name}`; expected one of {}",
                                known.join(", ")
                            )
                        })?,
                );
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

/// Shows `text` with its first line as the message and the rest as the hint.
fn show_error(window: &AssistantWindow, text: &str) {
    let (message, hint) = text.split_once('\n').unwrap_or((text, ""));
    window.set_error(message.into());
    window.set_error_hint(hint.trim().into());
}

/// Capitalized label of an intent chip.
fn intent_label(intent: Intent) -> String {
    let name = intent.as_str();
    let mut chars = name.chars();
    chars
        .next()
        .map(|first| first.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

/// Index into the list behind a leading "none" entry; `None` for that entry.
fn behind_none(index: i32) -> Option<usize> {
    usize::try_from(index).ok()?.checked_sub(1)
}

fn labels<'a>(first: &'a str, rest: impl Iterator<Item = &'a str>) -> ModelRc<SharedString> {
    ModelRc::new(VecModel::from(
        std::iter::once(first)
            .chain(rest)
            .map(SharedString::from)
            .collect::<Vec<_>>(),
    ))
}

fn show_projects(window: &AssistantWindow, projects: &[ProjectChoice], selected: Option<&str>) {
    window.set_project_names(labels(
        NO_PROJECT_LABEL,
        projects.iter().map(|project| project.label.as_str()),
    ));
    let index = selected
        .and_then(|id| projects.iter().position(|project| project.id == id))
        .and_then(|position| i32::try_from(position + 1).ok())
        .unwrap_or(0);
    window.set_project_index(index);
}

/// Shows the assistant-capable agents behind "Automatic", selecting `agent`
/// when the host has it.
fn show_agents(window: &AssistantWindow, choices: &Choices, agent: Option<&str>) {
    window.set_agent_names(labels(
        AUTOMATIC_LABEL,
        choices.assistant_agents.iter().map(String::as_str),
    ));
    let index = agent
        .and_then(|name| choices.assistant_agents.iter().position(|a| a == name))
        .and_then(|position| i32::try_from(position + 1).ok())
        .unwrap_or(0);
    window.set_agent_index(index);
}

/// Host and choice state shared by the dialog's callbacks and loader threads.
#[derive(Clone)]
struct Dialog {
    /// Targets of the host select, Local first.
    hosts: Arc<Mutex<Vec<String>>>,
    /// Remote loads wait for it.
    discovery: Arc<DiscoveryGate>,
    /// Why the requested host cannot be used; Start stays disabled until
    /// another host is chosen.
    host_failure: Arc<Mutex<Option<String>>>,
    /// Target of the host the dialog works against.
    host: Arc<Mutex<String>>,
    /// Reading the host's choices failed; Start stays disabled.
    load_failed: Arc<AtomicBool>,
    /// Advances on every host change so a late choices response is dropped.
    host_generation: Arc<Generation>,
    daemon: DaemonConfig,
    choices: Arc<Mutex<Choices>>,
    /// When the dialog process entered `run`; timestamps the load logs.
    started: Instant,
}

impl Dialog {
    fn host(&self) -> String {
        lock(&self.host).clone()
    }

    /// The form as it stands in the window.
    fn form(&self, window: &AssistantWindow) -> AssistantForm {
        let choices = lock(&self.choices);
        AssistantForm {
            intent: usize::try_from(window.get_intent_index())
                .ok()
                .and_then(|index| INTENTS.get(index).copied())
                .unwrap_or(INTENTS[0]),
            project: behind_none(window.get_project_index())
                .and_then(|index| choices.projects.get(index).map(|p| p.id.clone())),
            agent: behind_none(window.get_agent_index())
                .and_then(|index| choices.assistant_agents.get(index).cloned()),
            request: window.get_request().into(),
        }
    }

    fn activity(window: &AssistantWindow) -> Activity {
        Activity {
            loading: window.get_loading(),
            in_flight: window.get_busy(),
        }
    }

    /// Recomputes the line that says why Start is disabled.
    fn refresh_blocker(&self, window: &AssistantWindow) {
        if let Some(message) = lock(&self.host_failure).clone() {
            window.set_blocker(message.into());
            return;
        }
        if self.load_failed.load(Ordering::SeqCst) {
            window.set_blocker(HOST_UNREADABLE.into());
            return;
        }
        let blocker = assistant_blocker(
            &self.form(window),
            &lock(&self.choices),
            is_remote_host(&self.host()),
            Self::activity(window),
        );
        window.set_blocker(blocker.map(|b| b.to_string()).unwrap_or_default().into());
    }

    /// Switches to the host at `index` of the host select: drops the current
    /// choices and reads the new host's. The chosen project and agent stay
    /// selected when the new host has them.
    fn host_changed(&self, window: &AssistantWindow, index: i32) {
        let Some(target) = usize::try_from(index)
            .ok()
            .and_then(|index| lock(&self.hosts).get(index).cloned())
        else {
            return;
        };
        // The launch goes to the host it was submitted for; the select snaps
        // back while a request is in flight or the session already exists.
        if window.get_busy() || window.get_created() {
            let current = lock(&self.hosts)
                .iter()
                .position(|host| *host == self.host());
            window.set_host_index(current.and_then(|i| i32::try_from(i).ok()).unwrap_or(0));
            return;
        }
        let form = self.form(window);
        info!(host = %target, "host chosen");
        *lock(&self.host) = target;
        *lock(&self.host_failure) = None;
        self.load_failed.store(false, Ordering::SeqCst);
        let token = self.host_generation.advance();
        *lock(&self.choices) = Choices::default();
        show_error(window, "");
        window.set_loading(true);
        window.set_projects_ready(false);
        show_projects(window, &[], None);
        show_agents(window, &Choices::default(), None);
        self.refresh_blocker(window);
        load_in_background(
            window.as_weak(),
            self.clone(),
            token,
            form.project,
            form.agent,
        );
    }
}

/// Reads the choices of the current host and shows them, selecting `project`
/// and `agent` when the host has them. The projects are shown as soon as the
/// daemon lists them, the agents when it has probed its runtimes. A failure
/// shows the error and leaves Start disabled. A remote host is read only after
/// host discovery has registered its route.
fn load_in_background(
    window: slint::Weak<AssistantWindow>,
    dialog: Dialog,
    token: u64,
    project: Option<String>,
    agent: Option<String>,
) {
    std::thread::spawn(move || {
        let host = dialog.host();
        if host != LOCAL_HOST {
            dialog.discovery.wait();
        }
        let early = {
            let (window, dialog, project) = (window.clone(), dialog.clone(), project.clone());
            move |projects: &[ProjectChoice]| {
                let projects = projects.to_vec();
                let delivered = window.upgrade_in_event_loop(move |window| {
                    if !dialog.host_generation.is_current(token) {
                        return;
                    }
                    info!(
                        elapsed_ms = elapsed_ms(dialog.started),
                        projects = projects.len(),
                        "projects loaded"
                    );
                    show_projects(&window, &projects, project.as_deref());
                    lock(&dialog.choices).projects = projects;
                    window.set_projects_ready(true);
                    dialog.refresh_blocker(&window);
                });
                if let Err(err) = delivered {
                    error!(error = %err, "deliver the assistant projects to the UI");
                }
            }
        };
        let outcome = block_on(load_choices_staged(&host, &dialog.daemon, early))
            .map_err(|err| format!("cannot start the background runtime: {err}"))
            .and_then(|loaded| loaded.map_err(|err| err.to_string()));
        if let Err(message) = &outcome {
            error!(host = %host, error = %message, "load assistant choices");
        }
        let delivered = window.upgrade_in_event_loop(move |window| {
            if !dialog.host_generation.is_current(token) {
                return;
            }
            window.set_loading(false);
            info!(
                elapsed_ms = elapsed_ms(dialog.started),
                ok = outcome.is_ok(),
                "choices loaded"
            );
            match outcome {
                Ok(loaded) => {
                    // Projects shown early keep whatever the owner picked since.
                    if !window.get_projects_ready() {
                        show_projects(&window, &loaded.projects, project.as_deref());
                    }
                    show_agents(&window, &loaded, agent.as_deref());
                    *lock(&dialog.choices) = loaded;
                }
                Err(message) => {
                    dialog.load_failed.store(true, Ordering::SeqCst);
                    show_error(&window, &message);
                }
            }
            dialog.refresh_blocker(&window);
        });
        if let Err(err) = delivered {
            error!(error = %err, "deliver the assistant choices to the UI");
        }
    });
}

/// Fills the host select with Local followed by `hosts`.
fn show_hosts(window: &AssistantWindow, hosts: &[DialogHost]) {
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
    window: slint::Weak<AssistantWindow>,
    dialog: Dialog,
    configured: Vec<HostConfig>,
    requested: Option<String>,
    launch_token: u64,
    project: Option<String>,
) {
    std::thread::spawn(move || {
        let _open_gate = FinishOnDrop(dialog.discovery.clone());
        let (hosts, failure) = block_on(discover_dialog_hosts(&dialog.daemon, &configured))
            .unwrap_or_else(|err| {
                (
                    dialog_hosts(&configured, &DiscoveryPlan::default()),
                    Some(format!("cannot start the background runtime: {err}")),
                )
            });
        // Routes are registered; remote loads may proceed.
        dialog.discovery.finish();
        if let Some(message) = failure {
            warn!(error = %message, "host discovery failed; the dialog lists configured hosts only");
        }
        let delivered = window.upgrade_in_event_loop(move |window| {
            info!(
                elapsed_ms = elapsed_ms(dialog.started),
                hosts = hosts.len(),
                "hosts listed"
            );
            let picked_since = !dialog.host_generation.is_current(launch_token);
            let awaited = requested.is_some() && !picked_since;
            let target = requested.filter(|_| !picked_since);
            let mut index = None;
            match resolve_host(target.as_deref(), &hosts, true) {
                Requested::Host(position) => index = Some(position),
                Requested::Refused(message) => {
                    error!(error = %message, "resolve the requested host");
                    *lock(&dialog.host_failure) = Some(message);
                    index = Some(0);
                }
                Requested::Local | Requested::Pending(_) => {}
            }
            let targets = host_targets(&hosts);
            let index = index
                .or_else(|| {
                    // Keep what the owner has selected, wherever it moved to.
                    let current = dialog.host();
                    targets.iter().position(|target| *target == current)
                })
                .unwrap_or(0);
            *lock(&dialog.hosts) = targets.clone();
            show_hosts(&window, &hosts);
            window.set_hosts_loading(false);
            window.set_host_index(i32::try_from(index).unwrap_or(0));
            if awaited {
                *lock(&dialog.host) = targets[index].clone();
                let token = dialog.host_generation.advance();
                *lock(&dialog.choices) = Choices::default();
                window.set_projects_ready(false);
                window.set_loading(true);
                dialog.refresh_blocker(&window);
                load_in_background(window.as_weak(), dialog.clone(), token, project, None);
            }
        });
        if let Err(err) = delivered {
            error!(error = %err, "deliver the discovered hosts to the UI");
        }
    });
}

fn submit_in_background(
    window: slint::Weak<AssistantWindow>,
    dialog: Dialog,
    config: NewSessionConfig,
    form: AssistantForm,
    attach_after: Option<AttachAfter>,
) {
    std::thread::spawn(move || {
        let host = dialog.host();
        let outcome = match block_on(launch_assistant(&host, &dialog.daemon, &config, &form)) {
            Ok(outcome) => outcome,
            Err(err) => Err(CreateError::NotSent(format!(
                "cannot start the background runtime: {err}"
            ))),
        };
        match &outcome {
            Ok(id) => {
                info!(host = %host, session = %id, intent = %form.intent, "assistant started")
            }
            Err(err) => warn!(host = %host, code = err.code(), "assistant start failed"),
        }
        let attached = match (&outcome, &attach_after) {
            (Ok(id), Some(after)) => attach_created(after, &host, id),
            _ => Ok(()),
        };
        if let Err(message) = &attached {
            warn!(error = %message, "attach after the assistant start failed");
        }
        let delivered = window.upgrade_in_event_loop(move |window| match (outcome, attached) {
            (Ok(_), Ok(())) => {
                if let Err(err) = window.hide() {
                    error!(error = %err, "hide the assistant window");
                }
            }
            (Ok(_), Err(message)) => {
                window.set_busy(false);
                window.set_created(true);
                show_error(&window, &message);
            }
            (Err(err), _) => {
                // An unknown outcome or an unconfirmed prompt keeps the window
                // busy: Start stays disabled so the request is not repeated.
                window.set_busy(matches!(
                    err,
                    CreateError::Unconfirmed | CreateError::PromptUnconfirmed
                ));
                show_error(&window, &err.to_string());
                dialog.refresh_blocker(&window);
            }
        });
        if let Err(err) = delivered {
            error!(error = %err, "deliver the assistant outcome to the UI");
        }
    });
}

/// A change callback that recomputes why Start is disabled.
fn refresher(dialog: &Dialog, window: &AssistantWindow) -> impl Fn(i32) + 'static {
    let dialog = dialog.clone();
    let weak = window.as_weak();
    move |_| {
        if let Some(window) = weak.upgrade() {
            dialog.refresh_blocker(&window);
        }
    }
}

pub fn run(args: Args) -> Result<(), slint::PlatformError> {
    let started = Instant::now();
    info!("assistant dialog started");
    let window = AssistantWindow::new()?;
    window.global::<Fuzzy>().on_matches(|query, items| {
        let items: Vec<SharedString> = items.iter().collect();
        let matched: Vec<i32> = fuzzy_matches(query.as_str(), &items)
            .into_iter()
            .filter_map(|index| i32::try_from(index).ok())
            .collect();
        ModelRc::new(VecModel::from(matched))
    });
    slint::set_xdg_app_id(APP_ID)?;

    window.set_intent_names(ModelRc::new(VecModel::from(
        INTENTS
            .iter()
            .map(|intent| SharedString::from(intent_label(*intent)))
            .collect::<Vec<_>>(),
    )));
    if let Some(intent) = args.intent
        && let Some(position) = INTENTS.iter().position(|candidate| *candidate == intent)
    {
        window.set_intent_index(i32::try_from(position).unwrap_or(0));
    }
    show_projects(&window, &[], None);
    show_agents(&window, &Choices::default(), None);
    window.set_blocker(AssistantFormError::Loading.to_string().into());
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
            let dialog = Dialog {
                started,
                hosts: Arc::new(Mutex::new(targets.clone())),
                discovery: gate,
                host_failure: Arc::new(Mutex::new(host_failure)),
                host: Arc::new(Mutex::new(targets[host_index].clone())),
                load_failed: Arc::new(AtomicBool::new(false)),
                host_generation: Arc::new(Generation::default()),
                daemon: config.daemon.clone(),
                choices: Arc::new(Mutex::new(Choices::default())),
            };
            window.set_attach_after_create(config.new_session.attach_after_create);
            let config_path = Config::path().map_err(|err| err.to_string());
            let load_token = dialog.host_generation.advance();
            if waiting_for.is_none() {
                load_in_background(
                    window.as_weak(),
                    dialog.clone(),
                    load_token,
                    args.project.clone(),
                    None,
                );
            }
            if !discovered {
                discover_in_background(
                    window.as_weak(),
                    dialog.clone(),
                    config.hosts.clone(),
                    args.host.clone().filter(|_| waiting_for.is_some()),
                    load_token,
                    args.project.clone(),
                );
            }
            let on_host = dialog.clone();
            let weak = window.as_weak();
            window.on_host_changed(move |index| {
                if let Some(window) = weak.upgrade() {
                    on_host.host_changed(&window, index);
                }
            });
            window.on_project_changed(refresher(&dialog, &window));
            window.on_intent_changed(refresher(&dialog, &window));
            window.on_agent_changed(refresher(&dialog, &window));
            let weak = window.as_weak();
            window.on_submit(move || {
                let Some(window) = weak.upgrade() else { return };
                let form = dialog.form(&window);
                if let Some(blocker) = assistant_blocker(
                    &form,
                    &lock(&dialog.choices),
                    is_remote_host(&dialog.host()),
                    Dialog::activity(&window),
                ) {
                    show_error(&window, &blocker.to_string());
                    return;
                }
                info!(intent = %form.intent, "assistant start requested");
                show_error(&window, "");
                window.set_busy(true);
                dialog.refresh_blocker(&window);
                let attach_after = window.get_attach_after_create().then(|| AttachAfter {
                    settings: config.attach(),
                    config_path: config_path.clone(),
                });
                submit_in_background(
                    weak.clone(),
                    dialog.clone(),
                    config.new_session.clone(),
                    form,
                    attach_after,
                );
            });
        }
        Err(err) => {
            error!(error = %err, "load companion.toml");
            window.set_loading(false);
            window.set_blocker(CONFIG_UNREADABLE.into());
            show_error(&window, &err.to_string());
        }
    }
    let weak = window.as_weak();
    window.on_cancel(move || {
        if let Some(window) = weak.upgrade()
            && let Err(err) = window.hide()
        {
            error!(error = %err, "hide the assistant window");
        }
    });
    info!(elapsed_ms = elapsed_ms(started), "window shown");
    window.run()
}
