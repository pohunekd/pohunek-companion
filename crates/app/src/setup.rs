//! The setup screen: probes the machine while the local daemon is down,
//! shows what is wrong, and runs the one command the owner confirms.
//! Nothing that changes the machine runs without a click on the action and
//! then on Run; the install downloads and verifies into a private temporary
//! directory first and shows what it found.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use pohunek_companion_core::{
    CLIENT_VERSION, Config, InstallPlan, Prepared, Probe, Release, ServiceCheck, SetupAction,
    SetupConfig, SetupState, companion_protocols, derive_setup, locate_cli, plan_for_host,
    prepare_release, run_action, run_brew_install, run_release_install, service_check,
    service_status, supports_service_start,
};
use slint::ComponentHandle;
use tracing::{info, warn};

use crate::AppWindow;

/// The Pohunek install documentation.
const INSTALL_DOCS_URL: &str = "https://github.com/zajca/pohunek/blob/main/docs/install.md";

/// Manual macOS install commands from the Pohunek README ("Install manually").
const MANUAL_COMMANDS: &str = "brew install zajca/pohunek/pohunek\npohunek service install";

/// Shown after an install; these commands are never run by the companion.
const NEXT_STEPS: &str = "Next steps, in a terminal: `pohunek assistant setup` and \
                          `pohunek integration install`.";

/// What the daemon connection loop last reported for the local host.
#[derive(Debug, Clone, Default)]
struct Down {
    reason: String,
    code: Option<String>,
}

#[derive(Default)]
struct Inner {
    /// The local daemon answered at least once in this process.
    connected: bool,
    /// Some other host already lists sessions.
    rows_elsewhere: bool,
    down: Option<Down>,
    state: Option<SetupState>,
    probing: bool,
    /// A confirmed command is running or waiting for its confirmation.
    busy: bool,
    confirming: Option<Confirming>,
    result: String,
    error: String,
    /// The step an install is on; empty when idle.
    progress: String,
    start_supported: Option<(PathBuf, bool)>,
}

/// What Run executes.
enum Target {
    /// A `pohunek` CLI command.
    Cli(PathBuf),
    /// The verified, unpacked release; dropping it removes the download.
    Release(Box<Prepared>),
    Brew(PathBuf),
}

struct Confirming {
    action: SetupAction,
    target: Target,
    /// The check passed; Run is enabled.
    can_run: bool,
    preview: String,
}

/// Shared between the daemon loop thread and the UI thread.
pub struct Setup {
    config: SetupConfig,
    pohunek_bin: String,
    login_shell_timeout: Duration,
    window: slint::Weak<AppWindow>,
    inner: Mutex<Inner>,
}

impl Setup {
    pub fn new(window: &AppWindow, config: &Config) -> Arc<Self> {
        let setup = Arc::new(Self {
            config: config.setup.clone(),
            pohunek_bin: config.pohunek_bin.clone(),
            login_shell_timeout: Duration::from_millis(config.login_shell_timeout_ms),
            window: window.as_weak(),
            inner: Mutex::default(),
        });
        {
            let setup = setup.clone();
            window.on_setup_action(move || setup.start_confirmation());
        }
        {
            let setup = setup.clone();
            window.on_setup_cancel(move || {
                setup.update(|inner| inner.confirming = None);
            });
        }
        {
            let setup = setup.clone();
            window.on_setup_run(move || setup.run_confirmed());
        }
        {
            let setup = setup.clone();
            window.on_setup_recheck(move || {
                info!("setup screen re-check requested");
                setup.probe();
            });
        }
        setup
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Mutates the state and redraws.
    fn update(&self, change: impl FnOnce(&mut Inner)) {
        let view = {
            let mut inner = self.lock();
            change(&mut inner);
            View::of(&inner)
        };
        self.show(view);
    }

    fn show(&self, view: View) {
        if let Err(err) = self.window.upgrade_in_event_loop(move |window| {
            window.set_setup_open(view.open);
            window.set_setup_heading(view.heading.into());
            window.set_setup_body(view.body.into());
            window.set_setup_commands(view.commands.into());
            window.set_setup_detail(view.detail.into());
            window.set_setup_action_label(view.action_label.into());
            window.set_setup_confirming(view.confirming);
            window.set_setup_preview(view.preview.into());
            window.set_setup_can_run(view.can_run);
            window.set_setup_busy(view.busy);
            window.set_setup_progress(view.progress.into());
            window.set_setup_result(view.result.into());
            window.set_setup_error(view.error.into());
        }) {
            warn!(error = %err, "deliver the setup screen to the UI");
        }
    }

    /// The local daemon answered: the list takes over.
    pub fn connected(&self) {
        self.update(|inner| {
            inner.connected = true;
            inner.down = None;
            inner.state = None;
            inner.confirming = None;
            inner.result.clear();
            inner.error.clear();
            inner.progress.clear();
        });
    }

    /// The local daemon failed once more; the connection loop's backoff
    /// paces these calls.
    pub fn down(self: &Arc<Self>, reason: String, code: Option<String>) {
        let first = {
            let mut inner = self.lock();
            let first = inner.down.is_none();
            inner.down = Some(Down { reason, code });
            first
        };
        if first {
            info!("local daemon unreachable, probing the machine for the setup screen");
        }
        self.probe();
    }

    /// Whether some host other than the local one lists sessions; the setup
    /// screen then stays out of the way.
    pub fn rows_elsewhere(&self, present: bool) {
        self.update(|inner| inner.rows_elsewhere = present);
    }

    /// Reads the machine state on a worker thread and redraws.
    fn probe(self: &Arc<Self>) {
        let down = {
            let mut inner = self.lock();
            if inner.probing || inner.busy {
                return;
            }
            let Some(down) = inner.down.clone() else {
                return;
            };
            inner.probing = true;
            down
        };
        let setup = self.clone();
        std::thread::spawn(move || {
            let probe = setup.read_machine(&down);
            let state = derive_setup(&probe);
            let key = state.key();
            setup.update(|inner| {
                inner.probing = false;
                if inner.down.is_some() {
                    inner.state = Some(state);
                }
            });
            info!(state = key, "setup screen state");
        });
    }

    fn read_machine(&self, down: &Down) -> Probe {
        let probe_timeout = self.config.probe_timeout();
        let max = self.config.message_max_chars;
        let cli = locate_cli(&self.pohunek_bin, self.login_shell_timeout);
        let install = if cli.is_err() {
            let brew = locate_cli("brew", self.login_shell_timeout).ok();
            plan_for_host(brew)
        } else {
            InstallPlan::Unsupported {
                reason: String::new(),
            }
        };
        let status = cli
            .as_ref()
            .ok()
            .map(|cli| service_status(cli, probe_timeout, max));
        let start_supported = match (&cli, &status) {
            (Ok(cli), Some(Ok(status))) if status.installed => {
                let cached = self
                    .lock()
                    .start_supported
                    .clone()
                    .filter(|(path, _)| path == cli);
                match cached {
                    Some((_, supported)) => supported,
                    None => {
                        let supported = supports_service_start(cli, probe_timeout);
                        self.lock().start_supported = Some((cli.clone(), supported));
                        supported
                    }
                }
            }
            _ => false,
        };
        Probe {
            refusal: down.code.clone().map(|code| (code, down.reason.clone())),
            reason: down.reason.clone(),
            cli,
            status,
            start_supported,
            install,
        }
    }

    /// First click: show the exact command and what it touches. For a CLI
    /// command that is the read-only `service check`; for the release install
    /// it is the download, verification and unpacking into a private
    /// temporary directory, and then the unpacked CLI's `service check`.
    /// Nothing outside that directory changes.
    fn start_confirmation(self: &Arc<Self>) {
        let (action, state) = {
            let mut inner = self.lock();
            let Some(state) = inner.state.clone() else {
                return;
            };
            let Some(action) = state.action() else {
                return;
            };
            if inner.busy {
                return;
            }
            inner.busy = true;
            inner.result.clear();
            inner.error.clear();
            inner.progress.clear();
            (action, state)
        };
        self.update(|_| {});
        let setup = self.clone();
        std::thread::spawn(move || {
            let outcome = setup.preview(action, &state);
            match &outcome {
                Ok(confirming) => {
                    info!(action = ?action, can_run = confirming.can_run, "setup action previewed");
                }
                Err(err) => warn!(action = ?action, error = %err, "setup action preview failed"),
            }
            setup.update(|inner| {
                inner.busy = false;
                inner.progress.clear();
                match outcome {
                    Ok(confirming) => inner.confirming = Some(confirming),
                    Err(err) => inner.error = err,
                }
            });
        });
    }

    fn preview(&self, action: SetupAction, state: &SetupState) -> Result<Confirming, String> {
        match (action, state) {
            (
                SetupAction::InstallRelease,
                SetupState::CliMissing {
                    install: InstallPlan::Release,
                    ..
                },
            ) => {
                let gh = locate_cli("gh", self.login_shell_timeout).ok();
                let prepared =
                    prepare_release(&self.config, &Release::pinned(), gh.as_deref(), &|step| {
                        self.update(|inner| inner.progress = step)
                    })
                    .map_err(|err| format!("{err}"))?;
                Ok(Confirming {
                    action,
                    can_run: true,
                    preview: prepared.preview(),
                    target: Target::Release(Box::new(prepared)),
                })
            }
            (
                SetupAction::BrewInstall,
                SetupState::CliMissing {
                    install: InstallPlan::Brew { brew },
                    ..
                },
            ) => Ok(Confirming {
                action,
                can_run: true,
                preview: format!(
                    "$ {}\n\nHomebrew installs the formula's current release, which can be \
                     newer than the {} this companion is built against. The login service is \
                     registered afterwards with a separate confirmation.",
                    action.command_line(),
                    Release::pinned().tag
                ),
                target: Target::Brew(brew.clone()),
            }),
            (_, state) => {
                let cli = state
                    .cli()
                    .ok_or_else(|| "no pohunek command to run".to_owned())?
                    .to_owned();
                let checked = service_check(
                    &cli,
                    self.config.probe_timeout(),
                    self.config.message_max_chars,
                );
                let (can_run, preview) = match checked {
                    Ok(check) => (true, preview_text(action, &check)),
                    Err(err) => (
                        false,
                        format!(
                            "$ {}\n\nThe pre-install check refused it:\n{err}",
                            action.command_line()
                        ),
                    ),
                };
                Ok(Confirming {
                    action,
                    target: Target::Cli(cli),
                    can_run,
                    preview,
                })
            }
        }
    }

    /// Second click: runs the confirmed command and shows its outcome.
    fn run_confirmed(self: &Arc<Self>) {
        let confirming = {
            let mut inner = self.lock();
            if inner.busy || !inner.confirming.as_ref().is_some_and(|c| c.can_run) {
                return;
            }
            let Some(confirming) = inner.confirming.take() else {
                return;
            };
            inner.busy = true;
            inner.progress = "Running ".to_owned() + &confirming.action.command_line();
            confirming
        };
        self.update(|_| {});
        let action = confirming.action;
        info!(action = ?action, "running the confirmed setup command");
        let setup = self.clone();
        std::thread::spawn(move || {
            let max = setup.config.message_max_chars;
            let outcome = match &confirming.target {
                Target::Cli(cli) => run_action(cli, action, setup.config.action_timeout(), max),
                Target::Release(prepared) => {
                    run_release_install(prepared, setup.config.install_timeout(), max)
                        .map(|done| format!("{done} {NEXT_STEPS}"))
                        .map_err(|err| err.to_string())
                }
                Target::Brew(brew) => run_brew_install(brew, setup.config.install_timeout(), max)
                    .map(|done| format!("{done} {NEXT_STEPS}"))
                    .map_err(|err| err.to_string()),
            };
            // The unpacked release and its download go away here.
            drop(confirming);
            match &outcome {
                Ok(_) => info!(action = ?action, "setup command succeeded"),
                Err(err) => warn!(action = ?action, error = %err, "setup command failed"),
            }
            setup.update(|inner| {
                inner.busy = false;
                inner.progress.clear();
                match outcome {
                    Ok(done) => {
                        inner.result = format!("{done} Waiting for the daemon.");
                        inner.error.clear();
                    }
                    Err(err) => {
                        inner.result.clear();
                        inner.error = err;
                    }
                }
            });
            setup.probe();
        });
    }
}

fn preview_text(action: SetupAction, check: &ServiceCheck) -> String {
    format!("$ {}\n\n{}", action.command_line(), check.summary(action))
}

/// Everything the window shows, derived from [`Inner`].
struct View {
    open: bool,
    heading: String,
    body: String,
    commands: String,
    detail: String,
    action_label: String,
    confirming: bool,
    preview: String,
    can_run: bool,
    busy: bool,
    progress: String,
    result: String,
    error: String,
}

impl View {
    fn of(inner: &Inner) -> Self {
        let mut view = Self {
            open: false,
            heading: String::new(),
            body: String::new(),
            commands: String::new(),
            detail: String::new(),
            action_label: String::new(),
            confirming: false,
            preview: String::new(),
            can_run: false,
            busy: inner.busy,
            progress: inner.progress.clone(),
            result: inner.result.clone(),
            error: inner.error.clone(),
        };
        let Some(state) = inner
            .state
            .as_ref()
            .filter(|_| inner.down.is_some() && !inner.connected && !inner.rows_elsewhere)
        else {
            return view;
        };
        view.open = true;
        describe(state, &mut view);
        if let Some(confirming) = &inner.confirming {
            view.confirming = true;
            view.preview = confirming.preview.clone();
            view.can_run = confirming.can_run;
        }
        view
    }
}

fn describe(state: &SetupState, view: &mut View) {
    match state {
        SetupState::CliMissing { program, install } => {
            view.heading = "Pohunek is not installed".to_owned();
            let searched = if program.contains('/') {
                format!(
                    "`{program}` (the `pohunek_bin` setting) does not exist or is not executable."
                )
            } else {
                format!(
                    "no `{program}` command was found on PATH, in ~/.local/bin, /opt/homebrew/bin \
                     or /usr/local/bin."
                )
            };
            let tag = Release::pinned().tag;
            let what =
                format!("Pohunek Companion shows the sessions of a Pohunek daemon, and {searched}");
            match install {
                InstallPlan::Release => {
                    view.action_label = SetupAction::InstallRelease.label().to_owned();
                    view.body = format!(
                        "{what} Install the Pohunek release {tag}, the one this companion is built against."
                    );
                    view.detail = format!(
                        "Install downloads the Linux daemon archive of {tag} from GitHub over \
                         HTTPS into a private temporary directory, checks its sha256 (and its \
                         attestation when gh is signed in), unpacks it, and shows what it will \
                         install. Nothing is installed until you press Run."
                    );
                }
                InstallPlan::Brew { .. } => {
                    view.action_label = SetupAction::BrewInstall.label().to_owned();
                    view.body = format!("{what} Install Pohunek with Homebrew.");
                    view.detail = format!(
                        "This companion is built against {tag}; Homebrew installs the formula's \
                         current release. The login service is registered in a second step. \
                         {NEXT_STEPS}"
                    );
                }
                InstallPlan::Manual => {
                    view.body = format!(
                        "{what} Homebrew was not found, so Install is not offered. Install \
                         Homebrew from https://brew.sh, or follow the install guide, then run:"
                    );
                    view.commands = MANUAL_COMMANDS.to_owned();
                    view.detail = format!(
                        "Install guide: {INSTALL_DOCS_URL}. This screen updates when the command appears."
                    );
                }
                InstallPlan::Unsupported { reason } => {
                    view.body = format!(
                        "{what} This platform is not supported by the installer: {reason}."
                    );
                    view.detail = format!(
                        "Install guide: {INSTALL_DOCS_URL}. This screen updates when the command appears."
                    );
                }
            }
        }
        SetupState::NotInstalled { cli } => {
            view.heading = "The Pohunek service is not installed".to_owned();
            view.body = format!(
                "`{}` is available, but the Pohunek daemon is not registered as a login service.",
                cli.display()
            );
            view.action_label = SetupAction::InstallService.label().to_owned();
        }
        SetupState::NotRunning {
            cli,
            daemon_state,
            job,
            start_supported,
        } => {
            view.heading = "The Pohunek daemon is not running".to_owned();
            view.body = format!(
                "The login service is installed, and the service manager reports the daemon as \
                 {daemon_state}."
            );
            if *start_supported {
                view.action_label = SetupAction::StartService.label().to_owned();
            } else {
                view.detail = format!(
                    "`{}` has no `service start` command. Start the daemon job {} with your \
                     service manager, or log out and in again.",
                    cli.display(),
                    if job.is_empty() {
                        "of the service"
                    } else {
                        job
                    }
                );
            }
        }
        SetupState::NotAnswering { reason, .. } => {
            view.heading = "The Pohunek daemon does not answer".to_owned();
            view.body = "The service manager reports the daemon as running, but connecting to it \
                         fails."
                .to_owned();
            view.detail = reason.clone();
        }
        SetupState::Busy { .. } => {
            view.heading = "A service install or upgrade is unfinished".to_owned();
            view.body = "Run `pohunek service status` in a terminal to see the interrupted step, \
                         then resume it by repeating the command or roll it back."
                .to_owned();
        }
        SetupState::VersionMismatch { code, message } => {
            view.heading = "Pohunek and the companion do not match".to_owned();
            view.body = format!("The daemon refused the connection ({code}): {message}");
            view.detail = format!(
                "This companion uses pohunek-client v{CLIENT_VERSION} (protocol {}). Upgrade the \
                 older side so that the protocol ranges overlap.",
                companion_protocols()
            );
        }
        SetupState::StatusFailed { cli, message } => {
            view.heading = "Cannot read the Pohunek service state".to_owned();
            view.body = format!("`{} service status` failed.", cli.display());
            view.detail = message.clone();
        }
    }
}
