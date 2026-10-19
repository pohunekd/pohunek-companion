use pohunek_client::ClientError;

use crate::socket::SocketError;

/// Failure to obtain or keep the daemon session data; its message is shown to
/// the owner as is.
#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error(transparent)]
    Socket(#[from] SocketError),
    #[error("daemon request failed: {0}")]
    Client(#[from] ClientError),
    #[error("the daemon closed the event stream")]
    StreamClosed,
    #[error("the daemon rejects a `notification.list` page of even one record as too large")]
    NotificationPageTooLarge,
}
