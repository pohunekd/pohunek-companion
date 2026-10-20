use std::ffi::OsStr;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Component, Path, PathBuf};

const RUNTIME_ENV: &str = "XDG_RUNTIME_DIR";
const APP_DIR: &str = "pohunek";
const SOCKET_NAME: &str = "daemon.sock";
const ROOT_MODE: u32 = 0o700;
const SOCKET_MODE: u32 = 0o600;
const MODE_MASK: u32 = 0o7777;
#[cfg(target_os = "macos")]
const MACOS_DEFAULT_ROOT_PREFIX: &str = "/private/tmp/pohunek-";
// `sun_path` capacity including the terminating NUL.
#[cfg(target_os = "macos")]
const SUN_PATH_CAPACITY: usize = 104;
#[cfg(not(target_os = "macos"))]
const SUN_PATH_CAPACITY: usize = 108;

/// Why the local daemon socket path could not be resolved or trusted. Nothing
/// is created or repaired.
#[derive(Debug, thiserror::Error)]
pub enum SocketError {
    #[error("{RUNTIME_ENV} is not set; the daemon socket cannot be located")]
    MissingRuntimeDir,
    #[error("{RUNTIME_ENV} is invalid: {0}")]
    InvalidRuntimeDir(&'static str),
    #[error("socket path {path} is {len} bytes, over the {max} byte limit of a Unix socket")]
    PathTooLong {
        path: PathBuf,
        len: usize,
        max: usize,
    },
    #[error("cannot inspect {path}: {source}")]
    Inspect {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("{path} is a symlink; refusing to follow it")]
    Symlink { path: PathBuf },
    #[error("{path} is not a directory")]
    RootNotDirectory { path: PathBuf },
    #[error("{path} is not a socket")]
    NotSocket { path: PathBuf },
    #[error("{path} is owned by uid {found}, expected {expected}")]
    WrongOwner {
        path: PathBuf,
        found: u32,
        expected: u32,
    },
    #[error("{path} has mode {found:04o}, expected {expected:04o}")]
    WrongMode {
        path: PathBuf,
        found: u32,
        expected: u32,
    },
}

/// Resolves the local daemon socket from the process environment and verifies
/// that the runtime root and the socket are owned by this user with owner-only
/// modes. Follows the runtime-path contract in Pohunek's `docs/public-api.md`.
pub fn local_socket_path() -> Result<PathBuf, SocketError> {
    let euid = rustix::process::geteuid().as_raw();
    let runtime_dir = std::env::var_os(RUNTIME_ENV);
    let root = runtime_root(runtime_dir.as_deref(), euid)?;
    let socket = root.join(SOCKET_NAME);
    check_length(&socket)?;
    verify_root(&root, euid)?;
    verify_socket(&socket, euid)?;
    Ok(socket)
}

fn runtime_root(runtime_dir: Option<&OsStr>, euid: u32) -> Result<PathBuf, SocketError> {
    match runtime_dir {
        Some(value) => explicit_root(value),
        None => default_root(euid),
    }
}

fn explicit_root(value: &OsStr) -> Result<PathBuf, SocketError> {
    if value.is_empty() {
        return Err(SocketError::InvalidRuntimeDir("empty"));
    }
    if value.as_bytes().contains(&0) {
        return Err(SocketError::InvalidRuntimeDir("contains a NUL byte"));
    }
    let path = Path::new(value);
    if !path.is_absolute() {
        return Err(SocketError::InvalidRuntimeDir("not an absolute path"));
    }
    if path.components().any(|c| c == Component::ParentDir) {
        return Err(SocketError::InvalidRuntimeDir("contains a `..` component"));
    }
    Ok(path.join(APP_DIR))
}

#[cfg(target_os = "macos")]
fn default_root(euid: u32) -> Result<PathBuf, SocketError> {
    Ok(PathBuf::from(format!("{MACOS_DEFAULT_ROOT_PREFIX}{euid}")))
}

#[cfg(not(target_os = "macos"))]
fn default_root(_euid: u32) -> Result<PathBuf, SocketError> {
    Err(SocketError::MissingRuntimeDir)
}

fn check_length(socket: &Path) -> Result<(), SocketError> {
    let len = socket.as_os_str().len();
    if len >= SUN_PATH_CAPACITY {
        return Err(SocketError::PathTooLong {
            path: socket.to_path_buf(),
            len,
            max: SUN_PATH_CAPACITY - 1,
        });
    }
    Ok(())
}

fn inspect(path: &Path) -> Result<std::fs::Metadata, SocketError> {
    let meta = std::fs::symlink_metadata(path).map_err(|source| SocketError::Inspect {
        path: path.to_path_buf(),
        source,
    })?;
    if meta.file_type().is_symlink() {
        return Err(SocketError::Symlink {
            path: path.to_path_buf(),
        });
    }
    Ok(meta)
}

fn check_owner_and_mode(
    path: &Path,
    meta: &std::fs::Metadata,
    euid: u32,
    mode: u32,
) -> Result<(), SocketError> {
    if meta.uid() != euid {
        return Err(SocketError::WrongOwner {
            path: path.to_path_buf(),
            found: meta.uid(),
            expected: euid,
        });
    }
    let found = meta.mode() & MODE_MASK;
    if found != mode {
        return Err(SocketError::WrongMode {
            path: path.to_path_buf(),
            found,
            expected: mode,
        });
    }
    Ok(())
}

fn verify_root(root: &Path, euid: u32) -> Result<(), SocketError> {
    let meta = inspect(root)?;
    if !meta.is_dir() {
        return Err(SocketError::RootNotDirectory {
            path: root.to_path_buf(),
        });
    }
    check_owner_and_mode(root, &meta, euid, ROOT_MODE)
}

fn verify_socket(socket: &Path, euid: u32) -> Result<(), SocketError> {
    let meta = inspect(socket)?;
    if !meta.file_type().is_socket() {
        return Err(SocketError::NotSocket {
            path: socket.to_path_buf(),
        });
    }
    check_owner_and_mode(socket, &meta, euid, SOCKET_MODE)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Cases from Pohunek crates/paths/fixtures/runtime-paths.json at v0.35.2
    // that concern the runtime root.
    #[test]
    fn runtime_root_cases() {
        let cases: &[(&str, Option<&str>, Result<&str, &str>)] = &[
            (
                "explicit",
                Some("/run/user/1000"),
                Ok("/run/user/1000/pohunek"),
            ),
            (
                "explicit with spaces",
                Some("/Volumes/Runtime Disk/user"),
                Ok("/Volumes/Runtime Disk/user/pohunek"),
            ),
            ("empty", Some(""), Err("empty")),
            ("relative", Some("run/user"), Err("not an absolute path")),
            (
                "parent component",
                Some("/run/../x"),
                Err("contains a `..` component"),
            ),
        ];
        for (name, env, expected) in cases {
            let got = runtime_root(env.map(OsStr::new), 501);
            match (expected, got) {
                (Ok(path), Ok(root)) => assert_eq!(root, Path::new(path), "{name}"),
                (Err(reason), Err(SocketError::InvalidRuntimeDir(got))) => {
                    assert_eq!(*reason, got, "{name}")
                }
                (_, got) => panic!("{name}: unexpected {got:?}"),
            }
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_default_root_uses_euid() {
        assert_eq!(
            runtime_root(None, 501).unwrap(),
            Path::new("/private/tmp/pohunek-501")
        );
        assert_eq!(
            runtime_root(None, 4294967294).unwrap(),
            Path::new("/private/tmp/pohunek-4294967294")
        );
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn linux_requires_runtime_dir() {
        assert!(matches!(
            runtime_root(None, 1000),
            Err(SocketError::MissingRuntimeDir)
        ));
    }
}
