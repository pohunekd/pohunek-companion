//! JSON log output: `logs/` under the working directory in debug builds, the
//! platform state directory in release builds.

use std::fs::{DirBuilder, File, OpenOptions};
use std::io;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::PathBuf;
use std::sync::Arc;

use tracing::Level;

const LOG_FILE: &str = "companion.json";
const DEBUG_DIR: &str = "logs";
const APP_DIR: &str = "pohunek-companion";
const DIR_MODE: u32 = 0o700;
const FILE_MODE: u32 = 0o600;
const MAX_LEVEL: Level = Level::INFO;

#[derive(Debug, thiserror::Error)]
pub enum LogError {
    #[error("cannot locate the log directory: {0}")]
    NoDirectory(&'static str),
    #[error("cannot open the log file {path}: {source}")]
    Open {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("cannot install the log subscriber: {0}")]
    Subscriber(String),
}

fn log_dir() -> Result<PathBuf, LogError> {
    if cfg!(debug_assertions) {
        return Ok(PathBuf::from(DEBUG_DIR));
    }
    state_dir()
}

#[cfg(target_os = "macos")]
fn state_dir() -> Result<PathBuf, LogError> {
    let home = std::env::home_dir().ok_or(LogError::NoDirectory("HOME is not set"))?;
    Ok(home.join("Library/Logs").join(APP_DIR))
}

#[cfg(not(target_os = "macos"))]
fn state_dir() -> Result<PathBuf, LogError> {
    let base = match std::env::var_os("XDG_STATE_HOME").map(PathBuf::from) {
        Some(dir) if dir.is_absolute() => dir,
        _ => std::env::home_dir()
            .ok_or(LogError::NoDirectory(
                "neither XDG_STATE_HOME nor HOME is set",
            ))?
            .join(".local/state"),
    };
    Ok(base.join(APP_DIR))
}

fn open_log_file() -> Result<(File, PathBuf), LogError> {
    let dir = log_dir()?;
    let path = dir.join(LOG_FILE);
    let open_err = |source| LogError::Open {
        path: path.clone(),
        source,
    };
    DirBuilder::new()
        .recursive(true)
        .mode(DIR_MODE)
        .create(&dir)
        .map_err(open_err)?;
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(FILE_MODE)
        .open(&path)
        .map_err(open_err)?;
    Ok((file, path))
}

/// Installs the global JSON subscriber and returns the log file path. Must run
/// once, before any event worth keeping.
pub fn init() -> Result<PathBuf, LogError> {
    let (file, path) = open_log_file()?;
    tracing_subscriber::fmt()
        .json()
        .with_max_level(MAX_LEVEL)
        .with_writer(Arc::new(file))
        .try_init()
        .map_err(|err| LogError::Subscriber(err.to_string()))?;
    Ok(path)
}
