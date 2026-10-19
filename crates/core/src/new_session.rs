//! Creating a session: the choices the daemon offers, validation of the form,
//! and the single `session.new` call. The only place the companion mutates
//! daemon state.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

use pohunek_client::protocol::method::{HostInspect, ProjectAction, ProjectActions, ProjectList};
use pohunek_client::protocol::{
    ActionSummary, AgentRuntime, ProjectActionParams, ProjectActionResult, ProjectActionsParams,
    ProjectInfo, ProjectListParams, ProviderKind, SessionNewParams,
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

/// An agent the host reports, with the reason it cannot be launched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentChoice {
    pub name: String,
    /// `None` when the agent can be launched now.
    pub unlaunchable: Option<String>,
}

impl AgentChoice {
    pub fn is_launchable(&self) -> bool {
        self.unlaunchable.is_none()
    }
}

/// What the dialog offers, as the daemon reported it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Choices {
    pub projects: Vec<ProjectChoice>,
    /// Every agent the host reports: launchable ones first in the daemon's
    /// order, then the unlaunchable ones.
    pub agents: Vec<AgentChoice>,
    /// `git` is present on the daemon's host.
    pub git_available: bool,
    /// The daemon's host supports a worktree per session.
    pub worktree_supported: bool,
}

impl Choices {
    pub fn has_launchable_agent(&self) -> bool {
        self.agents.iter().any(AgentChoice::is_launchable)
    }

    /// Index of the launchable agent called `name`.
    pub fn launchable_position(&self, name: &str) -> Option<usize> {
        self.agents
            .iter()
            .position(|agent| agent.name == name && agent.is_launchable())
    }
}

/// The launch recipe of a chosen project template. Its prompt is static text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recipe {
    pub agent: String,
    pub branch: Option<String>,
    pub base_branch: Option<String>,
    pub prompt: String,
}

/// The form as the owner filled it in.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NewSessionForm {
    pub project: Option<String>,
    pub agent: Option<String>,
    pub name: String,
    pub prompt: String,
    /// The chosen template's recipe; `None` means Blank.
    pub recipe: Option<Recipe>,
    /// A template was chosen and its recipe has not arrived yet.
    pub template_loading: bool,
    /// Branch override, used only for Blank.
    pub branch: String,
    /// Base branch override, used only for Blank.
    pub base_branch: String,
}

/// What the dialog is doing besides showing the form.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Activity {
    /// The choices are still being read from the daemon.
    pub loading: bool,
    /// A create request is in flight, or its outcome is unknown.
    pub in_flight: bool,
}

/// Why a form cannot be submitted; the message is shown as is.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FormError {
    #[error("Reading projects and agents from the daemon...")]
    Loading,
    #[error("A create request is in progress.")]
    InFlight,
    #[error("Project not found on this daemon: `{0}`.")]
    RequestedProjectMissing(String),
    #[error("The daemon has no projects.")]
    NoProjectsOnDaemon,
    #[error("Choose a project.")]
    NoProject,
    #[error("The daemon reports no launchable agent.")]
    NoLaunchableAgent,
    #[error("Choose an agent.")]
    NoAgent,
    #[error("The agent `{0}` is not launchable on this host.")]
    AgentNotLaunchable(String),
    #[error("The template is still loading.")]
    TemplateLoading,
    #[error("A branch or base branch is set, but `git` is not available on the daemon's host.")]
    GitUnavailable,
    #[error("A branch or base branch is set, but the daemon's host does not support worktrees.")]
    WorktreeUnsupported,
}

/// Why a template could not be offered or applied.
#[derive(Debug, thiserror::Error)]
pub enum TemplateError {
    #[error("The template `{0}` uses variables (`${{...}}`); only static templates are supported.")]
    Variables(String),
    #[error(transparent)]
    Daemon(#[from] CoreError),
}

/// Counts selection changes so a response that arrives after the project or
/// template changed can be recognised and dropped.
#[derive(Debug, Default)]
pub struct Generation(AtomicU64);

impl Generation {
    /// Invalidates every earlier token and returns the new one.
    pub fn advance(&self) -> u64 {
        self.0.fetch_add(1, Ordering::SeqCst) + 1
    }

    pub fn is_current(&self, token: u64) -> bool {
        self.0.load(Ordering::SeqCst) == token
    }
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
    /// The session exists but the daemon did not confirm delivering the
    /// initial prompt; creating again would duplicate the session.
    #[error(
        "The session was created, but the daemon did not confirm delivering the prompt; check the sidebar."
    )]
    PromptUnconfirmed,
}

impl CreateError {
    /// Stable code for logs: the daemon's error code, or a companion code.
    pub fn code(&self) -> &str {
        match self {
            Self::Refused { code, .. } => code,
            Self::NotSent(_) => "not_sent",
            Self::Unconfirmed => "unconfirmed",
            Self::PromptUnconfirmed => "prompt_unconfirmed",
        }
    }
}

/// Why `runtime` cannot be launched, or `None` when it can: it must be present
/// on the host and not reported as an unsupported version.
fn unlaunchable_reason(runtime: &AgentRuntime) -> Option<String> {
    if !runtime.available {
        return Some("not available: binary not found on the daemon's PATH".to_owned());
    }
    if runtime.supported == Some(false) {
        return Some(match runtime.version.as_deref().filter(|v| !v.is_empty()) {
            Some(version) => format!("unsupported version ({version})"),
            None => "unsupported version".to_owned(),
        });
    }
    None
}

/// Every reported agent, launchable ones first; each group keeps the
/// daemon's order.
fn agent_choices(runtimes: &[AgentRuntime]) -> Vec<AgentChoice> {
    let (launchable, blocked): (Vec<_>, Vec<_>) = runtimes
        .iter()
        .map(|runtime| AgentChoice {
            name: runtime.agent.clone(),
            unlaunchable: unlaunchable_reason(runtime),
        })
        .partition(AgentChoice::is_launchable);
    launchable.into_iter().chain(blocked).collect()
}

fn trimmed_or_none(text: &str) -> Option<String> {
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

/// The branch and base branch the request will carry: the template's, or the
/// Blank overrides.
fn branch_overrides(form: &NewSessionForm) -> (Option<String>, Option<String>) {
    match &form.recipe {
        Some(recipe) => (recipe.branch.clone(), recipe.base_branch.clone()),
        None => (
            trimmed_or_none(&form.branch),
            trimmed_or_none(&form.base_branch),
        ),
    }
}

/// The first reason `form` cannot be submitted, in the order the owner should
/// fix them; `None` when Create is allowed. `requested_project` is the id
/// given on the command line.
pub fn submit_blocker(
    form: &NewSessionForm,
    choices: &Choices,
    requested_project: Option<&str>,
    activity: Activity,
) -> Option<FormError> {
    if activity.loading {
        return Some(FormError::Loading);
    }
    if activity.in_flight {
        return Some(FormError::InFlight);
    }
    // Picking a project by hand supersedes a missing command-line one.
    if let Some(id) = requested_project
        && form.project.as_deref().is_none_or(str::is_empty)
        && !choices.projects.iter().any(|project| project.id == id)
    {
        return Some(FormError::RequestedProjectMissing(id.to_owned()));
    }
    if choices.projects.is_empty() {
        return Some(FormError::NoProjectsOnDaemon);
    }
    if form.project.as_deref().is_none_or(str::is_empty) {
        return Some(FormError::NoProject);
    }
    if !choices.has_launchable_agent() {
        return Some(FormError::NoLaunchableAgent);
    }
    let Some(agent) = form.agent.as_deref().filter(|agent| !agent.is_empty()) else {
        return Some(FormError::NoAgent);
    };
    if choices.launchable_position(agent).is_none() {
        return Some(FormError::AgentNotLaunchable(agent.to_owned()));
    }
    if form.template_loading {
        return Some(FormError::TemplateLoading);
    }
    let (branch, base_branch) = branch_overrides(form);
    if branch.is_some() || base_branch.is_some() {
        if !choices.git_available {
            return Some(FormError::GitUnavailable);
        }
        if !choices.worktree_supported {
            return Some(FormError::WorktreeUnsupported);
        }
    }
    None
}

/// Validates `form` against the offered `choices` and builds the request.
pub fn build_params(
    form: &NewSessionForm,
    choices: &Choices,
    requested_project: Option<&str>,
    activity: Activity,
    config: &NewSessionConfig,
) -> Result<SessionNewParams, FormError> {
    if let Some(blocker) = submit_blocker(form, choices, requested_project, activity) {
        return Err(blocker);
    }
    // `submit_blocker` returned `None`, so project and agent are set.
    let (Some(project), Some(agent)) = (form.project.as_deref(), form.agent.as_deref()) else {
        return Err(FormError::NoProject);
    };
    let (branch, base_branch) = branch_overrides(form);
    Ok(SessionNewParams {
        agent: agent.to_owned(),
        name: trimmed_or_none(&form.name),
        cwd: None,
        cols: config.cols,
        rows: config.rows,
        project: Some(project.to_owned()),
        repo: None,
        branch,
        base_branch,
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
        agents: agent_choices(&capabilities.runtimes),
        git_available: capabilities.git_available,
        worktree_supported: capabilities.worktree_supported,
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
        launchable = choices.agents.iter().filter(|a| a.is_launchable()).count(),
        "new-session choices loaded"
    );
    Ok(choices)
}

/// Names of the offered templates: the provider-less project actions.
pub fn template_names(actions: Vec<ActionSummary>) -> Vec<String> {
    actions
        .into_iter()
        .filter(|action| action.provider == ProviderKind::None)
        .map(|action| action.name)
        .collect()
}

/// Turns a resolved action into a recipe, refusing a prompt with `${...}`
/// variables because rendering is static.
pub fn recipe_from_action(
    name: &str,
    result: ProjectActionResult,
) -> Result<Recipe, TemplateError> {
    if result.prompt_content.contains("${") {
        return Err(TemplateError::Variables(name.to_owned()));
    }
    Ok(Recipe {
        agent: result.agent,
        branch: result.branch.as_deref().and_then(trimmed_or_none),
        base_branch: result.base_branch.as_deref().and_then(trimmed_or_none),
        prompt: result.prompt_content,
    })
}

/// Reads the templates of `project` (a project id) from the local daemon.
pub async fn load_templates(
    daemon: &DaemonConfig,
    project: &str,
) -> Result<Vec<String>, CoreError> {
    let socket = resolve_socket()?;
    let mut client = connect(&socket, daemon).await?;
    let result = client
        .call::<ProjectActions>(ProjectActionsParams {
            reference: project.to_owned(),
        })
        .await?;
    Ok(template_names(result.actions))
}

/// Resolves the template `name` of `project` to its recipe.
pub async fn load_recipe(
    daemon: &DaemonConfig,
    project: &str,
    name: &str,
) -> Result<Recipe, TemplateError> {
    let socket = resolve_socket()?;
    let mut client = connect(&socket, daemon).await?;
    let result = client
        .call::<ProjectAction>(ProjectActionParams {
            reference: project.to_owned(),
            name: name.to_owned(),
        })
        .await
        .map_err(CoreError::from)?;
    recipe_from_action(name, result)
}

/// Sends exactly one `session.new` and returns the new session's id. Never
/// retries: a failure that may have happened after the request left is
/// reported as [`CreateError::Unconfirmed`]. A prompt the daemon did not
/// confirm as delivered is [`CreateError::PromptUnconfirmed`].
pub async fn create_session(
    daemon: &DaemonConfig,
    config: &NewSessionConfig,
    params: SessionNewParams,
) -> Result<String, CreateError> {
    let socket = resolve_socket().map_err(|err| CreateError::NotSent(err.to_string()))?;
    let mut client = connect_for_create(&socket, daemon, config)
        .await
        .map_err(|err| CreateError::NotSent(err.to_string()))?;
    let sent_input = params.input.is_some();
    match client.session_new(params).await {
        Ok(result) if sent_input && result.applied_input != Some(true) => {
            Err(CreateError::PromptUnconfirmed)
        }
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
        err @ (ClientError::DaemonUnreachable { .. }
        | ClientError::ClientFileDescriptorsExhausted { .. }
        | ClientError::SystemFileDescriptorsExhausted { .. }
        | ClientError::Envelope(_)
        | ClientError::IncompleteOriginEnvironment
        | ClientError::InvalidOriginEnvironment) => CreateError::NotSent(err.to_string()),
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

    fn choice(name: &str, unlaunchable: Option<&str>) -> AgentChoice {
        AgentChoice {
            name: name.to_owned(),
            unlaunchable: unlaunchable.map(str::to_owned),
        }
    }

    fn choices() -> Choices {
        Choices {
            projects: vec![ProjectChoice {
                id: "p-1".to_owned(),
                label: "proj".to_owned(),
            }],
            agents: vec![
                choice("shell", None),
                choice("claude", None),
                choice("codex", Some("x")),
            ],
            git_available: true,
            worktree_supported: true,
        }
    }

    fn blocker(
        form: &NewSessionForm,
        choices: &Choices,
        requested: Option<&str>,
        activity: Activity,
    ) -> Option<FormError> {
        submit_blocker(form, choices, requested, activity)
    }

    fn form() -> NewSessionForm {
        NewSessionForm {
            project: Some("p-1".to_owned()),
            agent: Some("shell".to_owned()),
            name: "  epsilon-new \n".to_owned(),
            prompt: "\n echo hi  ".to_owned(),
            ..NewSessionForm::default()
        }
    }

    fn action(provider: ProviderKind, prompt: &str) -> ProjectActionResult {
        ProjectActionResult {
            provider,
            agent: "shell".to_owned(),
            base_branch: Some("main".to_owned()),
            branch: Some("feat/x".to_owned()),
            prompt_name: "t".to_owned(),
            prompt_content: prompt.to_owned(),
        }
    }

    #[test]
    fn agent_reasons_name_the_cause_and_launchable_agents_come_first() {
        let mut old = runtime("old", true, Some(false));
        old.version = Some("0.1.2".to_owned());
        let runtimes = vec![
            runtime("missing", false, None),
            old,
            runtime("odd", true, Some(false)),
            runtime("ok", true, Some(true)),
        ];
        let agents = agent_choices(&runtimes);
        let names: Vec<_> = agents.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["ok", "missing", "old", "odd"]);
        let reasons: Vec<_> = agents.iter().map(|a| a.unlaunchable.as_deref()).collect();
        assert_eq!(
            reasons,
            [
                None,
                Some("not available: binary not found on the daemon's PATH"),
                Some("unsupported version (0.1.2)"),
                Some("unsupported version"),
            ]
        );
    }

    #[test]
    fn params_trim_and_carry_config_size() {
        let config = NewSessionConfig {
            cols: 132,
            rows: 50,
            ..NewSessionConfig::default()
        };
        let params = build_params(&form(), &choices(), None, Activity::default(), &config).unwrap();
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
        let params = build_params(
            &form,
            &choices(),
            None,
            Activity::default(),
            &NewSessionConfig::default(),
        )
        .unwrap();
        assert_eq!((params.name, params.input), (None, None));
        assert_eq!((params.cols, params.rows), (80, 24));
    }

    #[test]
    fn each_blocker_is_reported() {
        let idle = Activity::default();
        let with = |f: NewSessionForm| blocker(&f, &choices(), None, idle);
        assert_eq!(with(form()), None);
        assert_eq!(
            with(NewSessionForm {
                project: None,
                ..form()
            }),
            Some(FormError::NoProject)
        );
        assert_eq!(
            with(NewSessionForm {
                agent: None,
                ..form()
            }),
            Some(FormError::NoAgent)
        );
        for ghost in ["ghost", "codex"] {
            assert_eq!(
                with(NewSessionForm {
                    agent: Some(ghost.to_owned()),
                    ..form()
                }),
                Some(FormError::AgentNotLaunchable(ghost.to_owned()))
            );
        }
        let no_project = NewSessionForm {
            project: None,
            ..form()
        };
        assert_eq!(
            blocker(&no_project, &choices(), Some("gone"), idle),
            Some(FormError::RequestedProjectMissing("gone".to_owned()))
        );
        // A project picked by hand supersedes a missing command-line one.
        assert_eq!(blocker(&form(), &choices(), Some("gone"), idle), None);
        let busy = Activity {
            in_flight: true,
            ..idle
        };
        assert_eq!(
            blocker(&form(), &choices(), None, busy),
            Some(FormError::InFlight)
        );
        let loading = Activity {
            loading: true,
            ..idle
        };
        assert_eq!(
            blocker(&form(), &choices(), None, loading),
            Some(FormError::Loading)
        );
        let none = Choices {
            projects: vec![],
            ..choices()
        };
        assert_eq!(
            blocker(&form(), &none, None, idle),
            Some(FormError::NoProjectsOnDaemon)
        );
        let none = Choices {
            agents: vec![choice("codex", Some("x"))],
            ..choices()
        };
        assert_eq!(
            blocker(&form(), &none, None, idle),
            Some(FormError::NoLaunchableAgent)
        );
    }

    #[test]
    fn git_reasons_apply_only_when_a_branch_is_set() {
        let idle = Activity::default();
        let no_git = Choices {
            git_available: false,
            ..choices()
        };
        let no_worktree = Choices {
            worktree_supported: false,
            ..choices()
        };
        let branched = NewSessionForm {
            branch: " topic ".to_owned(),
            ..form()
        };
        let based = NewSessionForm {
            base_branch: "main".to_owned(),
            ..form()
        };
        assert_eq!(blocker(&form(), &no_git, None, idle), None);
        assert_eq!(blocker(&form(), &no_worktree, None, idle), None);
        for f in [&branched, &based] {
            assert_eq!(
                blocker(f, &no_git, None, idle),
                Some(FormError::GitUnavailable)
            );
            assert_eq!(
                blocker(f, &no_worktree, None, idle),
                Some(FormError::WorktreeUnsupported)
            );
            assert_eq!(blocker(f, &choices(), None, idle), None);
        }
        let both = Choices {
            git_available: false,
            worktree_supported: false,
            ..choices()
        };
        assert_eq!(
            blocker(&branched, &both, None, idle),
            Some(FormError::GitUnavailable)
        );
        let blank = NewSessionForm {
            branch: "  ".to_owned(),
            ..form()
        };
        assert_eq!(blocker(&blank, &no_git, None, idle), None);
        let recipe = recipe_from_action("t", action(ProviderKind::None, "x")).unwrap();
        let templated = NewSessionForm {
            recipe: Some(recipe),
            ..form()
        };
        assert_eq!(
            blocker(&templated, &no_git, None, idle),
            Some(FormError::GitUnavailable)
        );
    }

    #[test]
    fn blockers_have_a_fixed_precedence() {
        let all_wrong = NewSessionForm {
            project: None,
            agent: None,
            ..form()
        };
        let empty = Choices::default();
        let activity = |loading, in_flight| Activity { loading, in_flight };
        let check = |activity| blocker(&all_wrong, &empty, Some("gone"), activity);
        assert_eq!(check(activity(true, true)), Some(FormError::Loading));
        assert_eq!(check(activity(false, true)), Some(FormError::InFlight));
        assert_eq!(
            check(activity(false, false)),
            Some(FormError::RequestedProjectMissing("gone".to_owned()))
        );
        let idle = Activity::default();
        assert_eq!(
            blocker(&all_wrong, &empty, None, idle),
            Some(FormError::NoProjectsOnDaemon)
        );
        let projects_only = Choices {
            projects: choices().projects,
            ..Choices::default()
        };
        assert_eq!(
            blocker(&all_wrong, &projects_only, None, idle),
            Some(FormError::NoProject)
        );
        let picked = NewSessionForm {
            project: Some("p-1".to_owned()),
            ..all_wrong.clone()
        };
        assert_eq!(
            blocker(&picked, &projects_only, None, idle),
            Some(FormError::NoLaunchableAgent)
        );
        assert_eq!(
            blocker(&picked, &choices(), None, idle),
            Some(FormError::NoAgent)
        );
        let loading_branched = NewSessionForm {
            template_loading: true,
            branch: "b".to_owned(),
            ..form()
        };
        let no_git = Choices {
            git_available: false,
            ..choices()
        };
        assert_eq!(
            blocker(&loading_branched, &no_git, None, idle),
            Some(FormError::TemplateLoading)
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

    #[test]
    fn recipe_branches_reach_params_and_blank_overrides_are_trimmed() {
        let config = NewSessionConfig::default();
        let recipe = recipe_from_action("t", action(ProviderKind::None, "do it")).unwrap();
        let from_recipe = NewSessionForm {
            recipe: Some(recipe),
            branch: "ignored".to_owned(),
            base_branch: "ignored".to_owned(),
            ..form()
        };
        let params =
            build_params(&from_recipe, &choices(), None, Activity::default(), &config).unwrap();
        assert_eq!(params.branch.as_deref(), Some("feat/x"));
        assert_eq!(params.base_branch.as_deref(), Some("main"));

        let overridden = NewSessionForm {
            branch: "  topic \n".to_owned(),
            base_branch: "release".to_owned(),
            ..form()
        };
        let params =
            build_params(&overridden, &choices(), None, Activity::default(), &config).unwrap();
        assert_eq!(params.branch.as_deref(), Some("topic"));
        assert_eq!(params.base_branch.as_deref(), Some("release"));

        let empty = NewSessionForm {
            branch: " ".to_owned(),
            ..form()
        };
        let params = build_params(&empty, &choices(), None, Activity::default(), &config).unwrap();
        assert_eq!((params.branch, params.base_branch), (None, None));
    }

    #[test]
    fn loading_template_blocks_create() {
        let loading = NewSessionForm {
            template_loading: true,
            ..form()
        };
        assert_eq!(
            build_params(
                &loading,
                &choices(),
                None,
                Activity::default(),
                &NewSessionConfig::default()
            )
            .unwrap_err(),
            FormError::TemplateLoading
        );
    }

    #[test]
    fn variable_prompt_is_refused_and_static_one_kept() {
        let refused = recipe_from_action("fix", action(ProviderKind::None, "work on ${issue}"));
        assert!(matches!(refused, Err(TemplateError::Variables(name)) if name == "fix"));
        let kept = recipe_from_action("fix", action(ProviderKind::None, "plain $5 text")).unwrap();
        assert_eq!(kept.prompt, "plain $5 text");
        let mut blank_branch = action(ProviderKind::None, "x");
        blank_branch.branch = Some("  ".to_owned());
        let recipe = recipe_from_action("fix", blank_branch).unwrap();
        assert_eq!(recipe.branch, None);
    }

    #[test]
    fn only_provider_less_actions_are_templates() {
        let summary = |name: &str, provider| ActionSummary {
            name: name.to_owned(),
            provider,
            template: "t".to_owned(),
            layer: pohunek_client::protocol::PromptLayer::InRepo,
        };
        let names = template_names(vec![
            summary("a", ProviderKind::None),
            summary("b", ProviderKind::GithubPr),
            summary("c", ProviderKind::None),
        ]);
        assert_eq!(names, ["a", "c"]);
    }

    #[test]
    fn stale_generation_token_is_not_current() {
        let generation = Generation::default();
        let first = generation.advance();
        assert!(generation.is_current(first));
        let second = generation.advance();
        assert!(!generation.is_current(first) && generation.is_current(second));
    }
}
