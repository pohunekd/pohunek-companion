//! Opening a session in Terminal.app: a generated `.command` script that runs
//! `pohunek attach`, started through `open -a`. The script and the `open`
//! argv carry session data only as quoted bytes or single argv elements; no
//! AppleScript and no shell text built elsewhere is involved.

use std::ffi::OsStr;
use std::fs::{DirBuilder, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Deserializer};

const OPEN_PROGRAM: &str = "/usr/bin/open";
const SCRIPT_PREFIX: &str = "attach-";
const SCRIPT_SUFFIX: &str = ".command";
const OWNER_ONLY_DIR: u32 = 0o700;
const OWNER_ONLY_EXEC: u32 = 0o700;
const GROUP_OTHER_BITS: u32 = 0o077;

/// Terminal apps `attach_terminal` can name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachTerminal {
    /// Terminal.app (`attach_terminal = "terminal"`).
    Terminal,
}

impl AttachTerminal {
    /// Application name handed to `open -a`.
    fn app_name(self) -> &'static str {
        match self {
            Self::Terminal => "Terminal",
        }
    }
}

impl<'de> Deserialize<'de> for AttachTerminal {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match String::deserialize(deserializer)?.as_str() {
            "terminal" => Ok(Self::Terminal),
            other => Err(serde::de::Error::custom(format!(
                "attach_terminal: unknown terminal `{other}` (known: terminal)"
            ))),
        }
    }
}

/// Single-quotes `value` for a POSIX shell so it reads back byte for byte;
/// embedded single quotes are written as `'\''`. The caller rejects NUL.
fn shell_quote(value: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(value.len() + 2);
    out.push(b'\'');
    for &byte in value {
        if byte == b'\'' {
            out.extend_from_slice(b"'\\''");
        } else {
            out.push(byte);
        }
    }
    out.push(b'\'');
    out
}

/// The script body: replaces the shell with `pohunek attach` for the session.
fn render_script(bin: &Path, host: &str, id: &str) -> Result<Vec<u8>, io::Error> {
    let words: [&[u8]; 5] = [
        bin.as_os_str().as_bytes(),
        b"attach",
        b"--host",
        host.as_bytes(),
        id.as_bytes(),
    ];
    if words.iter().any(|word| word.contains(&0)) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "value contains a NUL byte",
        ));
    }
    let mut script = b"#!/bin/sh\nexec".to_vec();
    for word in words {
        script.push(b' ');
        script.extend(shell_quote(word));
    }
    script.push(b'\n');
    Ok(script)
}

/// Creates `dir` (and parents) when missing and requires it to be a real
/// directory owned by the current user with no group or other access.
fn ensure_private_dir(dir: &Path) -> io::Result<()> {
    DirBuilder::new()
        .recursive(true)
        .mode(OWNER_ONLY_DIR)
        .create(dir)?;
    let meta = std::fs::symlink_metadata(dir)?;
    if !meta.is_dir() {
        return Err(io::Error::other("script directory is not a directory"));
    }
    if meta.uid() != rustix::process::geteuid().as_raw() {
        return Err(io::Error::other("script directory is not owned by you"));
    }
    if meta.mode() & GROUP_OTHER_BITS != 0 {
        std::fs::set_permissions(
            dir,
            std::os::unix::fs::PermissionsExt::from_mode(OWNER_ONLY_DIR),
        )?;
    }
    Ok(())
}

/// Removes staged scripts in `dir` older than `max_age`; other files are left
/// alone. Errors on single entries are ignored: cleanup is best effort.
fn remove_stale_scripts(dir: &Path, max_age: Duration) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !(name.starts_with(SCRIPT_PREFIX) && name.ends_with(SCRIPT_SUFFIX)) {
            continue;
        }
        let stale = entry
            .metadata()
            .ok()
            .filter(|meta| meta.is_file())
            .and_then(|meta| meta.modified().ok())
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age > max_age);
        if stale {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

fn create_script_file(dir: &Path) -> io::Result<(File, PathBuf)> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let name = format!(
        "{SCRIPT_PREFIX}{}-{nanos}-{}{SCRIPT_SUFFIX}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    let path = dir.join(name);
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(OWNER_ONLY_EXEC)
        .open(&path)?;
    Ok((file, path))
}

/// Stages the attach script for `host`/`id` in `dir` and returns its path.
/// Scripts older than `max_age` are removed first.
pub fn stage_script(
    dir: &Path,
    max_age: Duration,
    bin: &Path,
    host: &str,
    id: &str,
) -> io::Result<PathBuf> {
    let script = render_script(bin, host, id)?;
    ensure_private_dir(dir)?;
    remove_stale_scripts(dir, max_age);
    let (mut file, path) = create_script_file(dir)?;
    if let Err(err) = file.write_all(&script).and_then(|()| file.sync_all()) {
        let _ = std::fs::remove_file(&path);
        return Err(err);
    }
    Ok(path)
}

/// The `open` invocation: program and argv (the script path is one element).
pub fn open_command(terminal: AttachTerminal, script: &Path) -> (&'static str, Vec<&OsStr>) {
    (
        OPEN_PROGRAM,
        vec![
            OsStr::new("-a"),
            OsStr::new(terminal.app_name()),
            script.as_os_str(),
        ],
    )
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;

    use super::*;

    const HOSTILE: [&str; 8] = [
        "plain",
        "with space",
        "it's \"quoted\"",
        "$HOME and ${PATH} and $(id)",
        "`id` back`tick",
        "line1\nline2\n",
        "'; rm -rf / #",
        "back\\slash\\n and \ttab",
    ];

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ph-macos-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn quoted_values_read_back_byte_for_byte() {
        let mut script = b"printf '%s\\0'".to_vec();
        for value in HOSTILE {
            script.push(b' ');
            script.extend(shell_quote(value.as_bytes()));
        }
        let out = Command::new("sh")
            .arg("-c")
            .arg(OsStr::from_bytes(&script))
            .output()
            .unwrap();
        let got: Vec<&[u8]> = out.stdout.split(|b| *b == 0).collect();
        let expected: Vec<&[u8]> = HOSTILE.iter().map(|v| v.as_bytes()).collect();
        assert_eq!(&got[..HOSTILE.len()], expected.as_slice());
    }

    #[test]
    fn staged_script_parses_and_runs_with_exact_arguments() {
        let dir = scratch("script");
        let dump = dir.join("argv.bin");
        let fake_bin = dir.join("fake pohunek's bin");
        let body = format!("#!/bin/sh\nprintf '%s\\0' \"$@\" > '{}'\n", dump.display());
        std::fs::write(&fake_bin, body).unwrap();
        std::fs::set_permissions(&fake_bin, std::fs::Permissions::from_mode(0o700)).unwrap();

        for (host, id) in HOSTILE
            .iter()
            .flat_map(|h| HOSTILE.iter().map(move |i| (h, i)))
            .take(64)
        {
            let script_dir = dir.join("scripts");
            let path =
                stage_script(&script_dir, Duration::from_secs(3600), &fake_bin, host, id).unwrap();
            let syntax = Command::new("sh").arg("-n").arg(&path).status().unwrap();
            assert!(syntax.success());
            assert!(Command::new(&path).status().unwrap().success());
            let raw = std::fs::read(&dump).unwrap();
            let got: Vec<&[u8]> = raw.split(|b| *b == 0).collect();
            assert_eq!(
                &got[..4],
                [
                    b"attach".as_slice(),
                    b"--host".as_slice(),
                    host.as_bytes(),
                    id.as_bytes()
                ]
            );
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn nul_bytes_are_rejected() {
        assert!(render_script(Path::new("/bin/x"), "h\0", "i").is_err());
    }

    #[test]
    fn script_dir_and_files_are_owner_only_and_stale_ones_are_removed() {
        let dir = scratch("perm");
        let script_dir = dir.join("a").join("scripts");
        let old = stage_script(
            &script_dir,
            Duration::from_secs(3600),
            Path::new("/bin/true"),
            "h",
            "1",
        )
        .unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&script_dir), 0o700);
        assert_eq!(mode(&old), 0o700);

        let keep = script_dir.join("unrelated.txt");
        std::fs::write(&keep, "x").unwrap();
        std::thread::sleep(Duration::from_millis(30));
        let fresh = stage_script(
            &script_dir,
            Duration::from_millis(10),
            Path::new("/bin/true"),
            "h",
            "2",
        )
        .unwrap();
        assert!(!old.exists());
        assert!(fresh.exists());
        assert!(keep.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn open_gets_the_script_as_one_argv_element() {
        let script = Path::new("/tmp/a b/attach-1.command");
        let (program, args) = open_command(AttachTerminal::Terminal, script);
        assert_eq!(program, "/usr/bin/open");
        assert_eq!(
            args,
            [OsStr::new("-a"), OsStr::new("Terminal"), script.as_os_str()]
        );
    }
}
