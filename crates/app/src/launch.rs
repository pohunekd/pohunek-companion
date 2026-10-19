//! Starts the new-session dialog as its own process.

use std::process::{Command, Stdio};

use tracing::{info, warn};

/// Subcommand that runs the dialog (see `new_session`).
pub const NEW_SESSION_COMMAND: &str = "new-session";

/// Starts `<current exe> new-session [--project <id>]` without a shell and
/// returns once the process exists; the project id is a single argv word. A
/// thread reaps the child and logs a non-zero exit.
pub fn spawn_new_session(project: Option<&str>) -> Result<(), String> {
    let exe = std::env::current_exe()
        .map_err(|err| format!("Cannot start the new-session dialog: {err}"))?;
    let child = command(&exe, project)
        .spawn()
        .map_err(|err| format!("Cannot start the new-session dialog: {err}"))?;
    let pid = child.id();
    info!(
        pid,
        project = project.unwrap_or(""),
        "new-session dialog process started"
    );
    std::thread::spawn(move || reap(child, pid));
    Ok(())
}

fn command(exe: &std::path::Path, project: Option<&str>) -> Command {
    let mut command = Command::new(exe);
    command.arg(NEW_SESSION_COMMAND);
    if let Some(project) = project {
        command.arg("--project").arg(project);
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // Its own process group keeps a terminal's Ctrl+C or hangup for the
    // sidebar from reaching an open dialog.
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    command
}

fn reap(mut child: std::process::Child, pid: u32) {
    match child.wait() {
        Ok(status) if status.success() => info!(pid, "new-session dialog exited"),
        Ok(status) => warn!(pid, %status, "new-session dialog exited with a failure"),
        Err(err) => warn!(pid, error = %err, "wait for the new-session dialog"),
    }
}
