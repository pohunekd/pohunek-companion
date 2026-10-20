//! Starts the new-session dialog as its own process.

use std::process::{Command, Stdio};

use tracing::{info, warn};

/// Subcommand that runs the dialog (see `new_session`).
pub const NEW_SESSION_COMMAND: &str = "new-session";

/// Starts `<current exe> new-session [--project <id>] [--group <id>]` without
/// a shell and returns once the process exists; each id is a single argv word.
/// A thread reaps the child and logs a non-zero exit.
pub fn spawn_new_session(project: Option<&str>, group: Option<&str>) -> Result<(), String> {
    let exe = std::env::current_exe()
        .map_err(|err| format!("Cannot start the new-session dialog: {err}"))?;
    let child = command(&exe, project, group)
        .spawn()
        .map_err(|err| format!("Cannot start the new-session dialog: {err}"))?;
    let pid = child.id();
    info!(
        pid,
        project = project.unwrap_or(""),
        group = group.unwrap_or(""),
        "new-session dialog process started"
    );
    std::thread::spawn(move || reap(child, pid, "new-session dialog"));
    Ok(())
}

fn command(exe: &std::path::Path, project: Option<&str>, group: Option<&str>) -> Command {
    let mut command = Command::new(exe);
    command.arg(NEW_SESSION_COMMAND);
    if let Some(project) = project {
        command.arg("--project").arg(project);
    }
    if let Some(group) = group {
        command.arg("--group").arg(group);
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
