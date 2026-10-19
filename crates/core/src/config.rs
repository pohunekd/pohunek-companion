use std::ffi::OsString;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

use crate::attach::{AttachConfig, AttachTemplate};

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
    #[error("invalid {path}: `{key}` must be greater than zero (milliseconds)")]
    ZeroValue { path: PathBuf, key: &'static str },
    #[error("invalid {path}: `pohunek_bin` must not be empty")]
    EmptyPohunekBin { path: PathBuf },
}

/// Settings from `companion.toml`. Unknown keys are rejected. A missing file
/// yields the defaults documented on each field.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    /// `[daemon]` table: how the companion talks to the Pohunek daemon.
    pub daemon: DaemonConfig,
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
    /// `pohunek_bin`: the `pohunek` executable that `{bin}` expands to, an
    /// absolute path or a bare name resolved on `PATH`. Optional; default
    /// `pohunek`. Example: `pohunek_bin = "/opt/pohunek/bin/pohunek"`.
    pub pohunek_bin: String,
    /// `attach_observe_ms`: how long a freshly started attach program is
    /// watched for an early failed exit, in milliseconds, greater than zero.
    /// Optional; default 1000. Example: `attach_observe_ms = 2000`.
    pub attach_observe_ms: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            daemon: DaemonConfig::default(),
            attach_command: None,
            pohunek_bin: "pohunek".to_owned(),
            attach_observe_ms: 1000,
        }
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
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            connect_timeout_ms: 5000,
            request_timeout_ms: 5000,
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
            pohunek_bin: self.pohunek_bin.clone(),
            observe: Duration::from_millis(self.attach_observe_ms),
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
            ("attach_observe_ms", self.attach_observe_ms),
        ]
        .into_iter()
        .find_map(|(key, value)| (value == 0).then_some(key));
        if let Some(key) = zero_key {
            return Err(ConfigError::ZeroValue {
                path: path.to_owned(),
                key,
            });
        }
        if self.pohunek_bin.is_empty() {
            return Err(ConfigError::EmptyPohunekBin {
                path: path.to_owned(),
            });
        }
        Ok(())
    }
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
    fn zero_timeout_names_key() {
        let msg = parse("[daemon]\nrequest_timeout_ms = 0\n")
            .unwrap_err()
            .to_string();
        assert!(msg.contains("daemon.request_timeout_ms"), "{msg}");
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
        ] {
            let msg = parse(text).unwrap_err().to_string();
            assert!(msg.contains(key), "{text}: {msg}");
        }
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
