//! The setup screen: probes the machine while the local daemon is down,
//! shows what is wrong, and runs the one command the owner confirms.
//! Nothing runs without a click on the action and then on Run.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use pohunek_companion_core::{
    CLIENT_VERSION, Config, Probe, ServiceCheck, SetupAction, SetupConfig, SetupState,
    companion_protocols, derive_setup, locate_cli, run_action, service_check, service_status,
    supports_service_start,
};
use slint::ComponentHandle;
use tracing::{info, warn};

use crate::AppWindow;

/// Where the release archives are.
const RELEASES_URL: &str = "https://github.com/zajca/pohunek/releases";

/// Manual install commands from the Pohunek README ("Install manually").
#[cfg(target_os = "macos")]
const INSTALL_COMMANDS: &str = "brew install zajca/pohunek/pohunek\npohunek service install";
#[cfg(not(target_os = "macos"))]
const INSTALL_COMMANDS: &str = "./packaging/install-daemon.sh";

#[cfg(target_os = "macos")]
const INSTALL_STEP: &str = "Install it with Homebrew, then register the login service:";
#[cfg(not(target_os = "macos"))]
const INSTALL_STEP: &str =
    "Unpack the Linux daemon archive of a release, then run this inside the unpacked directory:";

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
    start_supported: Option<(PathBuf, bool)>,
}

struct Confirming {
    action: SetupAction,
    cli: PathBuf,
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
        }
    }

    /// First click: show the exact command and what it touches, from the
    /// read-only `service check`. Runs nothing that changes the machine.
    fn start_confirmation(self: &Arc<Self>) {
        let (action, cli) = {
            let mut inner = self.lock();
            let Some(state) = inner.state.clone() else {
                return;
            };
            let (Some(action), Some(cli)) = (state.action(), state.cli()) else {
                return;
            };
            if inner.busy {
                return;
            }
            inner.busy = true;
            inner.result.clear();
            inner.error.clear();
            (action, cli.to_owned())
        };
        self.update(|_| {});
        let setup = self.clone();
        std::thread::spawn(move || {
            let checked = service_check(
                &cli,
                setup.config.probe_timeout(),
                setup.config.message_max_chars,
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
            info!(action = ?action, can_run, "setup action previewed");
            setup.update(|inner| {
                inner.busy = false;
                inner.confirming = Some(Confirming {
                    action,
                    cli,
                    can_run,
                    preview,
                });
            });
        });
    }

    /// Second click: runs the confirmed command and shows its outcome.
    fn run_confirmed(self: &Arc<Self>) {
        let (action, cli) = {
            let mut inner = self.lock();
            let Some(confirming) = inner.confirming.as_ref().filter(|c| c.can_run) else {
                return;
            };
            if inner.busy {
                return;
            }
            let picked = (confirming.action, confirming.cli.clone());
            inner.busy = true;
            picked
        };
        self.update(|_| {});
        info!(action = ?action, "running the confirmed setup command");
        let setup = self.clone();
        std::thread::spawn(move || {
            let outcome = run_action(
                &cli,
                action,
                setup.config.action_timeout(),
                setup.config.message_max_chars,
            );
            match &outcome {
                Ok(_) => info!(action = ?action, "setup command succeeded"),
                Err(err) => warn!(action = ?action, error = %err, "setup command failed"),
            }
            setup.update(|inner| {
                inner.busy = false;
                inner.confirming = None;
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
        SetupState::CliMissing { program } => {
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
            view.body = format!(
                "Pohunek Companion shows the sessions of a Pohunek daemon, and {searched} \
                 {INSTALL_STEP}"
            );
            view.commands = INSTALL_COMMANDS.to_owned();
            view.detail = format!(
                "Release archives: {RELEASES_URL}. This screen updates when the command appears."
            );
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
