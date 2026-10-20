use crate::attach::AttachRefusal;
use crate::badge::{WorkBadge, WorkFacts, sanitize};
use crate::owner_state::OwnerState;
use pohunek_client::LOCAL_HOST;
use pohunek_client::protocol::SessionInfo;
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
}

impl SessionRow {
    pub(crate) fn from_local(
        info: SessionInfo,
        owner_state: OwnerState,
        waiting_since: Option<OffsetDateTime>,
        known_label: Option<&str>,
    ) -> Self {
        let (can_stop, can_remove) = (info.can_stop(), info.can_remove());
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
}
