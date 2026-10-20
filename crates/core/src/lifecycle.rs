use pohunek_client::ClientError;
use pohunek_client::protocol::SessionId;
use pohunek_client::protocol::method::{SessionRemove, SessionStop};
use tracing::error;

use crate::config::DaemonConfig;
use crate::connect::{connect_for_session_action, resolve_socket};

/// A daemon mutation that ends or deletes a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleAction {
    /// `session.stop`: ends the agent; the session stays in the list as ended.
    Stop,
    /// `session.remove`: deletes the session from the daemon, stopping it
    /// first when it is still live. The variant that accepts unconfirmed
    /// runtime sweeps is never used: a removal the daemon cannot confirm is
    /// refused and shown.
    Remove,
}

impl LifecycleAction {
    /// Lowercase verb for log lines and error messages.
    pub fn verb(self) -> &'static str {
        match self {
            Self::Stop => "stop",
            Self::Remove => "remove",
        }
    }
}

/// Why a stop or remove did not happen; the message is shown to the owner as is.
#[derive(Debug, thiserror::Error)]
pub enum LifecycleError {
    /// The daemon answered with a typed refusal.
    #[error("{message}")]
    Refused { code: String, message: String },
    #[error("{0}")]
    Failed(String),
}

/// Sends one `session.stop` or `session.remove` for `session_id`.
pub async fn run_lifecycle_action(
    daemon: &DaemonConfig,
    session_id: &str,
    action: LifecycleAction,
) -> Result<(), LifecycleError> {
    let socket = resolve_socket().map_err(|err| LifecycleError::Failed(err.to_string()))?;
    let mut client = connect_for_session_action(&socket, daemon)
        .await
        .map_err(|err| LifecycleError::Failed(err.to_string()))?;
    let id = SessionId(session_id.to_owned());
    let outcome = match action {
        LifecycleAction::Stop => client.call::<SessionStop>(id).await.map(drop),
        LifecycleAction::Remove => client.call::<SessionRemove>(id).await.map(drop),
    };
    match outcome {
        Ok(()) => Ok(()),
        Err(ClientError::Protocol(err)) => {
            error!(code = %err.code, action = action.verb(), "session lifecycle action refused");
            Err(LifecycleError::Refused {
                code: err.code,
                message: err.msg,
            })
        }
        Err(err) => {
            error!(error = %err, action = action.verb(), "session lifecycle action failed");
            Err(LifecycleError::Failed(err.to_string()))
        }
    }
}
