//! Whether an ended session can be resumed and, when it cannot, why. Both come
//! from the session snapshot: the frozen `resume` capability and, when the
//! daemon cleared it, the refusal code in `capability_refusals`.

use pohunek_client::protocol::{RuntimeState, SessionInfo};

use crate::badge::sanitize;

/// Refusal code assumed when the capability is false and the daemon names none.
const NOT_RESUMABLE: &str = "not_resumable";

/// Error code `session.resume` answers with when the profile changed since
/// launch and the caller did not accept the change.
pub const PROFILE_CHANGED_CODE: &str = "agent_profile_changed";

/// What the snapshot says about resuming one session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResumeOffer {
    /// The session is not ended (or is external): no Resume, nothing to explain.
    NotApplicable,
    /// An ended session the daemon can resume.
    Available,
    /// An ended session that cannot be resumed; the sentence says why.
    Refused(String),
}

impl ResumeOffer {
    pub fn is_available(&self) -> bool {
        matches!(self, Self::Available)
    }

    /// The sentence for the detail view; empty unless the session is refused.
    pub fn note(&self) -> &str {
        match self {
            Self::Refused(note) => note,
            _ => "",
        }
    }
}

/// The session stopped, finished, failed, or lost its runtime.
fn is_ended(info: &SessionInfo) -> bool {
    info.state.is_terminal()
        || info
            .runtime
            .as_ref()
            .is_some_and(|runtime| runtime.state == RuntimeState::Lost)
}

pub(crate) fn resume_offer(info: &SessionInfo) -> ResumeOffer {
    if info.external == Some(true) || !is_ended(info) {
        return ResumeOffer::NotApplicable;
    }
    if info.capabilities.resume {
        return ResumeOffer::Available;
    }
    let code = info
        .capability_refusals
        .as_ref()
        .and_then(|refusals| refusals.resume.as_deref())
        .unwrap_or(NOT_RESUMABLE);
    ResumeOffer::Refused(refusal_sentence(code))
}

/// A short sentence for a `session.resume` refusal code.
pub fn refusal_sentence(code: &str) -> String {
    match code {
        "not_resumable" | "agent_not_resumable" => {
            "This session has no saved agent conversation to resume.".to_owned()
        }
        "agent_binary_missing" => "The agent program can no longer be found.".to_owned(),
        "agent_native_reference_missing" => {
            "The agent's saved conversation can no longer be found.".to_owned()
        }
        PROFILE_CHANGED_CODE => {
            "The agent profile changed since launch; resuming needs the owner's approval."
                .to_owned()
        }
        "runtime_supervision_ambiguous" => {
            "The previous run could not be confirmed ended; try again later.".to_owned()
        }
        "task_session_ended" => {
            "The session belongs to an ended task; continue it from the task.".to_owned()
        }
        other => format!("The daemon refuses to resume it ({}).", sanitize(other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn info(patch: Value) -> SessionInfo {
        let mut base = json!({
            "id": "s-1",
            "capabilities": {"resume": true, "fork": false},
            "agent": "claude",
            "agent_base": "claude",
            "cwd": "/tmp",
            "pid": 1,
            "cols": 80,
            "rows": 24,
            "state": "stopped",
            "state_source": "screen",
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z",
        });
        base.as_object_mut()
            .unwrap()
            .extend(patch.as_object().unwrap().clone());
        serde_json::from_value(base).expect("session fixture")
    }

    #[test]
    fn ended_with_capability_is_available() {
        assert_eq!(resume_offer(&info(json!({}))), ResumeOffer::Available);
    }

    #[test]
    fn live_session_has_no_offer() {
        assert_eq!(
            resume_offer(&info(json!({"state": "running"}))),
            ResumeOffer::NotApplicable
        );
    }

    #[test]
    fn lost_runtime_counts_as_ended() {
        let lost = info(json!({
            "state": "running",
            "runtime": {"state": "lost", "runtime_generation": "1", "worker_instance_id": "w-1"},
        }));
        assert_eq!(resume_offer(&lost), ResumeOffer::Available);
    }

    #[test]
    fn refusal_code_from_the_snapshot_names_the_reason() {
        let refused = info(json!({
            "capabilities": {"resume": false, "fork": false},
            "capability_refusals": {"resume": "agent_binary_missing"},
        }));
        assert_eq!(
            resume_offer(&refused),
            ResumeOffer::Refused(refusal_sentence("agent_binary_missing"))
        );
    }

    #[test]
    fn false_capability_without_a_code_is_not_resumable() {
        let plain = info(json!({"capabilities": {"resume": false, "fork": false}}));
        assert_eq!(
            resume_offer(&plain),
            ResumeOffer::Refused(refusal_sentence(NOT_RESUMABLE))
        );
    }

    #[test]
    fn unknown_code_is_sanitized_into_the_sentence() {
        assert!(refusal_sentence("new_code\u{1b}[0m").contains("new_code"));
        assert!(!refusal_sentence("new_code\u{1b}[0m").contains('\u{1b}'));
    }
}
