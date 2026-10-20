//! Starts the new-session dialog as its own process.

use std::process::{Command, Stdio};

use pohunek_companion_core::{DialogHost, encode_dialog_hosts};
use tracing::{info, warn};

/// Subcommand that runs the dialog (see `new_session`).
pub const NEW_SESSION_COMMAND: &str = "new-session";

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
    let exe = std::env::current_exe()
        .map_err(|err| format!("Cannot start the new-session dialog: {err}"))?;
    // Without the hosts the dialog discovers them itself.
    let known_hosts = known_hosts.and_then(|hosts| match encode_dialog_hosts(hosts) {
        Ok(json) => Some(json),
        Err(err) => {
            warn!(error = %err, "pass no hosts to the new-session dialog");
            None
        }
    });
    let child = command(&exe, host, project, group, known_hosts.as_deref())
        .spawn()
        .map_err(|err| format!("Cannot start the new-session dialog: {err}"))?;
    let pid = child.id();
    info!(
        pid,
        host = host.unwrap_or(""),
        project = project.unwrap_or(""),
        group = group.unwrap_or(""),
        "new-session dialog process started"
    );
    std::thread::spawn(move || reap(child, pid, "new-session dialog"));
    Ok(())
}

fn command(
    exe: &std::path::Path,
    host: Option<&str>,
    project: Option<&str>,
    group: Option<&str>,
    known_hosts: Option<&str>,
) -> Command {
    let mut command = Command::new(exe);
    command.arg(NEW_SESSION_COMMAND);
    if let Some(host) = host {
        command.arg("--host").arg(host);
    }
    if let Some(project) = project {
        command.arg("--project").arg(project);
    }
    if let Some(group) = group {
        command.arg("--group").arg(group);
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
