//! Starting a Pohunek assistant session. The orchestration (agent choice,
//! knowledge bundle, live snapshot, prompt, `session.new`) is the
//! `pohunek-assistant` crate's; this module maps the dialog's form and the
//! companion's connection settings onto it.

use std::path::PathBuf;

use pohunek_assistant::launch::{
    AssistantPaths, LaunchParams, prepare_with_options, start_prepared_with_options,
};
use pohunek_assistant::{AssistantError, ConnectionOptions, HostConfig as AssistantHost};
use pohunek_client::protocol::ProtocolError;
use pohunek_client::{ClientError, LOCAL_HOST, OriginSource, is_local_host};
use tracing::{error, info};

pub use pohunek_assistant::launch::Intent;

use crate::config::{DaemonConfig, NewSessionConfig};
use crate::connect::socket_for_host;
use crate::new_session::{Activity, Choices, CreateError, classify, trimmed_or_none};
use crate::routes::route_for;

/// The intents offered by the dialog, default first.
pub const INTENTS: [Intent; 5] = [
    Intent::Help,
    Intent::Setup,
    Intent::Project,
    Intent::Update,
    Intent::Debug,
];

/// The dialog's fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssistantForm {
    pub intent: Intent,
    /// Project id; a remote host needs one.
    pub project: Option<String>,
    /// Agent name; `None` lets the launch pick the highest-ranked capable one.
    pub agent: Option<String>,
    /// Free-form request; blank is none.
    pub request: String,
}

/// Why the assistant cannot be started yet; the message is shown as is.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AssistantFormError {
    #[error("Reading projects and agents from the daemon...")]
    Loading,
    #[error("A launch is in progress.")]
    InFlight,
    #[error("A remote host needs a project: choose one.")]
    RemoteNeedsProject,
    #[error("The agent `{0}` cannot host an assistant session on this host.")]
    AgentNotCapable(String),
}

/// The first reason the assistant cannot be started with `form` on a host
/// that `remote` says is not this machine; `None` when Start is allowed. A host
/// with no capable agent is not a blocker: the launch reports it with the
/// daemon's hint.
pub fn assistant_blocker(
    form: &AssistantForm,
    choices: &Choices,
    remote: bool,
    activity: Activity,
) -> Option<AssistantFormError> {
    if activity.in_flight {
        return Some(AssistantFormError::InFlight);
    }
    if activity.loading {
        return Some(AssistantFormError::Loading);
    }
    if remote && form.project.is_none() {
        return Some(AssistantFormError::RemoteNeedsProject);
    }
    form.agent
        .as_ref()
        .filter(|agent| !choices.assistant_agents.contains(agent))
        .map(|agent| AssistantFormError::AgentNotCapable(agent.clone()))
}

/// Whether `host` (`local` or a remote route key) is a remote host.
pub fn is_remote_host(host: &str) -> bool {
    !is_local_host(host)
}

fn launch_params(form: &AssistantForm, config: &NewSessionConfig) -> LaunchParams {
    LaunchParams {
        intent: form.intent,
        request: trimmed_or_none(&form.request),
        agent: form.agent.clone(),
        project: form.project.clone(),
        repo: None,
        branch: None,
        base_branch: None,
        cols: config.cols,
        rows: config.rows,
        no_snapshot: false,
        degraded: false,
        auto_started_daemon: false,
    }
}

/// A refusal with the daemon's recovery hint appended.
fn refused(err: ProtocolError) -> CreateError {
    let message = match err.recover {
        Some(hint) => format!("{}\n{hint}", err.msg),
        None => err.msg,
    };
    CreateError::Refused {
        code: err.code,
        message,
    }
}

/// A failure before `session.new` was sent: nothing exists on the host.
fn prepare_failure(err: AssistantError) -> CreateError {
    match err {
        AssistantError::Protocol(err) | AssistantError::Client(ClientError::Protocol(err)) => {
            refused(err)
        }
        other => CreateError::NotSent(other.to_string()),
    }
}

/// A failure of the `session.new` connection or call, classified like a plain
/// create: a daemon refusal, a failure proven to precede the request, or an
/// unknown outcome.
fn start_failure(err: AssistantError) -> CreateError {
    match err {
        AssistantError::Protocol(err) => refused(err),
        AssistantError::Client(ClientError::Protocol(err)) => refused(err),
        AssistantError::Client(err) => classify(err),
        _ => CreateError::Unconfirmed,
    }
}

/// Starts an assistant session on `host` (`local` or a remote route key) and
/// returns the new session's id. The preparation (agent choice, knowledge
/// bundle, snapshot) can fail without a session existing; the single
/// `session.new` is never retried, so a failure after it left is
/// [`CreateError::Unconfirmed`], and a prompt the daemon did not confirm as
/// delivered is [`CreateError::PromptUnconfirmed`].
pub async fn launch_assistant(
    host: &str,
    daemon: &DaemonConfig,
    config: &NewSessionConfig,
    form: &AssistantForm,
) -> Result<String, CreateError> {
    let socket = socket_for_host(host).map_err(|err| CreateError::NotSent(err.to_string()))?;
    let target = if is_local_host(host) {
        AssistantHost::local(LOCAL_HOST, socket)
    } else {
        AssistantHost::remote(host, route_for(host), PathBuf::new())
    };
    let paths = AssistantPaths::resolve().map_err(prepare_failure)?;
    let options = ConnectionOptions {
        connect_timeout: daemon.connect_timeout(),
        request_timeout: Some(config.create_timeout()),
        origin_source: OriginSource::Omitted,
        ..ConnectionOptions::default()
    };
    let prepared = prepare_with_options(&target, &paths, launch_params(form, config), options)
        .await
        .map_err(|err| {
            let failure = prepare_failure(err);
            error!(code = failure.code(), "assistant preparation failed");
            failure
        })?;
    info!(
        agent = %prepared.selection.name,
        intent = %prepared.intent,
        knowledge = prepared.knowledge.label,
        "assistant prepared"
    );
    match start_prepared_with_options(&target, prepared, options).await {
        Ok(launched) if launched.applied_input != Some(true) => Err(CreateError::PromptUnconfirmed),
        Ok(launched) => Ok(launched.session.id.0),
        Err(err) => {
            let failure = start_failure(err);
            error!(code = failure.code(), "assistant session.new failed");
            Err(failure)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn form() -> AssistantForm {
        AssistantForm {
            intent: Intent::Help,
            project: None,
            agent: None,
            request: String::new(),
        }
    }

    fn choices() -> Choices {
        Choices {
            assistant_agents: vec!["claude".to_owned()],
            ..Choices::default()
        }
    }

    fn idle() -> Activity {
        Activity {
            loading: false,
            in_flight: false,
        }
    }

    #[test]
    fn local_start_needs_no_project_and_automatic_agent() {
        assert_eq!(assistant_blocker(&form(), &choices(), false, idle()), None);
        assert_eq!(
            assistant_blocker(&form(), &Choices::default(), false, idle()),
            None
        );
    }

    #[test]
    fn remote_start_needs_a_project() {
        assert_eq!(
            assistant_blocker(&form(), &choices(), true, idle()),
            Some(AssistantFormError::RemoteNeedsProject)
        );
        let with_project = AssistantForm {
            project: Some("p-1".to_owned()),
            ..form()
        };
        assert_eq!(
            assistant_blocker(&with_project, &choices(), true, idle()),
            None
        );
    }

    #[test]
    fn explicit_agent_must_be_capable_on_the_host() {
        let codex = AssistantForm {
            agent: Some("codex".to_owned()),
            ..form()
        };
        assert_eq!(
            assistant_blocker(&codex, &choices(), false, idle()),
            Some(AssistantFormError::AgentNotCapable("codex".to_owned()))
        );
    }

    #[test]
    fn refusal_carries_the_daemon_hint() {
        let CreateError::Refused { code, message } =
            prepare_failure(AssistantError::Protocol(ProtocolError::no_capable_agent()))
        else {
            panic!("expected a refusal");
        };
        assert_eq!(code, "no_capable_agent");
        assert!(message.starts_with("no capable assistant agent runtime is available\n"));
        assert!(message.contains("pass --agent"));
    }
}
