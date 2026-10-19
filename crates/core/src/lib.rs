//! Daemon connection and session model for Pohunek Companion. No UI
//! dependency: the app crate maps these values to its views.

mod badge;
mod connect;
mod error;
mod model;
mod session;
mod socket;

pub use badge::{WorkBadge, clip_display};
pub use connect::fetch_sessions;
pub use error::CoreError;
pub use model::{Applied, IgnoreReason, ReconcileReport, SessionStore};
pub use session::{SessionKey, SessionRow};
pub use socket::{SocketError, local_socket_path};
