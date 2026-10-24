//! Daemon connection and session model for Pohunek Companion. No UI
//! dependency: the app crate maps these values to its views.

mod assistant;
mod attach;
mod attach_macos;
mod attach_path;
mod badge;
mod config;
mod connect;
mod detail;
mod discovery;
mod error;
mod filter;
mod group_key;
mod group_write;
mod install;
mod layout;
mod lifecycle;
mod live;
mod model;
mod needs_you;
mod new_session;
mod open;
mod owner_state;
mod pending_placement;
mod rename;
mod resume;
mod routes;
mod session;
mod setup;
mod socket;
mod sort_key;
mod tray_summary;
mod view;

pub use assistant::{
    AssistantForm, AssistantFormError, INTENTS, Intent, assistant_blocker, is_remote_host,
    launch_assistant,
};
pub use attach::{AttachConfig, AttachError, AttachRefusal, AttachTemplate, attach};
pub use attach_macos::AttachTerminal;
pub use badge::{WorkBadge, WorkFacts, clip_display, clip_middle};
pub use config::{
    Config, ConfigError, DaemonConfig, HostConfig, LayoutConfig, NewSessionConfig, PlacementRule,
    SetupConfig, TrayConfig, ViewConfig,
};
pub use connect::host_naming_this_machine;
pub use detail::{DetailField, DetailSection, SessionDetails};
pub use discovery::{
    DialogHost, DialogHostsError, DiscoveredHost, DiscoveryPlan, IncompatibleHost,
    decode_dialog_hosts, dialog_hosts, discover_dialog_hosts, discover_records,
    encode_dialog_hosts, plan as plan_discovery, register_dialog_routes,
};
pub use error::CoreError;
pub use filter::{
    HostConnection, HostEntry, HostOption, ProjectChip, ProjectSelection, SessionFilter, StateChip,
    StateSelection, StoredFilter, filter_sections, fuzzy_matches, host_options, project_chips,
    state_chips, state_from_key, state_key,
};
pub use group_key::{
    GROUP_KEY, GroupNameProblem, GroupValue, INBOX_VALUE, check_group_name, parse_group_value,
};
pub use group_write::{
    GroupWriteError, WriteOutcome, WriteSummary, WriteTarget, set_group, write_all, write_group,
};
pub use install::{
    Attestation, CLIENT_TAG, InstallPlan, Prepared, Release, Step, StepError, plan_for_host,
    prepare_release, run_brew_install, run_release_install,
};
pub use layout::{
    CreateAndMoveError, GroupError, GroupOrigin, GroupPlacement, GroupSection, GroupSync, INBOX_ID,
    LayoutError, LayoutStore, MigrationPlan, MoveError, Placement, Reconciled, RulePlacement,
    layout_path, state_dir,
};
pub use lifecycle::{
    LifecycleAction, LifecycleError, LifecycleOutcome, Resumed, run_lifecycle_action,
};
pub use live::{LiveHost, LiveUpdate, run_live};
pub use model::{Applied, IgnoreReason, ReconcileReport, SessionStore};
pub use needs_you::{NEEDS_YOU_ID, with_needs_you};
pub use new_session::{
    Activity, AgentChoice, Choices, CreateError, FormError, Generation, NewSessionForm,
    ProjectChoice, Recipe, TemplateError, UNCONFIRMED_MESSAGE, build_params, create_session,
    load_choices, load_choices_staged, load_recipe, load_templates, recipe_from_action,
    submit_blocker, template_names,
};
pub use open::{
    OpenConfig, OpenKind, OpenTargets, OpenTemplate, default_open_template, is_openable_url, open,
};
pub use owner_state::{OwnerState, derive};
pub use pending_placement::{Pending, PendingEntry, PendingError, PendingStore, Settled};
pub use pohunek_client::LOCAL_HOST;
pub use pohunek_client::protocol::SessionNewParams;
pub use rename::{RenameError, name_from_input, rename_session};
pub use resume::{PROFILE_CHANGED_CODE, ResumeOffer, refusal_sentence};
pub use routes::set_route;
pub use session::{ProjectRef, SessionKey, SessionRow};
pub use setup::{
    CLIENT_VERSION, CallError, Probe, ServiceCheck, ServiceStatus, SetupAction, SetupState,
    companion_protocols, derive as derive_setup, locate_cli, run_action, service_check,
    service_status, supports_service_start,
};
pub use socket::{SocketError, local_socket_path};
pub use tray_summary::{TrayEntry, TraySummary};
pub use view::{NO_PROJECT_ID, NO_WORK_ID, View, by_project, by_state, by_work_item};
