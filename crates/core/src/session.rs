use crate::attach::AttachRefusal;
use crate::badge::{WorkBadge, WorkFacts, sanitize};
use crate::open::OpenTargets;
use crate::owner_state::OwnerState;
use pohunek_client::LOCAL_HOST;
use pohunek_client::protocol::{RuntimeState, SessionInfo};
use time::OffsetDateTime;

/// Identity of a session across hosts. Only the local daemon is used so far.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionKey {
    pub host: String,
    pub session_id: String,
}

/// The project a session runs in, as the daemon derived it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectRef {
    pub id: String,
    /// Sanitized display label; the id when no label has been seen.
    pub label: String,
}

/// The facts the sidebar list shows for one session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRow {
    pub key: SessionKey,
    /// Owner-set name, else the session id.
    pub display_name: String,
    pub agent: String,
    pub state: String,
    /// What the owner should know at a glance.
    pub owner_state: OwnerState,
    /// When the session started waiting on the owner, if its attention
    /// notification says so.
    pub waiting_since: Option<OffsetDateTime>,
    /// Pohunek Work link and role, when the metadata carries a link id.
    pub badge: Option<WorkBadge>,
    /// Project the session runs in; `None` without a `project_id`.
    pub project: Option<ProjectRef>,
    /// Pohunek Work link fields and role, for placement rules and views.
    pub work: Option<WorkFacts>,
    /// Why the session cannot be attached; `None` when it can.
    pub attach_refusal: Option<AttachRefusal>,
    /// The daemon accepts `session.stop` for this session.
    pub can_stop: bool,
    /// The daemon accepts `session.remove` for this session.
    pub can_remove: bool,
    /// Runtime state and loss reason of an unhealthy runtime (sanitized);
    /// empty for a healthy one.
    pub runtime_detail: String,
    /// Folder and link the row can open.
    pub open: OpenTargets,
}

/// Whether the session's runtime is in the given state.
fn runtime_is(info: &SessionInfo, states: &[RuntimeState]) -> bool {
    info.runtime
        .as_ref()
        .is_some_and(|runtime| states.contains(&runtime.state))
}

/// Stop is offered for a `conflict` runtime although the client rule refuses
/// it: the daemon stops some conflicts and refuses the rest with a reason.
fn offer_stop(info: &SessionInfo) -> bool {
    info.can_stop()
        || (info.external != Some(true)
            && !info.state.is_terminal()
            && runtime_is(info, &[RuntimeState::Conflict]))
}

/// Remove is offered for `conflict` and `incompatible` runtimes although the
/// client rule refuses it: the daemon retires some and refuses the rest.
fn offer_remove(info: &SessionInfo) -> bool {
    info.can_remove()
        || (info.external != Some(true)
            && runtime_is(info, &[RuntimeState::Conflict, RuntimeState::Incompatible]))
}

fn runtime_detail(info: &SessionInfo) -> String {
    let Some(runtime) = info
        .runtime
        .as_ref()
        .filter(|_| runtime_is(info, &[RuntimeState::Conflict, RuntimeState::Incompatible]))
    else {
        return String::new();
    };
    match runtime.loss_reason.as_deref().map(sanitize) {
        Some(reason) if !reason.is_empty() => format!("{}: {reason}", runtime.state.as_str()),
        _ => runtime.state.as_str().to_owned(),
    }
}

impl SessionRow {
    pub(crate) fn from_local(
        info: SessionInfo,
        owner_state: OwnerState,
        waiting_since: Option<OffsetDateTime>,
        known_label: Option<&str>,
    ) -> Self {
        let (can_stop, can_remove) = (offer_stop(&info), offer_remove(&info));
        let runtime_detail = runtime_detail(&info);
        let open = OpenTargets::from_info(&info);
        let display_name = info.name.unwrap_or_else(|| info.id.0.clone());
        let project = info.project_id.as_deref().map(|id| {
            let label = [info.project_label.as_deref(), known_label]
                .into_iter()
                .flatten()
                .map(sanitize)
                .find(|label| !label.is_empty());
            ProjectRef {
                id: sanitize(id),
                label: label.unwrap_or_else(|| sanitize(id)),
            }
        });
        let work = WorkFacts::from_metadata(&info.metadata);
        Self {
            key: SessionKey {
                host: LOCAL_HOST.to_owned(),
                session_id: info.id.0,
            },
            display_name,
            agent: info.agent,
            state: info.state.as_str().to_owned(),
            owner_state,
            waiting_since,
            badge: work.as_ref().and_then(WorkBadge::from_facts),
            project,
            work,
            attach_refusal: AttachRefusal::for_session(info.external, info.state),
            can_stop,
            can_remove,
            runtime_detail,
            open,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn info(patch: Value) -> SessionInfo {
        let mut base = json!({
            "id": "s-1",
            "capabilities": {"resume": false, "fork": false},
            "agent": "claude",
            "agent_base": "claude",
            "cwd": "/tmp",
            "pid": 1,
            "runtime": {
                "state": "live",
                "runtime_generation": "1",
                "worker_instance_id": "w-1",
            },
            "cols": 80,
            "rows": 24,
            "state": "running",
            "state_source": "screen",
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z",
        });
        base.as_object_mut()
            .unwrap()
            .extend(patch.as_object().unwrap().clone());
        serde_json::from_value(base).expect("session fixture")
    }

    fn row(info: SessionInfo, known_label: Option<&str>) -> SessionRow {
        SessionRow::from_local(info, OwnerState::Working, None, known_label)
    }

    #[test]
    fn project_label_comes_from_the_snapshot_then_the_known_label_then_the_id() {
        let labelled = row(
            info(json!({"project_id": "p-1", "project_label": "fresh"})),
            Some("old"),
        );
        assert_eq!(labelled.project.unwrap().label, "fresh");
        let remembered = row(info(json!({"project_id": "p-1"})), Some("pohunek"));
        let project = remembered.project.unwrap();
        assert_eq!(
            (project.id.as_str(), project.label.as_str()),
            ("p-1", "pohunek")
        );
        let unknown = row(info(json!({"project_id": "p-2"})), None);
        assert_eq!(unknown.project.unwrap().label, "p-2");
        assert_eq!(row(info(json!({})), Some("pohunek")).project, None);
    }

    #[test]
    fn work_facts_prefer_work_link_keys_over_legacy_keys() {
        let session = row(
            info(json!({"metadata": {
                "work.link.provider": "github",
                "work.link.kind": "pr",
                "work.link.id": "12",
                "work.role": "implementer",
                "link.id": "ABC-1",
            }})),
            None,
        );
        let work = session.work.unwrap();
        assert_eq!(work.id.as_deref(), Some("12"));
        assert_eq!(work.provider.as_deref(), Some("github"));
        assert_eq!(work.kind.as_deref(), Some("pr"));
        assert_eq!(work.role.as_deref(), Some("implementer"));
        assert_eq!(session.badge.unwrap().link, "GH PR #12");
    }

    #[test]
    fn conflict_offers_stop_and_remove_and_incompatible_only_remove() {
        let runtime = |state: &str| json!({"runtime": {"state": state, "runtime_generation": "1", "loss_reason": "resume_binding_shape_mismatch"}});
        let conflict = row(info(runtime("conflict")), None);
        assert!(conflict.can_stop && conflict.can_remove);
        assert_eq!(
            conflict.runtime_detail,
            "conflict: resume_binding_shape_mismatch"
        );
        let incompatible = row(info(runtime("incompatible")), None);
        assert!(!incompatible.can_stop && incompatible.can_remove);
        let external = row(
            info(
                json!({"external": true, "runtime": {"state": "conflict", "runtime_generation": "1"}}),
            ),
            None,
        );
        assert!(!external.can_stop && !external.can_remove);
        assert!(row(info(json!({})), None).runtime_detail.is_empty());
    }
}
