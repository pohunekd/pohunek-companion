use pohunek_client::ClientError;

use crate::socket::SocketError;

/// Failure to obtain the session list; its message is shown to the owner as is.
#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error(transparent)]
    Socket(#[from] SocketError),
    #[error("daemon request failed: {0}")]
    Client(#[from] ClientError),
}
