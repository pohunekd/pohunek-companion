use pohunek_client::ClientError;
use pohunek_client::protocol::SessionId;
use pohunek_client::protocol::SessionRenameParams;
use pohunek_client::protocol::method::SessionRename;
use tracing::error;

use crate::config::DaemonConfig;
use crate::connect::{connect, resolve_socket};

/// Why a rename did not happen; the message is shown to the owner as is.
#[derive(Debug, thiserror::Error)]
pub enum RenameError {
    /// The daemon answered with a typed refusal, for example for an external
    /// read-only session.
    #[error("{message}")]
    Refused { code: String, message: String },
    #[error("{0}")]
    Failed(String),
}

/// Sets the display name of a session through `session.rename`; `None` clears
/// it so the session shows its id. The daemon trims the name and validates it.
pub async fn rename_session(
    daemon: &DaemonConfig,
    session_id: &str,
    name: Option<String>,
) -> Result<(), RenameError> {
    let socket = resolve_socket().map_err(|err| RenameError::Failed(err.to_string()))?;
    let mut client = connect(&socket, daemon)
        .await
        .map_err(|err| RenameError::Failed(err.to_string()))?;
    let params = SessionRenameParams {
        session_id: SessionId(session_id.to_owned()),
        name,
    };
    match client.call::<SessionRename>(params).await {
        Ok(_) => Ok(()),
        Err(ClientError::Protocol(err)) => {
            error!(code = %err.code, "session.rename refused");
            Err(RenameError::Refused {
                code: err.code,
                message: err.msg,
            })
        }
        Err(err) => {
            error!(error = %err, "session.rename failed");
            Err(RenameError::Failed(err.to_string()))
        }
    }
}

/// Maps the editor text to the rename parameter: blank clears the name.
pub fn name_from_input(input: &str) -> Option<String> {
    let trimmed = input.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blank_input_clears_and_text_is_trimmed() {
        assert_eq!(name_from_input("   "), None);
        assert_eq!(name_from_input(""), None);
        assert_eq!(
            name_from_input("  alpha-agent "),
            Some("alpha-agent".to_owned())
        );
    }
}
