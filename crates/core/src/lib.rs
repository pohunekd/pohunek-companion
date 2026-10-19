//! Daemon connection and session model for Pohunek Companion. No UI
//! dependency: the app crate maps these values to its views.

mod attach;
mod attach_macos;
mod attach_path;
mod badge;
mod config;
mod connect;
mod error;
mod filter;
mod layout;
mod live;
mod model;
mod needs_you;
mod owner_state;
mod session;
mod socket;
mod sort_key;
mod tray_summary;
mod view;

pub use attach::{AttachConfig, AttachError, AttachRefusal, AttachTemplate, attach};
pub use attach_macos::AttachTerminal;
pub use badge::{WorkBadge, WorkFacts, clip_display};
pub use config::{
    Config, ConfigError, DaemonConfig, LayoutConfig, PlacementRule, TrayConfig, ViewConfig,
};
pub use error::CoreError;
pub use filter::{SessionFilter, filter_sections};
pub use layout::{
    CreateAndMoveError, GroupError, GroupPlacement, GroupSection, INBOX_ID, LayoutError,
    LayoutStore, MoveError, Placement, Reconciled, RulePlacement, layout_path, state_dir,
};
pub use live::{LiveUpdate, run_live};
pub use model::{Applied, IgnoreReason, ReconcileReport, SessionStore};
pub use needs_you::{NEEDS_YOU_ID, with_needs_you};
pub use owner_state::{OwnerState, derive};
pub use session::{ProjectRef, SessionKey, SessionRow};
pub use socket::{SocketError, local_socket_path};
pub use tray_summary::{TrayEntry, TraySummary};
pub use view::{NO_PROJECT_ID, NO_WORK_ID, View, by_project, by_state, by_work_item};
