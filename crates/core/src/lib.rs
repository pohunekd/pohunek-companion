//! Daemon connection and session model for Pohunek Companion. No UI
//! dependency: the app crate maps these values to its views.

mod attach;
mod attach_macos;
mod attach_path;
mod badge;
mod config;
mod connect;
mod error;
mod live;
mod model;
mod owner_state;
mod session;
mod socket;
mod tray_summary;

pub use attach::{AttachConfig, AttachError, AttachRefusal, AttachTemplate, attach};
pub use attach_macos::AttachTerminal;
pub use badge::{WorkBadge, WorkFacts, clip_display};
pub use config::{Config, ConfigError, DaemonConfig, TrayConfig};
pub use error::CoreError;
pub use live::{LiveUpdate, run_live};
pub use model::{Applied, IgnoreReason, ReconcileReport, SessionStore};
pub use owner_state::{OwnerState, derive};
pub use session::{ProjectRef, SessionKey, SessionRow};
pub use socket::{SocketError, local_socket_path};
pub use tray_summary::{TrayEntry, TraySummary};
