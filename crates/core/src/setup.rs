//! What the setup screen shows while the local daemon is down: where the
//! `pohunek` CLI is, what it says about the login service, and which single
//! command (if any) the owner may confirm. No UI dependency; every command
//! runs only when the caller asks for it.

use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use pohunek_client::protocol::CLIENT_PROTOCOL_VERSIONS;
use rustix::process::{Pid, Signal, kill_process_group};
use serde::Deserialize;

use crate::attach::resolve_program;
use crate::attach::resolve_program_in;
use crate::badge::{clip_display, sanitize};
use crate::install::{InstallPlan, brew_command_line};

/// Daemon error codes that mean the daemon is up but speaks another protocol.
const VERSION_CODES: [&str; 2] = ["version_mismatch", "daemon_protocol_too_old"];
/// Directories a GUI launched from the desktop may lack on `PATH`; `~` is the
/// owner's home directory.
const FALLBACK_DIRS: [&str; 3] = ["~/.local/bin", "/opt/homebrew/bin", "/usr/local/bin"];
/// Daemon job states for which the service manager is already running it.
const RUNNING_STATE: &str = "running";
/// Upper bound of the output kept from one command.
const OUTPUT_LIMIT: u64 = 1 << 20;
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// The `pohunek` version this build's client library was pinned to.
pub const CLIENT_VERSION: &str = env!("POHUNEK_CLIENT_VERSION");

/// A command the owner can confirm. Each one maps to exactly one argv; there
/// is no way to build `integration install`, `--accept-runtime-loss`,
/// `--stop-sessions` or `--purge` from it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetupAction {
    /// Downloads, verifies and unpacks the pinned release, then runs its
    /// `packaging/install-daemon.sh` (Linux).
    InstallRelease,
    /// `brew install zajca/pohunek/pohunek` (macOS).
    BrewInstall,
    /// `pohunek service install`: registers the login service.
    InstallService,
    /// `pohunek service start`: starts an installed, stopped service. Offered
    /// only when the CLI lists the command.
    StartService,
}

impl SetupAction {
    /// The arguments after the `pohunek` CLI path; `None` for the actions
    /// that install the CLI itself.
    pub fn args(self) -> Option<&'static [&'static str]> {
        match self {
            Self::InstallService => Some(&["service", "install"]),
            Self::StartService => Some(&["service", "start"]),
            Self::InstallRelease | Self::BrewInstall => None,
        }
    }

    /// The button label.
    pub fn label(self) -> &'static str {
        match self {
            Self::InstallRelease | Self::BrewInstall => "Install Pohunek",
            Self::InstallService => "Install service",
            Self::StartService => "Start service",
        }
    }

    /// The command as the owner would type it.
    pub fn command_line(self) -> String {
        match self.args() {
            Some(args) => format!("pohunek {}", args.join(" ")),
            None if self == Self::BrewInstall => brew_command_line(),
            None => "./packaging/install-daemon.sh".to_owned(),
        }
    }
}

/// What the screen shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetupState {
    /// No `pohunek` executable was found; `install` is how this machine can
    /// get one.
    CliMissing {
        program: String,
        install: InstallPlan,
    },
    /// The CLI is there; the login service is not installed.
    NotInstalled { cli: PathBuf },
    /// The service is installed and its daemon is not running.
    NotRunning {
        cli: PathBuf,
        daemon_state: String,
        /// The service manager's name for the daemon job.
        job: String,
        start_supported: bool,
    },
    /// The service manager reports the daemon running, yet it does not answer.
    NotAnswering { cli: PathBuf, reason: String },
    /// An install or upgrade transaction is interrupted or in progress.
    Busy { cli: PathBuf },
    /// The daemon answered with a protocol it shares no version with.
    VersionMismatch { code: String, message: String },
    /// The CLI could not report the service state.
    StatusFailed { cli: PathBuf, message: String },
}

impl SetupState {
    /// The action the screen offers, if any.
    pub fn action(&self) -> Option<SetupAction> {
        match self {
            Self::CliMissing {
                install: InstallPlan::Release,
                ..
            } => Some(SetupAction::InstallRelease),
            Self::CliMissing {
                install: InstallPlan::Brew { .. },
                ..
            } => Some(SetupAction::BrewInstall),
            Self::NotInstalled { .. } => Some(SetupAction::InstallService),
            Self::NotRunning {
                start_supported: true,
                ..
            } => Some(SetupAction::StartService),
            _ => None,
        }
    }

    /// The CLI to run the action with.
    pub fn cli(&self) -> Option<&Path> {
        match self {
            Self::NotInstalled { cli }
            | Self::NotRunning { cli, .. }
            | Self::NotAnswering { cli, .. }
            | Self::Busy { cli }
            | Self::StatusFailed { cli, .. } => Some(cli),
            Self::CliMissing { .. } | Self::VersionMismatch { .. } => None,
        }
    }

    /// Stable key for the UI and the logs.
    pub fn key(&self) -> &'static str {
        match self {
            Self::CliMissing { .. } => "cli-missing",
            Self::NotInstalled { .. } => "not-installed",
            Self::NotRunning { .. } => "not-running",
            Self::NotAnswering { .. } => "not-answering",
            Self::Busy { .. } => "busy",
            Self::VersionMismatch { .. } => "version-mismatch",
            Self::StatusFailed { .. } => "status-failed",
        }
    }
}

/// `pohunek service status --json`, the fields the screen reads.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct ServiceStatus {
    pub installed: bool,
    pub daemon: Option<JobStatus>,
    pub pending_transaction: Option<serde_json::Value>,
    pub transaction_in_progress: bool,
}

/// The daemon job inside [`ServiceStatus`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct JobStatus {
    pub state: String,
    pub service_id: String,
}

/// `pohunek service check --json`, the fields shown before a confirmation.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct ServiceCheck {
    pub operation: String,
    pub version: String,
    pub prefix: Option<PathBuf>,
    pub namespace: String,
    pub config_path: Option<PathBuf>,
    pub warnings: Vec<String>,
}

impl ServiceCheck {
    /// What `action` will touch, one fact per line. The check itself is
    /// read-only and describes the install (or upgrade) of this CLI's
    /// version; the action's own effect is named here.
    pub fn summary(&self, action: SetupAction) -> String {
        let mut lines = vec![format!(
            "Check passed: {} of pohunek {}, namespace {}.",
            self.operation, self.version, self.namespace
        )];
        match action {
            SetupAction::InstallService | SetupAction::InstallRelease => {
                if let Some(prefix) = &self.prefix {
                    lines.push(format!("Copies the binaries under {}.", prefix.display()));
                }
                if let Some(config) = &self.config_path {
                    lines.push(format!("Writes {}.", config.display()));
                }
                lines.push("Registers the daemon as a login service and starts it.".to_owned());
            }
            SetupAction::BrewInstall => {
                lines.push("Homebrew installs the formula's current release.".to_owned());
            }
            SetupAction::StartService => {
                lines.push(
                    "Asks the service manager to start the installed daemon job. Nothing is \
                     installed and no file is written."
                        .to_owned(),
                );
            }
        }
        lines.extend(self.warnings.iter().map(|w| format!("Warning: {w}")));
        lines.join("\n")
    }
}

/// A failure the CLI reported in its JSON envelope.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct CliFailure {
    pub code: String,
    #[serde(default)]
    pub msg: String,
}

impl CliFailure {
    /// `code: message`, cleaned of control characters and clipped.
    pub fn display(&self, max_chars: usize) -> String {
        clip_display(
            &sanitize(&format!("{}: {}", self.code, self.msg)),
            max_chars,
        )
    }
}

#[derive(Deserialize)]
struct Envelope<T> {
    ok: Option<T>,
    err: Option<CliFailure>,
}

/// Why a CLI call produced no payload.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CallError {
    #[error("{0}")]
    Run(String),
    #[error("{0}")]
    Reported(String),
}

/// Where the CLI is: `program` as configured (`pohunek_bin`), then the usual
/// install locations. `Err` carries `program` for the message.
pub fn locate_cli(program: &str, login_shell_timeout: Duration) -> Result<PathBuf, String> {
    if let Ok(found) = resolve_program(program, login_shell_timeout) {
        return Ok(found);
    }
    if program.contains('/') {
        return Err(program.to_owned());
    }
    let home = std::env::var_os("HOME");
    let dirs: Vec<PathBuf> = FALLBACK_DIRS
        .iter()
        .filter_map(|dir| match dir.strip_prefix("~/") {
            Some(rest) => home
                .as_deref()
                .filter(|home| !home.is_empty())
                .map(|home| Path::new(home).join(rest)),
            None => Some(PathBuf::from(dir)),
        })
        .collect();
    resolve_program_in(program, &dirs).map_err(|_| program.to_owned())
}

/// Captured result of one CLI call.
#[derive(Debug)]
pub(crate) struct Captured {
    pub(crate) success: bool,
    pub(crate) stdout: String,
    pub(crate) stderr: String,
}

/// Runs `cli args...` with no stdin, bounded by `timeout`; the whole process
/// group is killed on expiry. The environment is inherited because the
/// service manager needs the session bus.
pub(crate) fn run(cli: &Path, args: &[&str], timeout: Duration) -> Result<Captured, String> {
    let mut child = Command::new(cli)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .map_err(|err| format!("cannot start {}: {err}", cli.display()))?;
    let drain = |pipe: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut text = String::new();
            if let Some(pipe) = pipe {
                let _ = pipe.take(OUTPUT_LIMIT).read_to_string(&mut text);
            }
            text
        })
    };
    let stdout = drain(child.stdout.take().map(|p| Box::new(p) as _));
    let stderr = drain(child.stderr.take().map(|p| Box::new(p) as _));
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(POLL_INTERVAL),
            Ok(None) => {
                if let Some(pid) = i32::try_from(child.id()).ok().and_then(Pid::from_raw) {
                    let _ = kill_process_group(pid, Signal::KILL);
                }
                let _ = child.wait();
                return Err(format!(
                    "`pohunek {}` did not finish within {} s",
                    args.join(" "),
                    timeout.as_secs()
                ));
            }
            Err(err) => return Err(format!("cannot wait for pohunek: {err}")),
        }
    };
    Ok(Captured {
        success: status.success(),
        stdout: stdout.join().unwrap_or_default(),
        stderr: stderr.join().unwrap_or_default(),
    })
}

fn parse_envelope<T: for<'de> Deserialize<'de>>(
    captured: &Captured,
    max_chars: usize,
) -> Result<T, CallError> {
    match serde_json::from_str::<Envelope<T>>(&captured.stdout) {
        Ok(Envelope {
            ok: Some(payload), ..
        }) => Ok(payload),
        Ok(Envelope {
            err: Some(failure), ..
        }) => Err(CallError::Reported(failure.display(max_chars))),
        _ => {
            let text = if captured.stderr.trim().is_empty() {
                &captured.stdout
            } else {
                &captured.stderr
            };
            Err(CallError::Run(clip_display(&sanitize(text), max_chars)))
        }
    }
}

/// `pohunek service status --json`.
pub fn service_status(
    cli: &Path,
    timeout: Duration,
    max_chars: usize,
) -> Result<ServiceStatus, CallError> {
    let captured = run(cli, &["service", "status", "--json"], timeout).map_err(CallError::Run)?;
    parse_envelope(&captured, max_chars)
}

/// `pohunek service check --json`: the read-only preview of an install.
pub fn service_check(
    cli: &Path,
    timeout: Duration,
    max_chars: usize,
) -> Result<ServiceCheck, CallError> {
    let captured = run(cli, &["service", "check", "--json"], timeout).map_err(CallError::Run)?;
    parse_envelope(&captured, max_chars)
}

/// Whether `pohunek service --help` lists the `start` subcommand.
pub fn supports_service_start(cli: &Path, timeout: Duration) -> bool {
    run(cli, &["service", "--help"], timeout)
        .map(|out| lists_subcommand(&out.stdout, "start"))
        .unwrap_or(false)
}

fn lists_subcommand(help: &str, name: &str) -> bool {
    let mut in_commands = false;
    for line in help.lines() {
        if line.trim_end() == "Commands:" {
            in_commands = true;
        } else if in_commands && !line.starts_with(' ') {
            break;
        } else if in_commands && line.split_whitespace().next() == Some(name) {
            return true;
        }
    }
    false
}

/// Runs a confirmed action. `Ok` is the command's output (short, cleaned);
/// `Err` is its error, verbatim from the CLI's envelope when it gave one.
pub fn run_action(
    cli: &Path,
    action: SetupAction,
    timeout: Duration,
    max_chars: usize,
) -> Result<String, String> {
    let Some(base) = action.args() else {
        return Err(format!(
            "`{}` is not a pohunek command",
            action.command_line()
        ));
    };
    let mut args: Vec<&str> = base.to_vec();
    args.push("--json");
    let captured = run(cli, &args, timeout)?;
    match serde_json::from_str::<Envelope<serde_json::Value>>(&captured.stdout) {
        Ok(Envelope {
            err: Some(failure), ..
        }) => Err(failure.display(max_chars)),
        Ok(Envelope { ok: Some(_), .. }) if captured.success => {
            Ok(format!("`{}` finished.", action.command_line()))
        }
        _ => {
            let text = if captured.stderr.trim().is_empty() {
                &captured.stdout
            } else {
                &captured.stderr
            };
            Err(clip_display(&sanitize(text), max_chars))
        }
    }
}

/// What the app learned about the machine.
#[derive(Debug, Clone)]
pub struct Probe {
    /// The daemon's protocol error code and message, when it refused the
    /// connection with one.
    pub refusal: Option<(String, String)>,
    /// The connection error text.
    pub reason: String,
    /// The CLI lookup.
    pub cli: Result<PathBuf, String>,
    /// The service state; `None` when the CLI is missing or was not asked.
    pub status: Option<Result<ServiceStatus, CallError>>,
    /// Whether the CLI offers `service start`.
    pub start_supported: bool,
    /// How this machine can install the CLI.
    pub install: InstallPlan,
}

/// Decides what to show from plain facts.
pub fn derive(probe: &Probe) -> SetupState {
    if let Some((code, message)) = &probe.refusal
        && VERSION_CODES.contains(&code.as_str())
    {
        return SetupState::VersionMismatch {
            code: code.clone(),
            message: message.clone(),
        };
    }
    let cli = match &probe.cli {
        Ok(cli) => cli.clone(),
        Err(program) => {
            return SetupState::CliMissing {
                program: program.clone(),
                install: probe.install.clone(),
            };
        }
    };
    let status = match &probe.status {
        Some(Ok(status)) => status,
        Some(Err(err)) => {
            return SetupState::StatusFailed {
                cli,
                message: err.to_string(),
            };
        }
        None => {
            return SetupState::StatusFailed {
                cli,
                message: "the service state was not read".to_owned(),
            };
        }
    };
    if status.pending_transaction.is_some() || status.transaction_in_progress {
        return SetupState::Busy { cli };
    }
    if !status.installed {
        return SetupState::NotInstalled { cli };
    }
    let (daemon_state, job) = status.daemon.as_ref().map_or_else(
        || ("unknown".to_owned(), String::new()),
        |job| (job.state.clone(), job.service_id.clone()),
    );
    if daemon_state == RUNNING_STATE {
        return SetupState::NotAnswering {
            cli,
            reason: probe.reason.clone(),
        };
    }
    SetupState::NotRunning {
        cli,
        daemon_state,
        job,
        start_supported: probe.start_supported,
    }
}

/// The protocol range this build speaks, as `min to max`.
pub fn companion_protocols() -> String {
    format!(
        "{} to {}",
        CLIENT_PROTOCOL_VERSIONS.minimum().get(),
        CLIENT_PROTOCOL_VERSIONS.maximum().get()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe(cli: Result<&str, &str>, status: Option<ServiceStatus>) -> Probe {
        Probe {
            refusal: None,
            reason: "no socket".to_owned(),
            cli: cli.map(PathBuf::from).map_err(str::to_owned),
            status: status.map(Ok),
            start_supported: false,
            install: InstallPlan::Release,
        }
    }

    fn installed(state: &str) -> ServiceStatus {
        ServiceStatus {
            installed: true,
            daemon: Some(JobStatus {
                state: state.to_owned(),
                service_id: "pohunek-x-daemon.service".to_owned(),
            }),
            ..Default::default()
        }
    }

    #[test]
    fn version_refusal_wins_over_everything() {
        let mut p = probe(Err("pohunek"), None);
        p.refusal = Some(("version_mismatch".to_owned(), "ranges".to_owned()));
        assert_eq!(derive(&p).key(), "version-mismatch");
        p.refusal = Some(("socket_refused".to_owned(), "x".to_owned()));
        assert_eq!(derive(&p).key(), "cli-missing");
        assert_eq!(derive(&p).action(), Some(SetupAction::InstallRelease));
        p.install = InstallPlan::Manual;
        assert_eq!(derive(&p).action(), None);
    }

    #[test]
    fn service_states_map_to_screens_and_actions() {
        let missing = derive(&probe(Ok("/bin/pohunek"), Some(ServiceStatus::default())));
        assert_eq!(missing.action(), Some(SetupAction::InstallService));

        let stopped = derive(&probe(Ok("/bin/pohunek"), Some(installed("stopped"))));
        assert_eq!(stopped.key(), "not-running");
        assert_eq!(stopped.action(), None);

        let mut with_start = probe(Ok("/bin/pohunek"), Some(installed("failed")));
        with_start.start_supported = true;
        assert_eq!(
            derive(&with_start).action(),
            Some(SetupAction::StartService)
        );

        let running = derive(&probe(Ok("/bin/pohunek"), Some(installed("running"))));
        assert_eq!(running.key(), "not-answering");

        let pending = ServiceStatus {
            transaction_in_progress: true,
            ..Default::default()
        };
        assert_eq!(
            derive(&probe(Ok("/bin/pohunek"), Some(pending))).key(),
            "busy"
        );
    }

    #[test]
    fn help_listing_detects_the_start_command() {
        let help = "Usage: pohunek service\n\nCommands:\n  install  Install\n  start    Start\n\nOptions:\n  -h\n";
        assert!(lists_subcommand(help, "start"));
        assert!(!lists_subcommand(
            help.replace("  start    Start\n", "").as_str(),
            "start"
        ));
    }
}
