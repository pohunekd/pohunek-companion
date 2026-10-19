//! Creating a session: the choices the daemon offers, validation of the form,
//! and the single `session.new` call. The only place the companion mutates
//! daemon state.

use std::collections::BTreeMap;

use pohunek_client::protocol::method::{HostInspect, ProjectList};
use pohunek_client::protocol::{
    AgentRuntime, HostCapabilities, ProjectInfo, ProjectListParams, SessionNewParams,
};
use pohunek_client::{Client, ClientError};
use tracing::{error, info};

use crate::config::{DaemonConfig, NewSessionConfig};
use crate::connect::{connect, connect_for_create, resolve_socket};
use crate::error::CoreError;

/// Shown when a create may or may not have happened. `session.new` has no
/// idempotency key, so the request is never resent.
pub const UNCONFIRMED_MESSAGE: &str =
    "The daemon did not confirm the create; check the sidebar before trying again.";

/// A project the owner can pick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectChoice {
    pub id: String,
    pub label: String,
}

/// What the dialog offers, as the daemon reported it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Choices {
    pub projects: Vec<ProjectChoice>,
    /// Agent names that can be launched now, in the daemon's order.
    pub agents: Vec<String>,
}

/// The form as the owner filled it in.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NewSessionForm {
    pub project: Option<String>,
    pub agent: Option<String>,
    pub name: String,
    pub prompt: String,
}

/// Why a form cannot be submitted; the message is shown as is.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FormError {
    #[error("Choose a project.")]
    NoProject,
    #[error("Choose an agent.")]
    NoAgent,
    #[error("The agent `{0}` is not launchable on this host.")]
    AgentNotLaunchable(String),
}

/// Why a create did not produce a session.
#[derive(Debug, thiserror::Error)]
pub enum CreateError {
    /// The daemon answered with a typed error; its message is shown.
    #[error("{message}")]
    Refused { code: String, message: String },
    /// The request never left: the socket or the connection failed first.
    #[error("{0}")]
    NotSent(String),
    /// The outcome is unknown; the request must not be repeated.
    #[error("{UNCONFIRMED_MESSAGE}")]
    Unconfirmed,
}

impl CreateError {
    /// Stable code for logs: the daemon's error code, or a companion code.
    pub fn code(&self) -> &str {
        match self {
            Self::Refused { code, .. } => code,
            Self::NotSent(_) => "not_sent",
            Self::Unconfirmed => "unconfirmed",
        }
    }
}

/// Whether a runtime can be launched by the public rule: present on the host
/// and not reported as an unsupported version.
fn launchable(runtime: &AgentRuntime) -> bool {
    runtime.available && runtime.supported != Some(false)
}

/// Launchable agent names, in the daemon's order.
pub fn launchable_agents(capabilities: &HostCapabilities) -> Vec<String> {
    capabilities
        .runtimes
        .iter()
        .filter(|runtime| launchable(runtime))
        .map(|runtime| runtime.agent.clone())
        .collect()
}

fn trimmed_or_none(text: &str) -> Option<String> {
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

/// Validates `form` against the offered `agents` and builds the request.
pub fn build_params(
    form: &NewSessionForm,
    agents: &[String],
    config: &NewSessionConfig,
) -> Result<SessionNewParams, FormError> {
    let project = form
        .project
        .as_deref()
        .filter(|id| !id.is_empty())
        .ok_or(FormError::NoProject)?;
    let agent = form
        .agent
        .as_deref()
        .filter(|agent| !agent.is_empty())
        .ok_or(FormError::NoAgent)?;
    if !agents.iter().any(|offered| offered == agent) {
        return Err(FormError::AgentNotLaunchable(agent.to_owned()));
    }
    Ok(SessionNewParams {
        agent: agent.to_owned(),
        name: trimmed_or_none(&form.name),
        cwd: None,
        cols: config.cols,
        rows: config.rows,
        project: Some(project.to_owned()),
        repo: None,
        branch: None,
        base_branch: None,
        input: trimmed_or_none(&form.prompt),
        extended_input_ready_wait: None,
        metadata: BTreeMap::new(),
    })
}

fn project_choices(projects: Vec<ProjectInfo>) -> Vec<ProjectChoice> {
    projects
        .into_iter()
        .map(|project| ProjectChoice {
            id: project.id,
            label: project.label,
        })
        .collect()
}

async fn read_choices(client: &mut Client) -> Result<Choices, CoreError> {
    let projects = client
        .call::<ProjectList>(ProjectListParams::default())
        .await?;
    let capabilities = client.call::<HostInspect>(()).await?;
    Ok(Choices {
        projects: project_choices(projects),
        agents: launchable_agents(&capabilities),
    })
}

/// Reads the projects and launchable agents from the local daemon.
pub async fn load_choices(daemon: &DaemonConfig) -> Result<Choices, CoreError> {
    let socket = resolve_socket()?;
    let mut client = connect(&socket, daemon).await?;
    let choices = read_choices(&mut client).await?;
    info!(
        projects = choices.projects.len(),
        agents = choices.agents.len(),
        "new-session choices loaded"
    );
    Ok(choices)
}

/// Sends exactly one `session.new` and returns the new session's id. Never
/// retries: any failure after the request may have left the daemon is
/// reported as [`CreateError::Unconfirmed`].
pub async fn create_session(
    daemon: &DaemonConfig,
    config: &NewSessionConfig,
    params: SessionNewParams,
) -> Result<String, CreateError> {
    let socket = resolve_socket().map_err(|err| CreateError::NotSent(err.to_string()))?;
    let mut client = connect_for_create(&socket, daemon, config)
        .await
        .map_err(|err| CreateError::NotSent(err.to_string()))?;
    match client.session_new(params).await {
        Ok(result) => Ok(result.session.id.0),
        Err(err) => {
            let classified = classify(err);
            error!(code = classified.code(), "session.new failed");
            Err(classified)
        }
    }
}

/// Splits a failed `session.new` into a daemon refusal, a failure proven to
/// precede the request, and an unknown outcome.
fn classify(err: ClientError) -> CreateError {
    match err {
        ClientError::Protocol(err) => CreateError::Refused {
            code: err.code,
            message: err.msg,
        },
        err @ ClientError::DaemonUnreachable { .. } => CreateError::NotSent(err.to_string()),
        _ => CreateError::Unconfirmed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pohunek_client::protocol::{ErrorClass, ProtocolError};

    fn runtime(agent: &str, available: bool, supported: Option<bool>) -> AgentRuntime {
        AgentRuntime {
            agent: agent.to_owned(),
            agent_base: None,
            available,
            path: None,
            version: None,
            supported,
            config_home_id: None,
        }
    }

    fn agents() -> Vec<String> {
        vec!["shell".to_owned(), "claude".to_owned()]
    }

    fn form() -> NewSessionForm {
        NewSessionForm {
            project: Some("p-1".to_owned()),
            agent: Some("shell".to_owned()),
            name: "  epsilon-new \n".to_owned(),
            prompt: "\n echo hi  ".to_owned(),
        }
    }

    #[test]
    fn launchable_rule_covers_available_and_supported() {
        for (available, supported, expected) in [
            (true, None, true),
            (true, Some(true), true),
            (true, Some(false), false),
            (false, None, false),
            (false, Some(true), false),
        ] {
            assert_eq!(
                launchable(&runtime("a", available, supported)),
                expected,
                "available={available} supported={supported:?}"
            );
        }
    }

    #[test]
    fn params_trim_and_carry_config_size() {
        let config = NewSessionConfig {
            cols: 132,
            rows: 50,
            ..NewSessionConfig::default()
        };
        let params = build_params(&form(), &agents(), &config).unwrap();
        assert_eq!(params.name.as_deref(), Some("epsilon-new"));
        assert_eq!(params.input.as_deref(), Some("echo hi"));
        assert_eq!(params.project.as_deref(), Some("p-1"));
        assert_eq!(params.agent, "shell");
        assert_eq!((params.cols, params.rows), (132, 50));
        assert!(params.cwd.is_none() && params.repo.is_none() && params.branch.is_none());
        assert!(params.metadata.is_empty());
    }

    #[test]
    fn blank_name_and_prompt_become_none() {
        let form = NewSessionForm {
            name: " \t".to_owned(),
            prompt: "\n\n".to_owned(),
            ..form()
        };
        let params = build_params(&form, &agents(), &NewSessionConfig::default()).unwrap();
        assert_eq!((params.name, params.input), (None, None));
        assert_eq!((params.cols, params.rows), (80, 24));
    }

    #[test]
    fn project_and_launchable_agent_are_required() {
        let config = NewSessionConfig::default();
        let no_project = NewSessionForm {
            project: None,
            ..form()
        };
        assert_eq!(
            build_params(&no_project, &agents(), &config).unwrap_err(),
            FormError::NoProject
        );
        let no_agent = NewSessionForm {
            agent: None,
            ..form()
        };
        assert_eq!(
            build_params(&no_agent, &agents(), &config).unwrap_err(),
            FormError::NoAgent
        );
        let unknown = NewSessionForm {
            agent: Some("ghost".to_owned()),
            ..form()
        };
        assert_eq!(
            build_params(&unknown, &agents(), &config).unwrap_err(),
            FormError::AgentNotLaunchable("ghost".to_owned())
        );
    }

    #[test]
    fn daemon_refusal_keeps_its_message_and_timeout_is_unconfirmed() {
        let refused = classify(ClientError::Protocol(ProtocolError::new(
            ErrorClass::Runtime,
            "agent_unavailable",
            "agent is not installed",
            None,
        )));
        assert_eq!(refused.to_string(), "agent is not installed");
        assert_eq!(refused.code(), "agent_unavailable");

        let timeout = classify(ClientError::RequestTimeout {
            host: None,
            timeout: std::time::Duration::from_secs(1),
        });
        assert_eq!(timeout.to_string(), UNCONFIRMED_MESSAGE);
        let lost = classify(ClientError::Framing("eof".to_owned()));
        assert!(matches!(lost, CreateError::Unconfirmed));
    }
}
