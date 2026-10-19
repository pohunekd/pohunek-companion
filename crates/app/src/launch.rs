//! Starts the new-session dialog as its own process.

use std::process::{Command, Stdio};

use tracing::{info, warn};

/// Subcommand that runs the dialog (see `new_session`).
pub const NEW_SESSION_COMMAND: &str = "new-session";

/// Starts `<current exe> new-session` without a shell and returns once the
/// process exists. A thread reaps the child and logs a non-zero exit.
pub fn spawn_new_session() -> Result<(), String> {
    let exe = std::env::current_exe()
        .map_err(|err| format!("Cannot start the new-session dialog: {err}"))?;
    let child = command(&exe)
        .spawn()
        .map_err(|err| format!("Cannot start the new-session dialog: {err}"))?;
    let pid = child.id();
    info!(pid, "new-session dialog started from the sidebar");
    std::thread::spawn(move || reap(child, pid));
    Ok(())
}

fn command(exe: &std::path::Path) -> Command {
    let mut command = Command::new(exe);
    command
        .arg(NEW_SESSION_COMMAND)
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
