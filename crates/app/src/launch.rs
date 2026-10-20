//! Starts the new-session and assistant dialogs as their own processes.

use std::process::{Command, Stdio};

use pohunek_companion_core::{DialogHost, encode_dialog_hosts};
use tracing::{info, warn};

/// Subcommand that runs the dialog (see `new_session`).
pub const NEW_SESSION_COMMAND: &str = "new-session";
/// Subcommand that runs the assistant dialog (see `assistant`).
pub const ASSISTANT_COMMAND: &str = "assistant";

/// Starts `<current exe> new-session [--host <target>] [--project <id>]
/// [--group <id>] [--known-hosts <json>]` without a shell and returns once the process exists; each
/// value is a single argv word.
/// A thread reaps the child and logs a non-zero exit.
pub fn spawn_new_session(
    host: Option<&str>,
    project: Option<&str>,
    group: Option<&str>,
    known_hosts: Option<&[DialogHost]>,
) -> Result<(), String> {
    let flags = [("--host", host), ("--project", project), ("--group", group)];
    spawn_dialog(
        NEW_SESSION_COMMAND,
        "new-session dialog",
        &flags,
        known_hosts,
    )
}

/// Starts `<current exe> assistant [--host <target>] [--project <id>]
/// [--known-hosts <json>]` the same way.
pub fn spawn_assistant(
    host: Option<&str>,
    project: Option<&str>,
    known_hosts: Option<&[DialogHost]>,
) -> Result<(), String> {
    let flags = [("--host", host), ("--project", project)];
    spawn_dialog(ASSISTANT_COMMAND, "assistant dialog", &flags, known_hosts)
}

fn spawn_dialog(
    subcommand: &str,
    what: &str,
    flags: &[(&str, Option<&str>)],
    known_hosts: Option<&[DialogHost]>,
) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|err| format!("Cannot start the {what}: {err}"))?;
    // Without the hosts the dialog discovers them itself.
    let known_hosts = known_hosts.and_then(|hosts| match encode_dialog_hosts(hosts) {
        Ok(json) => Some(json),
        Err(err) => {
            warn!(error = %err, "pass no hosts to the {what}");
            None
        }
    });
    let child = command(&exe, subcommand, flags, known_hosts.as_deref())
        .spawn()
        .map_err(|err| format!("Cannot start the {what}: {err}"))?;
    let pid = child.id();
    info!(
        pid,
        flags = ?flags,
        "{what} process started"
    );
    let what = what.to_owned();
    std::thread::spawn(move || reap(child, pid, &what));
    Ok(())
}

fn command(
    exe: &std::path::Path,
    subcommand: &str,
    flags: &[(&str, Option<&str>)],
    known_hosts: Option<&str>,
) -> Command {
    let mut command = Command::new(exe);
    command.arg(subcommand);
    for (flag, value) in flags {
        if let Some(value) = value {
            command.arg(flag).arg(value);
        }
    }
    if let Some(known_hosts) = known_hosts {
        command.arg("--known-hosts").arg(known_hosts);
    }
    detach(&mut command);
    command
}

fn detach(command: &mut Command) {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // Its own process group keeps a terminal's Ctrl+C or hangup for the
    // sidebar from reaching an open dialog.
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(command, 0);
}

fn reap(mut child: std::process::Child, pid: u32, what: &str) {
    match child.wait() {
        Ok(status) if status.success() => info!(pid, "{what} exited"),
        Ok(status) => warn!(pid, %status, "{what} exited with a failure"),
        Err(err) => warn!(pid, error = %err, "wait for the {what}"),
    }
}
