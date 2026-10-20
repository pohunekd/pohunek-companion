use pohunek_client::ClientError;
use pohunek_client::protocol::SessionId;
use pohunek_client::protocol::method::{
    SessionRemove, SessionRemoveAcceptingUnconfirmed, SessionStop,
};
use tracing::error;

use crate::config::DaemonConfig;
use crate::connect::{connect_for_session_action, socket_for_host};

/// A daemon mutation that ends or deletes a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleAction {
    /// `session.stop`: ends the agent; the session stays in the list as ended.
    Stop,
    /// `session.remove`: deletes the session from the daemon, stopping it
    /// first when it is still live. A removal the daemon cannot confirm is
    /// refused.
    Remove,
    /// `session.remove_accepting_unconfirmed`: like `Remove`, but accepts the
    /// processes with an unreadable environment that are the only reason the
    /// daemon cannot confirm the runtime ended. Those processes are never
    /// signalled and may keep running. Only sent after the owner consented
    /// to this one removal.
    RemoveAcceptingUnconfirmed,
}

impl LifecycleAction {
    /// Lowercase verb for log lines and error messages.
    pub fn verb(self) -> &'static str {
        match self {
            Self::Stop => "stop",
            Self::Remove => "remove",
            Self::RemoveAcceptingUnconfirmed => "remove_accepting_unconfirmed",
        }
    }
}

/// Why a stop or remove did not happen; the message is shown to the owner as is.
#[derive(Debug, thiserror::Error)]
pub enum LifecycleError {
    /// The daemon answered with a typed refusal.
    #[error("{}", refusal_text(.message, .recover))]
    Refused {
        code: String,
        message: String,
        /// The daemon's suggested recovery action.
        recover: Option<String>,
    },
    #[error("{0}")]
    Failed(String),
}

fn refusal_text(message: &str, recover: &Option<String>) -> String {
    match recover.as_deref().filter(|hint| !hint.is_empty()) {
        Some(hint) => format!("{message} ({hint})"),
        None => message.to_owned(),
    }
}

/// Error code of a removal refused because the daemon cannot prove the
/// runtime's processes gone.
const UNCONFIRMED_CODE: &str = "runtime_supervision_ambiguous";

impl LifecycleError {
    /// The daemon refused only because it could not confirm the runtime's
    /// processes ended; `RemoveAcceptingUnconfirmed` can proceed after consent.
    pub fn is_unconfirmed(&self) -> bool {
        matches!(self, Self::Refused { code, .. } if code == UNCONFIRMED_CODE)
    }
}

/// What a successful stop or remove reported.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct LifecycleOutcome {
    /// Worktree checkouts a removal could not delete; the daemon no longer
    /// tracks them, so they need manual cleanup.
    pub worktrees_failed: u32,
    /// Processes an accepting removal left unverified and unsignalled, as
    /// `pid (command)`.
    pub unconfirmed_processes: Vec<String>,
}

/// Sends one `session.stop`, `session.remove` or
/// `session.remove_accepting_unconfirmed` for `session_id` to the daemon of
/// `host` (the local host or a remote host route).
pub async fn run_lifecycle_action(
    daemon: &DaemonConfig,
    host: &str,
    session_id: &str,
    action: LifecycleAction,
) -> Result<LifecycleOutcome, LifecycleError> {
    let socket = socket_for_host(host).map_err(|err| LifecycleError::Failed(err.to_string()))?;
    let mut client = connect_for_session_action(host, &socket, daemon)
        .await
        .map_err(|err| LifecycleError::Failed(err.to_string()))?;
    let id = SessionId(session_id.to_owned());
    let outcome = match action {
        LifecycleAction::Stop => client
            .call::<SessionStop>(id)
            .await
            .map(|_| LifecycleOutcome::default()),
        LifecycleAction::Remove => {
            client
                .call::<SessionRemove>(id)
                .await
                .map(|result| LifecycleOutcome {
                    worktrees_failed: result.worktrees_failed,
                    unconfirmed_processes: Vec::new(),
                })
        }
        LifecycleAction::RemoveAcceptingUnconfirmed => client
            .call::<SessionRemoveAcceptingUnconfirmed>(id)
            .await
            .map(|result| LifecycleOutcome {
                worktrees_failed: result.worktrees_failed,
                unconfirmed_processes: result
                    .accepted_unconfirmed_processes
                    .iter()
                    .map(|process| match &process.command {
                        Some(command) => format!("{} ({command})", process.pid),
                        None => process.pid.to_string(),
                    })
                    .collect(),
            }),
    };
    match outcome {
        Ok(outcome) => Ok(outcome),
        Err(ClientError::Protocol(err)) => {
            error!(code = %err.code, action = action.verb(), "session lifecycle action refused");
            Err(LifecycleError::Refused {
                code: err.code,
                message: err.msg,
                recover: err.recover,
            })
        }
        Err(err) => {
            error!(error = %err, action = action.verb(), "session lifecycle action failed");
            Err(LifecycleError::Failed(err.to_string()))
        }
    }
}
