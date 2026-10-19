//! Daemon connection and session model for Pohunek Companion. No UI
//! dependency: the app crate maps these values to its views.

mod connect;
mod error;
mod session;
mod socket;

pub use connect::fetch_sessions;
pub use error::CoreError;
pub use session::{SessionKey, SessionRow};
pub use socket::{SocketError, local_socket_path};
