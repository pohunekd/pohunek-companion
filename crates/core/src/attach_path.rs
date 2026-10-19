//! Program lookup directories for the attach launcher. On macOS an app started
//! from Finder or launchd inherits a minimal `PATH`, so the owner's login shell
//! and a fixed list of package-manager directories are consulted as well.

use std::ffi::{OsStr, OsString};
use std::io::{self, Read};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::OnceLock;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use rustix::process::{Pid, Signal, kill_process_group};
use tracing::warn;

const SHELL_ENV: &str = "SHELL";
const HOME_ENV: &str = "HOME";
const POLL_INTERVAL: Duration = Duration::from_millis(10);
/// Printed around `$PATH` so output from the shell's startup files is ignored.
const PROBE_MARKER: &str = "__POHUNEK_COMPANION_PATH__";
const PROBE_SCRIPT: &str =
    "printf '__POHUNEK_COMPANION_PATH__%s__POHUNEK_COMPANION_PATH__' \"$PATH\"";

/// Directories searched after the inherited and login-shell `PATH` on macOS.
/// `~` stands for the owner's home directory.
const MACOS_FALLBACK_DIRS: [&str; 3] = ["/opt/homebrew/bin", "/usr/local/bin", "~/.local/bin"];

/// Composes the program search path. Relative and empty entries are skipped and
/// duplicates dropped, first occurrence wins. On `darwin` the order is the
/// inherited `PATH`, the login shell's `PATH`, then `fallbacks`; elsewhere only
/// the inherited `PATH` counts.
pub fn compose_search_path(
    inherited: Option<&OsStr>,
    login_shell: Option<&OsStr>,
    fallbacks: &[PathBuf],
    darwin: bool,
) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut push = |dir: PathBuf| {
        if dir.is_absolute() && !dirs.contains(&dir) {
            dirs.push(dir);
        }
    };
    if let Some(path) = inherited {
        std::env::split_paths(path).for_each(&mut push);
    }
    if darwin {
        if let Some(path) = login_shell {
            std::env::split_paths(path).for_each(&mut push);
        }
        fallbacks.iter().cloned().for_each(push);
    }
    dirs
}

/// The search path for this process. A successful login-shell probe is kept
/// for the rest of the run; a failed one is retried on the next call so a slow
/// shell start does not disable the login `PATH` until restart.
pub fn search_path(login_shell_timeout: Duration) -> Vec<PathBuf> {
    static PROBED: OnceLock<Vec<PathBuf>> = OnceLock::new();
    if let Some(path) = PROBED.get() {
        return path.clone();
    }
    let darwin = cfg!(target_os = "macos");
    let login = darwin
        .then(|| probe_login_shell(login_shell_timeout))
        .flatten();
    let path = compose_search_path(
        std::env::var_os("PATH").as_deref(),
        login.as_deref(),
        &fallback_dirs(std::env::var_os(HOME_ENV).as_deref()),
        darwin,
    );
    if login.is_some() || !darwin {
        let _ = PROBED.set(path.clone());
    }
    path
}

fn fallback_dirs(home: Option<&OsStr>) -> Vec<PathBuf> {
    MACOS_FALLBACK_DIRS
        .iter()
        .filter_map(|dir| match dir.strip_prefix("~/") {
            Some(rest) => home
                .filter(|home| !home.is_empty())
                .map(|home| Path::new(home).join(rest)),
            None => Some(PathBuf::from(dir)),
        })
        .collect()
}

/// Asks the owner's login shell (`$SHELL`, an absolute path) for its `PATH`.
/// Bounded by `timeout`; any failure is logged and yields `None`.
fn probe_login_shell(timeout: Duration) -> Option<OsString> {
    let shell = PathBuf::from(std::env::var_os(SHELL_ENV).filter(|s| !s.is_empty())?);
    if !shell.is_absolute() {
        warn!(shell = %shell.display(), "login shell probe skipped: SHELL is not absolute");
        return None;
    }
    match run_probe(&shell, timeout) {
        Ok(path) => path,
        Err(err) => {
            warn!(shell = %shell.display(), error = %err, "login shell probe failed");
            None
        }
    }
}

fn run_probe(shell: &Path, timeout: Duration) -> Result<Option<OsString>, String> {
    let mut child = Command::new(shell)
        .args(["-l", "-c", PROBE_SCRIPT])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .map_err(|err| err.to_string())?;
    let mut stdout = child.stdout.take().ok_or("no stdout pipe")?;
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(read_marked(&mut stdout));
    });
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break,
            Ok(Some(status)) => return Err(format!("shell exited with {status}")),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(POLL_INTERVAL),
            Ok(None) => {
                kill_group(&child);
                let _ = child.wait();
                return Err(format!("shell did not answer within {timeout:?}"));
            }
            Err(err) => return Err(err.to_string()),
        }
    }
    // The reader stops at the marked value, so background jobs of the shell
    // that keep the pipe open do not delay the result.
    rx.recv_timeout(timeout)
        .map_err(|err| err.to_string())?
        .map_err(|err| err.to_string())
}

/// Kills the probe shell and everything it started in its process group.
fn kill_group(child: &Child) {
    let killed = i32::try_from(child.id())
        .ok()
        .and_then(Pid::from_raw)
        .map(|pid| kill_process_group(pid, Signal::KILL));
    if !matches!(killed, Some(Ok(()))) {
        warn!("cannot kill the login shell probe process group");
    }
}

/// Reads `reader` until the marked value is complete or the stream ends.
fn read_marked(reader: &mut impl Read) -> io::Result<Option<OsString>> {
    let mut out = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let read = reader.read(&mut chunk)?;
        if read == 0 {
            return Ok(extract_marked(&out));
        }
        out.extend_from_slice(&chunk[..read]);
        if let Some(value) = extract_marked(&out) {
            return Ok(Some(value));
        }
    }
}

/// The bytes between the last two probe markers, if present and non-empty.
fn extract_marked(out: &[u8]) -> Option<OsString> {
    let marker = PROBE_MARKER.as_bytes();
    let end = out.windows(marker.len()).rposition(|w| w == marker)?;
    let start = out[..end]
        .windows(marker.len())
        .rposition(|w| w == marker)?
        + marker.len();
    let value = &out[start..end];
    (!value.is_empty()).then(|| OsStr::from_bytes(value).to_owned())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn paths(dirs: &[&str]) -> Vec<PathBuf> {
        dirs.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn relative_and_empty_entries_are_skipped_and_duplicates_dropped() {
        let got = compose_search_path(Some("/a::rel/dir:./x:/b:/a:/b".as_ref()), None, &[], false);
        assert_eq!(got, paths(&["/a", "/b"]));
    }

    #[test]
    fn darwin_order_is_inherited_then_login_shell_then_fallbacks() {
        let got = compose_search_path(
            Some("/usr/bin:/bin".as_ref()),
            Some("/opt/homebrew/bin:/usr/bin:rel:/Users/o/bin".as_ref()),
            &paths(&["/opt/homebrew/bin", "/usr/local/bin", "relative"]),
            true,
        );
        assert_eq!(
            got,
            paths(&[
                "/usr/bin",
                "/bin",
                "/opt/homebrew/bin",
                "/Users/o/bin",
                "/usr/local/bin"
            ])
        );
    }

    #[test]
    fn other_hosts_use_only_the_inherited_path() {
        let got = compose_search_path(
            Some("/usr/bin".as_ref()),
            Some("/login".as_ref()),
            &paths(&["/fallback"]),
            false,
        );
        assert_eq!(got, paths(&["/usr/bin"]));
    }

    #[test]
    fn missing_inputs_give_an_empty_or_fallback_only_path() {
        assert!(compose_search_path(None, None, &paths(&["/f"]), false).is_empty());
        assert_eq!(
            compose_search_path(None, None, &paths(&["/f"]), true),
            paths(&["/f"])
        );
    }

    #[test]
    fn fallback_home_entry_needs_an_absolute_home_to_be_used() {
        let dirs = fallback_dirs(Some("/Users/o".as_ref()));
        assert!(dirs.contains(&PathBuf::from("/Users/o/.local/bin")));
        assert_eq!(fallback_dirs(None).len(), 2);
    }

    #[test]
    fn probe_output_is_extracted_between_markers_ignoring_shell_noise() {
        let out = format!("motd\n{PROBE_MARKER}/a:/b{PROBE_MARKER}");
        assert_eq!(extract_marked(out.as_bytes()), Some("/a:/b".into()));
        assert_eq!(extract_marked(b"no markers"), None);
        assert_eq!(
            extract_marked(format!("{PROBE_MARKER}{PROBE_MARKER}").as_bytes()),
            None
        );
    }

    /// Runs the probe, retrying while a freshly written script is still held
    /// open for writing by a child that a parallel test forked (ETXTBSY).
    fn run_probe_past_busy_script(
        shell: &Path,
        timeout: Duration,
    ) -> Result<Option<OsString>, String> {
        let mut attempts = 0;
        loop {
            let result = run_probe(shell, timeout);
            attempts += 1;
            match &result {
                Err(err) if err.contains("Text file busy") && attempts < 50 => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                _ => return result,
            }
        }
    }

    #[test]
    fn probe_runs_a_shell_and_times_out_a_hung_one() {
        let path = run_probe(Path::new("/bin/sh"), Duration::from_secs(5)).unwrap();
        assert!(path.is_some());

        let dir = std::env::temp_dir().join(format!("ph-probe-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let hung = dir.join("hung-shell");
        std::fs::write(&hung, "#!/bin/sh\nexec sleep 30\n").unwrap();
        std::fs::set_permissions(&hung, std::fs::Permissions::from_mode(0o700)).unwrap();
        let noisy = dir.join("noisy-shell");
        std::fs::write(
            &noisy,
            format!("#!/bin/sh\nsleep 30 &\nprintf '{PROBE_MARKER}/x:/y{PROBE_MARKER}'\n"),
        )
        .unwrap();
        std::fs::set_permissions(&noisy, std::fs::Permissions::from_mode(0o700)).unwrap();
        let got = run_probe_past_busy_script(&noisy, Duration::from_secs(5)).unwrap();
        assert_eq!(got, Some("/x:/y".into()));
        let started = Instant::now();
        let err = run_probe_past_busy_script(&hung, Duration::from_millis(100)).unwrap_err();
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(err.contains("did not answer"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}
