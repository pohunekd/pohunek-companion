use std::ffi::OsString;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use pohunek_client::is_local_host;
use serde::Deserialize;

use crate::attach::{AttachConfig, AttachTemplate};
use crate::attach_macos::AttachTerminal;
use crate::badge::sanitize;
use crate::open::{OpenConfig, OpenTemplate, default_open_template};

const CONFIG_HOME_ENV: &str = "XDG_CONFIG_HOME";
const HOME_ENV: &str = "HOME";
const CONFIG_DIR: &str = "pohunek";
const CONFIG_FILE: &str = "companion.toml";
const HOME_CONFIG_DIR: &str = ".config";

/// Why `companion.toml` could not be located, read, or accepted. Every
/// variant names the file, and the invalid-value variants name the key.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot locate the config directory: neither {CONFIG_HOME_ENV} nor {HOME_ENV} is set")]
    NoConfigDir,
    #[error("{CONFIG_HOME_ENV} must be an absolute path, got {}", .0.display())]
    RelativeConfigHome(PathBuf),
    #[error("cannot read {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("invalid {path}: {message}")]
    Parse { path: PathBuf, message: String },
    #[error("invalid {path}: `{key}` must be greater than zero")]
    ZeroValue { path: PathBuf, key: &'static str },
    #[error(
        "invalid {path}: `attach_command` and `attach_terminal` are mutually exclusive; set only one"
    )]
    AttachConflict { path: PathBuf },
    #[error("invalid {path}: `setup.release_base_url`: {problem}")]
    ReleaseUrl { path: PathBuf, problem: String },
    #[error("invalid {path}: `pohunek_bin` must not be empty")]
    EmptyPohunekBin { path: PathBuf },
    #[error("invalid {path}: `layout.rules[{index}]` {problem}")]
    Rule {
        path: PathBuf,
        index: usize,
        problem: String,
    },
    #[error("invalid {path}: `hosts[{index}]` (target {target:?}) {problem}")]
    Host {
        path: PathBuf,
        index: usize,
        target: String,
        problem: String,
    },
    #[error(
        "invalid {path}: `daemon.reconnect_initial_ms` ({initial}) must not exceed \
         `daemon.reconnect_max_ms` ({max})"
    )]
    BackoffOrder {
        path: PathBuf,
        initial: u64,
        max: u64,
    },
}

/// Settings from `companion.toml`. Unknown keys are rejected. A missing file
/// yields the defaults documented on each field.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    /// `[daemon]` table: how the companion talks to the Pohunek daemon.
    pub daemon: DaemonConfig,
    /// `[tray]` table: how sessions are classified for the owner.
    pub tray: TrayConfig,
    /// `[layout]` table: where sessions land without a drag.
    pub layout: LayoutConfig,
    /// `[view]` table: how the sidebar sections are titled.
    pub view: ViewConfig,
    /// `[setup]` table: how the setup screen runs the `pohunek` CLI.
    pub setup: SetupConfig,
    /// `[new_session]` table: how the new-session dialog creates sessions.
    pub new_session: NewSessionConfig,
    /// `[[hosts]]`: remote Pohunek hosts whose sessions are listed next to the
    /// local ones, each on its own connection. Optional; default none (local
    /// only). Example: `[[hosts]]` with `target = "zajcaair"` and
    /// `label = "Air"`.
    pub hosts: Vec<HostConfig>,
    /// `attach_command`: argv template that opens a session in a terminal,
    /// as an array of words (no shell is involved). Optional; when absent,
    /// clicking a session reports that attach is not configured. `{bin}`
    /// (the resolved `pohunek_bin`), `{host}` and `{id}` are replaced in each
    /// word in one left-to-right pass; inserted values are never rescanned,
    /// and any other `{name}` is rejected at startup. The first word is the
    /// program: an absolute path, or a bare name resolved on `PATH`.
    /// Example: `attach_command = ["foot", "--", "{bin}", "attach", "--host",
    /// "{host}", "{id}"]`.
    pub attach_command: Option<AttachTemplate>,
    /// `attach_terminal`: opens a session in a terminal app without an argv
    /// template. Only `"terminal"` (Terminal.app, macOS) is known. It starts
    /// Terminal.app through `open -a` on a generated `.command` script that
    /// runs `pohunek attach`. Optional; mutually exclusive with
    /// `attach_command`. Example: `attach_terminal = "terminal"`.
    pub attach_terminal: Option<AttachTerminal>,
    /// `pohunek_bin`: the `pohunek` executable that `{bin}` expands to and
    /// that the setup screen runs, an absolute path or a bare name resolved
    /// on `PATH` (on macOS also in the login shell's `PATH`, then
    /// `/opt/homebrew/bin`, `/usr/local/bin`, `~/.local/bin`; the setup
    /// screen checks those directories on every platform). Optional; default `pohunek`. Example: `pohunek_bin = "/opt/pohunek/bin/pohunek"`.
    pub pohunek_bin: String,
    /// `open_command`: argv template that opens a session's folder or Work
    /// link, as an array of words (no shell is involved). The only
    /// placeholder is `{target}` (the folder path or the `http(s)` URL), which
    /// must appear at least once and is replaced in each word in one pass.
    /// The first word is the program: an absolute path, or a bare name
    /// resolved on `PATH`. Optional; default `["xdg-open", "{target}"]` on
    /// Linux and `["open", "{target}"]` on macOS. Example:
    /// `open_command = ["firefox", "--new-tab", "{target}"]`.
    pub open_command: Option<OpenTemplate>,
    /// `attach_observe_ms`: how long a freshly started attach program is
    /// watched for an early failed exit, in milliseconds, greater than zero.
    /// Optional; default 1000. Example: `attach_observe_ms = 2000`.
    pub attach_observe_ms: u64,
    /// `login_shell_timeout_ms`: how long the macOS probe of the login
    /// shell's `PATH` may take, in milliseconds, greater than zero. Optional;
    /// default 2000. Example: `login_shell_timeout_ms = 5000`.
    pub login_shell_timeout_ms: u64,
    /// `attach_script_max_age_s`: age after which a staged Terminal.app
    /// script is removed, in seconds, greater than zero. Optional; default
    /// 3600. Example: `attach_script_max_age_s = 600`.
    pub attach_script_max_age_s: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            daemon: DaemonConfig::default(),
            tray: TrayConfig::default(),
            layout: LayoutConfig::default(),
            view: ViewConfig::default(),
            setup: SetupConfig::default(),
            new_session: NewSessionConfig::default(),
            hosts: Vec::new(),
            attach_command: None,
            attach_terminal: None,
            pohunek_bin: "pohunek".to_owned(),
            open_command: None,
            attach_observe_ms: 1000,
            login_shell_timeout_ms: 2000,
            attach_script_max_age_s: 3600,
        }
    }
}

/// One `[[hosts]]` entry: a remote Pohunek host.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostConfig {
    /// `target`: a Pohunek host route as `pohunek --host` accepts it (for
    /// example `zajcaair`, `netbird:peer~...`, or `netbird:peer~...@28722`).
    /// Required, not empty, not `local`, unique among the entries, and not a
    /// name of this machine. It is the session key's host, verbatim.
    pub target: String,
    /// `label`: the host name shown on the host's rows. Optional; default the
    /// `target`.
    pub label: Option<String>,
}

impl HostConfig {
    /// Why the entry is unusable, without the duplicate check.
    fn problem(&self) -> Option<String> {
        if self.target.trim().is_empty() {
            return Some("needs a non-empty `target`".to_owned());
        }
        if is_local_host(self.target.trim()) {
            return Some(
                "must not name the local host; the local daemon is always listed".to_owned(),
            );
        }
        if self.target != self.target.trim() {
            return Some("has leading or trailing whitespace in `target`".to_owned());
        }
        if self
            .label
            .as_deref()
            .is_some_and(|label| label.trim().is_empty())
        {
            return Some("has an empty `label`".to_owned());
        }
        None
    }

    /// The sanitized host name for display.
    pub fn display_label(&self) -> String {
        let label = self.label.as_deref().map(sanitize);
        match label {
            Some(label) if !label.is_empty() => label,
            _ => sanitize(&self.target),
        }
    }
}

/// `[layout]` table.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct LayoutConfig {
    /// `[[layout.rules]]`: ordered placement rules. The first rule whose
    /// conditions all match a session selects its group; a session the owner
    /// dragged is never moved by a rule. Optional; default none.
    pub rules: Vec<PlacementRule>,
}

/// One `[[layout.rules]]` entry: a target group and the conditions a session
/// must all meet, each an exact string comparison. At least one condition is
/// required.
/// Example: `group = "Reviews"`, `work_role = "reviewer"`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlacementRule {
    /// `group`: name of the target group (matched ignoring case; created
    /// when missing). Required, not empty.
    pub group: String,
    /// `project`: the session's project label or project id.
    pub project: Option<String>,
    /// `agent`: the session's agent.
    pub agent: Option<String>,
    /// `work_provider`: the Pohunek Work link provider.
    pub work_provider: Option<String>,
    /// `work_kind`: the Pohunek Work link kind.
    pub work_kind: Option<String>,
    /// `work_role`: the session's `work.role`.
    pub work_role: Option<String>,
}

impl PlacementRule {
    fn conditions(&self) -> [(&'static str, &Option<String>); 5] {
        [
            ("project", &self.project),
            ("agent", &self.agent),
            ("work_provider", &self.work_provider),
            ("work_kind", &self.work_kind),
            ("work_role", &self.work_role),
        ]
    }

    /// Why the rule is unusable, naming the key.
    fn problem(&self) -> Option<String> {
        if self.group.trim().is_empty() {
            return Some("needs a non-empty `group`".to_owned());
        }
        if let Some((key, _)) = self
            .conditions()
            .into_iter()
            .find(|(_, value)| value.as_deref().is_some_and(|v| v.trim().is_empty()))
        {
            return Some(format!("has an empty `{key}`"));
        }
        if self.conditions().iter().all(|(_, value)| value.is_none()) {
            return Some(
                "needs at least one of `project`, `agent`, `work_provider`, `work_kind`, \
                 `work_role`"
                    .to_owned(),
            );
        }
        None
    }
}

/// `[new_session]` table.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct NewSessionConfig {
    /// `new_session.cols`: initial terminal width of a created session, in
    /// columns, greater than zero. Optional; default 80.
    pub cols: u16,
    /// `new_session.rows`: initial terminal height of a created session, in
    /// rows, greater than zero. Optional; default 24.
    pub rows: u16,
    /// `new_session.create_timeout_ms`: time allowed for the daemon to answer
    /// `session.new`, in milliseconds, greater than zero. Creation runs
    /// project hooks and waits for the agent to accept the initial prompt, so
    /// it is far longer than an ordinary request. A create that exceeds it has
    /// an unknown outcome and is never resent. Optional; default 120000.
    pub create_timeout_ms: u64,
    /// `new_session.attach_after_create`: initial state of the dialog's
    /// "Attach after creating" checkbox; when checked, a created session is
    /// opened with the attach settings. Optional; default true.
    pub attach_after_create: bool,
    /// `new_session.pending_placement_max_age_secs`: how long a recorded
    /// placement for a session created from a row's menu waits for that
    /// session to show up in the sidebar before it is dropped, in seconds,
    /// greater than zero. Optional; default 600.
    pub pending_placement_max_age_secs: u64,
}

impl Default for NewSessionConfig {
    fn default() -> Self {
        Self {
            cols: 80,
            rows: 24,
            create_timeout_ms: 120_000,
            attach_after_create: true,
            pending_placement_max_age_secs: 600,
        }
    }
}

impl NewSessionConfig {
    pub fn pending_placement_max_age(&self) -> Duration {
        Duration::from_secs(self.pending_placement_max_age_secs)
    }

    pub fn create_timeout(&self) -> Duration {
        Duration::from_millis(self.create_timeout_ms)
    }
}

/// `[view]` table.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ViewConfig {
    /// `view.section_title_max_chars`: longest section title in the By
    /// project and By Work item views, in characters, greater than zero;
    /// longer titles end with an ellipsis. Optional; default 40.
    pub section_title_max_chars: usize,
}

impl Default for ViewConfig {
    fn default() -> Self {
        Self {
            section_title_max_chars: 40,
        }
    }
}

/// `[tray]` table.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct TrayConfig {
    /// `tray.idle_threshold_secs`: how long an idle session with no subagent
    /// or background work must stay so before it counts as Idle rather than
    /// Working, in seconds, greater than zero. Optional; default 300.
    pub idle_threshold_secs: u64,
}

impl Default for TrayConfig {
    fn default() -> Self {
        Self {
            idle_threshold_secs: 300,
        }
    }
}

impl TrayConfig {
    pub fn idle_threshold(&self) -> Duration {
        Duration::from_secs(self.idle_threshold_secs)
    }
}

/// `[setup]` table.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct SetupConfig {
    /// `setup.probe_timeout_secs`: time allowed for one read-only `pohunek`
    /// call (`service status`, `service check`), in seconds, greater than
    /// zero. Optional; default 15.
    pub probe_timeout_secs: u64,
    /// `setup.action_timeout_secs`: time allowed for a command the owner
    /// confirmed (`service install`, which waits until the daemon answers),
    /// in seconds, greater than zero. Optional; default 180.
    pub action_timeout_secs: u64,
    /// `setup.message_max_chars`: longest command output or error shown on
    /// the setup screen, in characters, greater than zero; longer text is cut
    /// with an ellipsis. Optional; default 400.
    pub message_max_chars: usize,
    /// `setup.download_timeout_secs`: time allowed for downloading one release
    /// file, in seconds, greater than zero. Optional; default 600.
    pub download_timeout_secs: u64,
    /// `setup.install_timeout_secs`: time allowed for the install script of a
    /// downloaded release (or `brew install`), in seconds, greater than zero.
    /// Optional; default 900.
    pub install_timeout_secs: u64,
    /// `setup.download_max_mib`: largest release archive accepted, in MiB,
    /// greater than zero. Optional; default 128.
    pub download_max_mib: u64,
    /// `setup.extract_max_mib`: largest total size of the unpacked archive, in
    /// MiB, greater than zero. Optional; default 512.
    pub extract_max_mib: u64,
    /// `setup.extract_max_entries`: most entries an archive may hold, greater
    /// than zero. Optional; default 4096.
    pub extract_max_entries: u64,
    /// `setup.stale_install_max_age_secs`: age after which an unfinished
    /// install directory left in the state directory (a crashed or killed
    /// install) is removed when the next install starts, in seconds, greater
    /// than zero. Optional; default 3600.
    pub stale_install_max_age_secs: u64,
    /// `setup.release_base_url`: where release files are downloaded from, as
    /// `<base>/<tag>/<file>`. Must be an `https` URL, or `http` to a loopback
    /// host (for a local mirror). Optional; default the GitHub releases of
    /// `zajca/pohunek`.
    pub release_base_url: String,
}

/// The GitHub download location of Pohunek releases.
pub const DEFAULT_RELEASE_BASE_URL: &str = "https://github.com/zajca/pohunek/releases/download";

impl Default for SetupConfig {
    fn default() -> Self {
        Self {
            probe_timeout_secs: 15,
            action_timeout_secs: 180,
            message_max_chars: 400,
            download_timeout_secs: 600,
            install_timeout_secs: 900,
            download_max_mib: 128,
            extract_max_mib: 512,
            extract_max_entries: 4096,
            stale_install_max_age_secs: 3600,
            release_base_url: DEFAULT_RELEASE_BASE_URL.to_owned(),
        }
    }
}

impl SetupConfig {
    pub fn probe_timeout(&self) -> Duration {
        Duration::from_secs(self.probe_timeout_secs)
    }

    pub fn action_timeout(&self) -> Duration {
        Duration::from_secs(self.action_timeout_secs)
    }

    pub fn download_timeout(&self) -> Duration {
        Duration::from_secs(self.download_timeout_secs)
    }

    pub fn install_timeout(&self) -> Duration {
        Duration::from_secs(self.install_timeout_secs)
    }
}

/// `[daemon]` table.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct DaemonConfig {
    /// `daemon.connect_timeout_ms`: time allowed to open the daemon
    /// connection, in milliseconds, greater than zero. Optional; default 5000
    /// (the `pohunek-client` default).
    pub connect_timeout_ms: u64,
    /// `daemon.request_timeout_ms`: time allowed for one daemon request to be
    /// answered, in milliseconds, greater than zero. Optional; default 5000
    /// (the `pohunek-client` default).
    pub request_timeout_ms: u64,
    /// `daemon.session_action_timeout_ms`: time allowed for the daemon to
    /// answer `session.stop` or `session.remove`, in milliseconds, greater
    /// than zero. Both wait for the session's runtime to end and sweep its
    /// processes, so it is longer than an ordinary request. Optional; default
    /// 60000.
    pub session_action_timeout_ms: u64,
    /// `daemon.reconnect_initial_ms`: wait before the first reconnect attempt
    /// after the daemon connection is lost or fails, in milliseconds, greater
    /// than zero and not above `reconnect_max_ms`. The wait doubles on every
    /// consecutive failure. Optional; default 500.
    pub reconnect_initial_ms: u64,
    /// `daemon.reconnect_max_ms`: upper bound of the reconnect wait, in
    /// milliseconds, greater than zero. A connection must stay up this long
    /// before the wait starts over at `reconnect_initial_ms`. Optional; default 30000.
    pub reconnect_max_ms: u64,
    /// `daemon.notification_page_size`: records requested per
    /// `notification.list` page while seeding, greater than zero. The page
    /// size is halved when the daemon reports the page as too large.
    /// Optional; default 200.
    pub notification_page_size: u32,
    /// `daemon.reseed_interval_secs`: while connected, how often the session
    /// list and attention notifications are re-read from the daemon and
    /// reconciled, in seconds, greater than zero. Bounds how long a dropped
    /// event can leave the list stale. Optional; default 60.
    pub reseed_interval_secs: u64,
    /// `daemon.discover_interval_secs`: how often the local daemon's
    /// `host.discover` is asked for reachable peers, in seconds, greater than
    /// zero. Every reachable peer is connected as a remote host. Optional;
    /// default 60.
    pub discover_interval_secs: u64,
    /// `daemon.discover_timeout_secs`: time allowed for one `host.discover`
    /// call, in seconds, greater than zero. The daemon probes every peer
    /// before it answers, so this is longer than an ordinary request.
    /// Optional; default 20.
    pub discover_timeout_secs: u64,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            connect_timeout_ms: 5000,
            request_timeout_ms: 5000,
            session_action_timeout_ms: 60_000,
            reconnect_initial_ms: 500,
            reconnect_max_ms: 30_000,
            notification_page_size: 200,
            reseed_interval_secs: 60,
            discover_interval_secs: 60,
            discover_timeout_secs: 20,
        }
    }
}

impl DaemonConfig {
    pub fn connect_timeout(&self) -> Duration {
        Duration::from_millis(self.connect_timeout_ms)
    }

    pub fn request_timeout(&self) -> Duration {
        Duration::from_millis(self.request_timeout_ms)
    }

    pub fn session_action_timeout(&self) -> Duration {
        Duration::from_millis(self.session_action_timeout_ms)
    }

    pub fn reconnect_initial(&self) -> Duration {
        Duration::from_millis(self.reconnect_initial_ms)
    }

    pub fn reconnect_max(&self) -> Duration {
        Duration::from_millis(self.reconnect_max_ms)
    }

    pub fn reseed_interval(&self) -> Duration {
        Duration::from_secs(self.reseed_interval_secs)
    }

    pub fn discover_interval(&self) -> Duration {
        Duration::from_secs(self.discover_interval_secs)
    }

    pub fn discover_timeout(&self) -> Duration {
        Duration::from_secs(self.discover_timeout_secs)
    }
}

impl Config {
    /// Location of `companion.toml`: `$XDG_CONFIG_HOME/pohunek/companion.toml`,
    /// else `~/.config/pohunek/companion.toml`.
    pub fn path() -> Result<PathBuf, ConfigError> {
        config_path(
            std::env::var_os(CONFIG_HOME_ENV),
            std::env::var_os(HOME_ENV),
        )
    }

    /// Loads the file at [`Config::path`]. A missing file is valid and gives
    /// the defaults.
    pub fn load() -> Result<Self, ConfigError> {
        Self::load_from(&Self::path()?)
    }

    /// The attach settings as the launcher consumes them.
    pub fn attach(&self) -> AttachConfig {
        AttachConfig {
            command: self.attach_command.clone(),
            terminal: self.attach_terminal,
            pohunek_bin: self.pohunek_bin.clone(),
            observe: Duration::from_millis(self.attach_observe_ms),
            login_shell_timeout: Duration::from_millis(self.login_shell_timeout_ms),
            script_max_age: Duration::from_secs(self.attach_script_max_age_s),
        }
    }

    /// The open settings as the launcher consumes them; `attach_observe_ms`
    /// and `login_shell_timeout_ms` apply to the opener as well.
    pub fn open(&self) -> OpenConfig {
        OpenConfig {
            command: self
                .open_command
                .clone()
                .unwrap_or_else(default_open_template),
            observe: Duration::from_millis(self.attach_observe_ms),
            login_shell_timeout: Duration::from_millis(self.login_shell_timeout_ms),
        }
    }

    fn load_from(path: &Path) -> Result<Self, ConfigError> {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::parse(&text, path),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(source) => Err(ConfigError::Read {
                path: path.to_owned(),
                source,
            }),
        }
    }

    /// Parses and validates `text`; `path` only labels errors.
    fn parse(text: &str, path: &Path) -> Result<Self, ConfigError> {
        unknown_rule_key(text, path)?;
        let config: Self = toml::from_str(text).map_err(|err| ConfigError::Parse {
            path: path.to_owned(),
            message: one_line(&err),
        })?;
        config.validate(path)?;
        Ok(config)
    }

    fn validate(&self, path: &Path) -> Result<(), ConfigError> {
        let zero_key = [
            ("daemon.connect_timeout_ms", self.daemon.connect_timeout_ms),
            ("daemon.request_timeout_ms", self.daemon.request_timeout_ms),
            (
                "daemon.session_action_timeout_ms",
                self.daemon.session_action_timeout_ms,
            ),
            ("setup.probe_timeout_secs", self.setup.probe_timeout_secs),
            ("setup.action_timeout_secs", self.setup.action_timeout_secs),
            (
                "setup.message_max_chars",
                u64::try_from(self.setup.message_max_chars).unwrap_or(u64::MAX),
            ),
            (
                "setup.download_timeout_secs",
                self.setup.download_timeout_secs,
            ),
            (
                "setup.install_timeout_secs",
                self.setup.install_timeout_secs,
            ),
            ("setup.download_max_mib", self.setup.download_max_mib),
            ("setup.extract_max_mib", self.setup.extract_max_mib),
            ("setup.extract_max_entries", self.setup.extract_max_entries),
            (
                "setup.stale_install_max_age_secs",
                self.setup.stale_install_max_age_secs,
            ),
            ("attach_observe_ms", self.attach_observe_ms),
            ("new_session.cols", u64::from(self.new_session.cols)),
            ("new_session.rows", u64::from(self.new_session.rows)),
            (
                "new_session.create_timeout_ms",
                self.new_session.create_timeout_ms,
            ),
            (
                "new_session.pending_placement_max_age_secs",
                self.new_session.pending_placement_max_age_secs,
            ),
            ("tray.idle_threshold_secs", self.tray.idle_threshold_secs),
            (
                "view.section_title_max_chars",
                u64::try_from(self.view.section_title_max_chars).unwrap_or(u64::MAX),
            ),
            (
                "daemon.reconnect_initial_ms",
                self.daemon.reconnect_initial_ms,
            ),
            ("daemon.reconnect_max_ms", self.daemon.reconnect_max_ms),
            (
                "daemon.notification_page_size",
                u64::from(self.daemon.notification_page_size),
            ),
            ("login_shell_timeout_ms", self.login_shell_timeout_ms),
            ("attach_script_max_age_s", self.attach_script_max_age_s),
            (
                "daemon.reseed_interval_secs",
                self.daemon.reseed_interval_secs,
            ),
            (
                "daemon.discover_interval_secs",
                self.daemon.discover_interval_secs,
            ),
            (
                "daemon.discover_timeout_secs",
                self.daemon.discover_timeout_secs,
            ),
        ]
        .into_iter()
        .find_map(|(key, value)| (value == 0).then_some(key));
        if let Some(key) = zero_key {
            return Err(ConfigError::ZeroValue {
                path: path.to_owned(),
                key,
            });
        }
        if let Err(problem) = crate::install::check_base_url(&self.setup.release_base_url) {
            return Err(ConfigError::ReleaseUrl {
                path: path.to_owned(),
                problem,
            });
        }
        if self.attach_command.is_some() && self.attach_terminal.is_some() {
            return Err(ConfigError::AttachConflict {
                path: path.to_owned(),
            });
        }
        for (index, rule) in self.layout.rules.iter().enumerate() {
            if let Some(problem) = rule.problem() {
                return Err(ConfigError::Rule {
                    path: path.to_owned(),
                    index,
                    problem,
                });
            }
        }
        for (index, host) in self.hosts.iter().enumerate() {
            let problem = host.problem().or_else(|| {
                self.hosts[..index]
                    .iter()
                    .position(|other| other.target == host.target)
                    .map(|first| format!("duplicates `hosts[{first}]`"))
            });
            if let Some(problem) = problem {
                return Err(ConfigError::Host {
                    path: path.to_owned(),
                    index,
                    target: host.target.clone(),
                    problem,
                });
            }
        }
        if self.pohunek_bin.is_empty() {
            return Err(ConfigError::EmptyPohunekBin {
                path: path.to_owned(),
            });
        }
        if self.daemon.reconnect_initial_ms > self.daemon.reconnect_max_ms {
            return Err(ConfigError::BackoffOrder {
                path: path.to_owned(),
                initial: self.daemon.reconnect_initial_ms,
                max: self.daemon.reconnect_max_ms,
            });
        }
        Ok(())
    }
}

const RULE_KEYS: [&str; 6] = [
    "group",
    "project",
    "agent",
    "work_provider",
    "work_kind",
    "work_role",
];

/// Names the rule index and key of an unknown `[[layout.rules]]` key, which
/// serde alone reports without the index. Anything else is left to the
/// regular parse.
fn unknown_rule_key(text: &str, path: &Path) -> Result<(), ConfigError> {
    let Ok(table) = text.parse::<toml::Table>() else {
        return Ok(());
    };
    let rules = table
        .get("layout")
        .and_then(|layout| layout.get("rules"))
        .and_then(toml::Value::as_array);
    for (index, rule) in rules.into_iter().flatten().enumerate() {
        let unknown = rule
            .as_table()
            .and_then(|rule| rule.keys().find(|key| !RULE_KEYS.contains(&key.as_str())));
        if let Some(key) = unknown {
            return Err(ConfigError::Rule {
                path: path.to_owned(),
                index,
                problem: format!("has the unknown key `{key}`"),
            });
        }
    }
    Ok(())
}

fn config_path(
    config_home: Option<OsString>,
    home: Option<OsString>,
) -> Result<PathBuf, ConfigError> {
    let base = match config_home.filter(|value| !value.is_empty()) {
        Some(dir) => {
            let dir = PathBuf::from(dir);
            if !dir.is_absolute() {
                return Err(ConfigError::RelativeConfigHome(dir));
            }
            dir
        }
        None => {
            let home = home
                .filter(|value| !value.is_empty())
                .ok_or(ConfigError::NoConfigDir)?;
            PathBuf::from(home).join(HOME_CONFIG_DIR)
        }
    };
    Ok(base.join(CONFIG_DIR).join(CONFIG_FILE))
}

/// Collapses the multi-line TOML diagnostic into one line: the message plus
/// the offending source line, which carries the key.
fn one_line(err: &impl fmt::Display) -> String {
    err.to_string()
        .lines()
        .filter_map(|line| match line.split_once(" | ") {
            Some((_, source)) => {
                let source = source.trim();
                (!source.is_empty() && !source.chars().all(|c| c == '^'))
                    .then(|| format!("near `{source}`"))
            }
            None => {
                let line = line.trim();
                (!line.is_empty() && !line.starts_with('|')).then(|| line.to_owned())
            }
        })
        .collect::<Vec<_>>()
        .join("; ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<Config, ConfigError> {
        Config::parse(text, Path::new("/cfg/companion.toml"))
    }

    #[test]
    fn empty_file_gives_defaults() {
        assert_eq!(parse("").unwrap(), Config::default());
    }

    #[test]
    fn hosts_parse_in_file_order_with_optional_labels() {
        let config = parse(
            "[[hosts]]\ntarget = \"zajcaair\"\nlabel = \"Air\"\n[[hosts]]\ntarget = \"netbird:peer~x@28722\"\n",
        )
        .unwrap();
        assert_eq!(config.hosts.len(), 2);
        assert_eq!(config.hosts[0].display_label(), "Air");
        assert_eq!(config.hosts[1].display_label(), "netbird:peer~x@28722");
        assert!(parse("").unwrap().hosts.is_empty());
    }

    #[test]
    fn invalid_hosts_fail_naming_the_entry() {
        for (text, entry, problem) in [
            ("[[hosts]]\ntarget = \"\"\n", "hosts[0]", "non-empty"),
            ("[[hosts]]\ntarget = \"local\"\n", "hosts[0]", "local host"),
            (
                "[[hosts]]\ntarget = \"a\"\n[[hosts]]\ntarget = \"local\"\n",
                "hosts[1]",
                "local host",
            ),
            (
                "[[hosts]]\ntarget = \"a\"\n[[hosts]]\ntarget = \"b\"\n[[hosts]]\ntarget = \"a\"\n",
                "hosts[2]",
                "duplicates `hosts[0]`",
            ),
            (
                "[[hosts]]\ntarget = \"a\"\nlabel = \" \"\n",
                "hosts[0]",
                "empty `label`",
            ),
        ] {
            let msg = parse(text).unwrap_err().to_string();
            assert!(msg.contains(entry) && msg.contains(problem), "{msg}");
        }
        let msg = parse("[[hosts]]\nlabel = \"x\"\n").unwrap_err().to_string();
        assert!(msg.contains("target"), "{msg}");
    }

    #[test]
    fn valid_file_overrides_defaults() {
        let config =
            parse("[daemon]\nconnect_timeout_ms = 250\nrequest_timeout_ms = 9000\n").unwrap();
        assert_eq!(config.daemon.connect_timeout(), Duration::from_millis(250));
        assert_eq!(config.daemon.request_timeout(), Duration::from_millis(9000));
    }

    #[test]
    fn unknown_key_names_path_and_key() {
        let msg = parse("[daemon]\nconect_timeout_ms = 1\n")
            .unwrap_err()
            .to_string();
        assert!(msg.contains("/cfg/companion.toml"), "{msg}");
        assert!(msg.contains("conect_timeout_ms"), "{msg}");
    }

    #[test]
    fn rules_parse_in_file_order() {
        let config = parse(
            "[[layout.rules]]\ngroup = \"A\"\nproject = \"p\"\n\
             [[layout.rules]]\ngroup = \"B\"\nwork_role = \"reviewer\"\nagent = \"claude\"\n",
        )
        .unwrap();
        let groups: Vec<&str> = config
            .layout
            .rules
            .iter()
            .map(|r| r.group.as_str())
            .collect();
        assert_eq!(groups, ["A", "B"]);
        assert_eq!(
            config.layout.rules[1].work_role.as_deref(),
            Some("reviewer")
        );
    }

    #[test]
    fn invalid_rules_name_the_rule_and_key() {
        let cases = [
            ("group = \"A\"\n", "layout.rules[1]", "at least one"),
            (
                "group = \" \"\nproject = \"p\"\n",
                "layout.rules[1]",
                "`group`",
            ),
            (
                "group = \"A\"\nagent = \"\"\n",
                "layout.rules[1]",
                "empty `agent`",
            ),
            (
                "group = \"A\"\nproject = \"p\"\nprojct = \"x\"\n",
                "layout.rules[1]",
                "unknown key `projct`",
            ),
        ];
        for (rule, index, needle) in cases {
            let text = format!(
                "[[layout.rules]]\ngroup = \"ok\"\nproject = \"p\"\n[[layout.rules]]\n{rule}"
            );
            let msg = parse(&text).unwrap_err().to_string();
            assert!(msg.contains("/cfg/companion.toml"), "{msg}");
            assert!(msg.contains(index) && msg.contains(needle), "{msg}");
        }
    }

    #[test]
    fn zero_timeout_names_key() {
        let msg = parse("[daemon]\nrequest_timeout_ms = 0\n")
            .unwrap_err()
            .to_string();
        assert!(msg.contains("daemon.request_timeout_ms"), "{msg}");
    }

    #[test]
    fn idle_threshold_is_read_and_must_be_positive() {
        assert_eq!(
            parse("").unwrap().tray.idle_threshold(),
            Duration::from_secs(300)
        );
        let config = parse("[tray]\nidle_threshold_secs = 45\n").unwrap();
        assert_eq!(config.tray.idle_threshold(), Duration::from_secs(45));
        let msg = parse("[tray]\nidle_threshold_secs = 0\n")
            .unwrap_err()
            .to_string();
        assert!(msg.contains("tray.idle_threshold_secs"), "{msg}");
    }

    #[test]
    fn invalid_backoff_is_rejected() {
        for key in [
            "reconnect_initial_ms",
            "reconnect_max_ms",
            "notification_page_size",
            "reseed_interval_secs",
            "discover_interval_secs",
            "discover_timeout_secs",
        ] {
            let msg = parse(&format!("[daemon]\n{key} = 0\n"))
                .unwrap_err()
                .to_string();
            assert!(msg.contains(&format!("daemon.{key}")), "{msg}");
        }
        let msg = parse("[daemon]\nreconnect_initial_ms = 9000\nreconnect_max_ms = 1000\n")
            .unwrap_err()
            .to_string();
        assert!(msg.contains("must not exceed"), "{msg}");
        assert!(parse("[daemon]\nreconnect_initial_ms = 1000\nreconnect_max_ms = 1000\n").is_ok());
    }

    #[test]
    fn reseed_interval_is_read_and_must_be_positive() {
        assert_eq!(
            parse("").unwrap().daemon.reseed_interval(),
            Duration::from_secs(60)
        );
        let config = parse("[daemon]\nreseed_interval_secs = 5\n").unwrap();
        assert_eq!(config.daemon.reseed_interval(), Duration::from_secs(5));
        let msg = parse("[daemon]\nreseed_interval_secs = -1\n")
            .unwrap_err()
            .to_string();
        assert!(msg.contains("reseed_interval_secs"), "{msg}");
    }

    #[test]
    fn attach_after_create_defaults_to_true_and_rejects_other_types() {
        assert!(parse("").unwrap().new_session.attach_after_create);
        let config = parse("[new_session]\nattach_after_create = false\n").unwrap();
        assert!(!config.new_session.attach_after_create);
        let msg = parse("[new_session]\nattach_after_create = \"yes\"\n")
            .unwrap_err()
            .to_string();
        assert!(msg.contains("attach_after_create"), "{msg}");
    }

    #[test]
    fn negative_or_mistyped_timeout_is_rejected() {
        for text in [
            "[daemon]\nconnect_timeout_ms = -5\n",
            "[daemon]\nconnect_timeout_ms = \"fast\"\n",
        ] {
            let msg = parse(text).unwrap_err().to_string();
            assert!(msg.contains("/cfg/companion.toml"), "{msg}");
            assert!(msg.contains("connect_timeout_ms"), "{msg}");
        }
    }

    #[test]
    fn attach_keys_are_read_and_default_to_unset() {
        let config = parse("").unwrap();
        assert_eq!(config.attach_command, None);
        assert_eq!(config.pohunek_bin, "pohunek");
        let config = parse(
            "attach_command = [\"foot\", \"{bin}\", \"{id}\"]\npohunek_bin = \"/x/pohunek\"\n\
             attach_observe_ms = 50\n",
        )
        .unwrap();
        assert!(config.attach_command.is_some());
        assert_eq!(config.attach().observe, Duration::from_millis(50));
    }

    #[test]
    fn invalid_attach_keys_name_the_key() {
        for (text, key) in [
            ("attach_command = []\n", "attach_command"),
            ("attach_command = [\"\"]\n", "attach_command"),
            ("attach_command = [\"t\", \"{nope}\"]\n", "attach_command"),
            ("attach_command = \"foot {id}\"\n", "attach_command"),
            ("pohunek_bin = \"\"\n", "pohunek_bin"),
            ("attach_observe_ms = 0\n", "attach_observe_ms"),
            ("login_shell_timeout_ms = 0\n", "login_shell_timeout_ms"),
            ("attach_script_max_age_s = 0\n", "attach_script_max_age_s"),
            ("attach_terminal = \"kitty\"\n", "attach_terminal"),
        ] {
            let msg = parse(text).unwrap_err().to_string();
            assert!(msg.contains(key), "{text}: {msg}");
        }
    }

    #[test]
    fn attach_command_and_attach_terminal_conflict_naming_both_keys() {
        let msg = parse("attach_command = [\"foot\"]\nattach_terminal = \"terminal\"\n")
            .unwrap_err()
            .to_string();
        assert!(msg.contains("attach_command"), "{msg}");
        assert!(msg.contains("attach_terminal"), "{msg}");
        assert!(msg.contains("/cfg/companion.toml"), "{msg}");
        let config = parse("attach_terminal = \"terminal\"\n").unwrap();
        assert_eq!(config.attach().terminal, Some(AttachTerminal::Terminal));
    }

    #[test]
    fn path_prefers_config_home_then_home() {
        let path = config_path(Some("/x".into()), Some("/h".into())).unwrap();
        assert_eq!(path, Path::new("/x/pohunek/companion.toml"));
        let path = config_path(None, Some("/h".into())).unwrap();
        assert_eq!(path, Path::new("/h/.config/pohunek/companion.toml"));
        assert!(config_path(Some("rel".into()), None).is_err());
        assert!(config_path(None, None).is_err());
    }
}
