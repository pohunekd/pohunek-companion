//! Opening a session in the owner's terminal through the configured argv
//! template. Session data reaches the terminal as argv elements only; no shell
//! ever sees it.

use std::io;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use pohunek_client::protocol::SessionState;
use serde::{Deserialize, Deserializer};
use tracing::{error, info};

use crate::attach_macos::{AttachTerminal, open_command, stage_script};
use crate::attach_path::search_path;
use crate::session::SessionKey;

const BIN_PLACEHOLDER: &str = "bin";
const HOST_PLACEHOLDER: &str = "host";
const ID_PLACEHOLDER: &str = "id";
const POLL_INTERVAL: Duration = Duration::from_millis(10);
const HOME_ENV: &str = "HOME";
/// Staged Terminal.app scripts, relative to the owner's home directory.
const SCRIPT_DIR: &str = "Library/Caches/pohunek-companion/attach";

/// Why a session cannot be attached; shown to the owner as is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum AttachRefusal {
    #[error("external sessions are read-only in Pohunek and have no terminal to attach to")]
    External,
    #[error("the session has ended and has no live terminal to attach to")]
    Ended,
    #[error("the session is still starting; attach once it is running")]
    Starting,
}

impl AttachRefusal {
    /// `None` when a live, daemon-owned session can be attached. `external`
    /// is the protocol's optional flag; an absent flag means a daemon-owned
    /// session.
    pub fn for_session(external: Option<bool>, state: SessionState) -> Option<Self> {
        if external == Some(true) {
            return Some(Self::External);
        }
        match state {
            SessionState::Running => None,
            SessionState::Starting => Some(Self::Starting),
            SessionState::Stopped | SessionState::Done | SessionState::Failed => Some(Self::Ended),
        }
    }
}

/// Why the attach settings or an attach attempt failed.
#[derive(Debug, thiserror::Error)]
pub enum AttachError {
    #[error("neither attach_command nor attach_terminal is set in {}", .config.display())]
    NotConfigured { config: PathBuf },
    #[error("cannot find `{program}` on PATH")]
    NotOnPath { program: String },
    #[error("`{}` is not an executable file", .path.display())]
    NotExecutable { path: PathBuf },
    #[error("`{program}` must be an absolute path or a bare name resolved on PATH")]
    RelativePath { program: String },
    #[error("cannot start `{}`: {source}", .program.display())]
    Spawn {
        program: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("`{}` exited immediately ({status})", .program.display())]
    ExitedEarly { program: PathBuf, status: String },
    #[error("attach_terminal opens Terminal.app and works on macOS only")]
    TerminalUnsupported,
    #[error("cannot stage the Terminal.app script: {0}")]
    Script(#[source] io::Error),
    #[error("cannot watch `{}`: {source}", .program.display())]
    Watch {
        program: PathBuf,
        #[source]
        source: io::Error,
    },
}

/// One word of the attach command with its placeholders located.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Piece {
    Literal(String),
    Bin,
    Host,
    Id,
}

/// Parsed `attach_command`: a program word followed by argument words, each
/// split into literal text and placeholders.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachTemplate {
    words: Vec<Vec<Piece>>,
}

impl AttachTemplate {
    /// Splits each word of `words` into literals and the `{bin}`, `{host}`,
    /// `{id}` placeholders. Any other `{name}` is rejected; braces that do not
    /// enclose an identifier stay literal.
    pub fn parse(words: &[String]) -> Result<Self, String> {
        if words.is_empty() {
            return Err("must contain at least the program word".to_owned());
        }
        if words[0].is_empty() {
            return Err("the program word must not be empty".to_owned());
        }
        words
            .iter()
            .map(|word| parse_word(word))
            .collect::<Result<Vec<_>, _>>()
            .map(|words| Self { words })
    }

    fn uses_bin(&self) -> bool {
        self.words.iter().flatten().any(|p| *p == Piece::Bin)
    }

    /// Renders the argv in one left-to-right pass per word; inserted values
    /// are never rescanned.
    fn render(&self, bin: &str, host: &str, id: &str) -> Vec<String> {
        self.words
            .iter()
            .map(|word| {
                word.iter()
                    .map(|piece| match piece {
                        Piece::Literal(text) => text.as_str(),
                        Piece::Bin => bin,
                        Piece::Host => host,
                        Piece::Id => id,
                    })
                    .collect()
            })
            .collect()
    }
}

fn parse_word(word: &str) -> Result<Vec<Piece>, String> {
    let mut pieces = Vec::new();
    let mut literal = String::new();
    let mut rest = word;
    while let Some(open) = rest.find('{') {
        literal.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let name_len = after
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or(after.len());
        let name = &after[..name_len];
        let is_placeholder = after[name_len..].starts_with('}')
            && name.chars().next().is_some_and(|c| !c.is_ascii_digit());
        if !is_placeholder {
            literal.push('{');
            rest = after;
            continue;
        }
        let piece = match name {
            BIN_PLACEHOLDER => Piece::Bin,
            HOST_PLACEHOLDER => Piece::Host,
            ID_PLACEHOLDER => Piece::Id,
            other => {
                return Err(format!(
                    "unknown placeholder `{{{other}}}` (known: {{{BIN_PLACEHOLDER}}}, \
                     {{{HOST_PLACEHOLDER}}}, {{{ID_PLACEHOLDER}}})"
                ));
            }
        };
        if !literal.is_empty() {
            pieces.push(Piece::Literal(std::mem::take(&mut literal)));
        }
        pieces.push(piece);
        rest = &after[name_len + 1..];
    }
    literal.push_str(rest);
    if !literal.is_empty() {
        pieces.push(Piece::Literal(literal));
    }
    Ok(pieces)
}

impl<'de> Deserialize<'de> for AttachTemplate {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let words = Vec::<String>::deserialize(deserializer)?;
        Self::parse(&words)
            .map_err(|message| serde::de::Error::custom(format!("attach_command: {message}")))
    }
}

/// Settings of the `attach_*` keys and `pohunek_bin` in `companion.toml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachConfig {
    pub command: Option<AttachTemplate>,
    pub terminal: Option<AttachTerminal>,
    pub pohunek_bin: String,
    pub observe: Duration,
    pub login_shell_timeout: Duration,
    pub script_max_age: Duration,
}

/// Resolves a program word: an absolute path as is, a bare name in the search
/// path (see `attach_path`); the result is an absolute path to an executable
/// file.
pub(crate) fn resolve_program(
    program: &str,
    login_shell_timeout: Duration,
) -> Result<PathBuf, AttachError> {
    if Path::new(program).is_absolute() || program.contains('/') {
        return resolve_program_in(program, &[]);
    }
    resolve_program_in(program, &search_path(login_shell_timeout))
}

fn resolve_program_in(program: &str, search: &[PathBuf]) -> Result<PathBuf, AttachError> {
    let candidate = Path::new(program);
    if candidate.is_absolute() {
        return check_executable(candidate.to_owned());
    }
    if program.contains('/') {
        return Err(AttachError::RelativePath {
            program: program.to_owned(),
        });
    }
    let not_found = || AttachError::NotOnPath {
        program: program.to_owned(),
    };
    for dir in search {
        let full = dir.join(program);
        if is_executable_file(&full) {
            return Ok(full);
        }
    }
    Err(not_found())
}

fn is_executable_file(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

fn check_executable(path: PathBuf) -> Result<PathBuf, AttachError> {
    if is_executable_file(&path) {
        Ok(path)
    } else {
        Err(AttachError::NotExecutable { path })
    }
}

/// Opens `key` in the configured terminal. Blocks for at most the
/// observation window: a program that cannot start, or exits unsuccessfully
/// within it, is an error; one still running (or exited with success) counts
/// as launched.
pub fn attach(
    config: &AttachConfig,
    config_path: &Path,
    key: &SessionKey,
) -> Result<(), AttachError> {
    let outcome = launch(config, config_path, key);
    match &outcome {
        Ok(program) => info!(
            host = %key.host,
            session_id = %key.session_id,
            program = %program.display(),
            outcome = "launched",
            "attach"
        ),
        Err(err) => error!(
            host = %key.host,
            session_id = %key.session_id,
            error = %err,
            outcome = "failed",
            "attach"
        ),
    }
    outcome.map(|_| ())
}

fn launch(
    config: &AttachConfig,
    config_path: &Path,
    key: &SessionKey,
) -> Result<PathBuf, AttachError> {
    if let Some(terminal) = config.terminal {
        return launch_terminal(config, terminal, key);
    }
    let template = config
        .command
        .as_ref()
        .ok_or_else(|| AttachError::NotConfigured {
            config: config_path.to_owned(),
        })?;
    let bin = if template.uses_bin() {
        resolve_program(&config.pohunek_bin, config.login_shell_timeout)?
            .to_string_lossy()
            .into_owned()
    } else {
        String::new()
    };
    let argv = template.render(&bin, &key.host, &key.session_id);
    let program = resolve_program(&argv[0], config.login_shell_timeout)?;
    let child = spawn_detached(&program, &argv[1..])?;
    observe(child, &program, config.observe)?;
    Ok(program)
}

/// Stages the attach script and asks `open` to run it in the terminal app.
fn launch_terminal(
    config: &AttachConfig,
    terminal: AttachTerminal,
    key: &SessionKey,
) -> Result<PathBuf, AttachError> {
    if !cfg!(target_os = "macos") {
        return Err(AttachError::TerminalUnsupported);
    }
    let bin = resolve_program(&config.pohunek_bin, config.login_shell_timeout)?;
    let home = std::env::var_os(HOME_ENV)
        .map(PathBuf::from)
        .filter(|home| home.is_absolute())
        .ok_or_else(|| {
            AttachError::Script(io::Error::other("HOME is not set to an absolute path"))
        })?;
    let script = stage_script(
        &home.join(SCRIPT_DIR),
        config.script_max_age,
        &bin,
        &key.host,
        &key.session_id,
    )
    .map_err(AttachError::Script)?;
    info!(script = %script.display(), "stage the Terminal.app attach script");
    let (open, args) = open_command(terminal, &script);
    let open = PathBuf::from(open);
    let child = spawn_detached(&open, &args)?;
    observe(child, &open, config.observe)?;
    Ok(open)
}

pub(crate) fn spawn_detached<S: AsRef<std::ffi::OsStr>>(
    program: &Path,
    args: &[S],
) -> Result<Child, AttachError> {
    Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .map_err(|source| AttachError::Spawn {
            program: program.to_owned(),
            source,
        })
}

/// Waits up to `window` for an early exit. A child that outlives the window
/// is reaped by a background thread once it exits.
pub(crate) fn observe(
    mut child: Child,
    program: &Path,
    window: Duration,
) -> Result<(), AttachError> {
    let deadline = Instant::now() + window;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => {
                return Err(AttachError::ExitedEarly {
                    program: program.to_owned(),
                    status: describe_status(status),
                });
            }
            Ok(None) if Instant::now() < deadline => std::thread::sleep(POLL_INTERVAL),
            Ok(None) => {
                std::thread::spawn(move || {
                    if let Err(err) = child.wait() {
                        error!(error = %err, "reap the attach process");
                    }
                });
                return Ok(());
            }
            Err(source) => {
                return Err(AttachError::Watch {
                    program: program.to_owned(),
                    source,
                });
            }
        }
    }
}

fn describe_status(status: ExitStatus) -> String {
    match (status.code(), status.signal()) {
        (Some(code), _) => format!("exit code {code}"),
        (None, Some(signal)) => format!("signal {signal}"),
        (None, None) => status.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn template(words: &[&str]) -> AttachTemplate {
        let words: Vec<String> = words.iter().map(|w| (*w).to_owned()).collect();
        AttachTemplate::parse(&words).unwrap()
    }

    #[test]
    fn hostile_values_stay_single_unchanged_elements() {
        let t = template(&["term", "--", "{bin}", "attach", "--host", "{host}", "{id}"]);
        for id in [
            "a b",
            "it's \"q\"",
            "x; rm -rf /",
            "$(touch /tmp/pwn)",
            "`id`",
            "{id}",
            "{host}{bin}",
        ] {
            let argv = t.render("/usr/bin/pohunek", "local", id);
            assert_eq!(
                argv,
                [
                    "term",
                    "--",
                    "/usr/bin/pohunek",
                    "attach",
                    "--host",
                    "local",
                    id
                ]
            );
        }
    }

    #[test]
    fn placeholders_inside_a_word_and_literal_braces() {
        let t = template(&["t", "{host}/{id}", "{}", "{ id }", "{1}", "a{", "{id"]);
        assert_eq!(
            t.render("b", "h", "i"),
            ["t", "h/i", "{}", "{ id }", "{1}", "a{", "{id"]
        );
    }

    #[test]
    fn values_with_braces_are_not_rescanned() {
        let t = template(&["t", "{host}{id}"]);
        assert_eq!(t.render("b", "{id}", "{host}"), ["t", "{id}{host}"]);
    }

    #[test]
    fn unknown_placeholder_is_rejected() {
        let err = AttachTemplate::parse(&["t".into(), "{sessoin}".into()]).unwrap_err();
        assert!(err.contains("{sessoin}"), "{err}");
    }

    #[test]
    fn empty_template_and_empty_program_are_rejected() {
        assert!(AttachTemplate::parse(&[]).is_err());
        assert!(AttachTemplate::parse(&[String::new(), "x".into()]).is_err());
    }

    #[test]
    fn bare_names_resolve_on_path_and_relative_paths_are_rejected() {
        let search: Vec<PathBuf> = ["/nonexistent", "/usr/bin", "/bin"]
            .map(PathBuf::from)
            .into();
        let sh = resolve_program_in("sh", &search).unwrap();
        assert!(sh.is_absolute());
        assert!(matches!(
            resolve_program_in("no-such-program-xyz", &search),
            Err(AttachError::NotOnPath { .. })
        ));
        assert!(matches!(
            resolve_program_in("./term", &search),
            Err(AttachError::RelativePath { .. })
        ));
        assert!(matches!(
            resolve_program_in("/no/such/term", &[]),
            Err(AttachError::NotExecutable { .. })
        ));
    }

    #[test]
    fn external_and_ended_sessions_are_refused() {
        assert_eq!(
            AttachRefusal::for_session(Some(true), SessionState::Running),
            Some(AttachRefusal::External)
        );
        assert_eq!(
            AttachRefusal::for_session(None, SessionState::Done),
            Some(AttachRefusal::Ended)
        );
        assert_eq!(
            AttachRefusal::for_session(Some(false), SessionState::Running),
            None
        );
    }
}
