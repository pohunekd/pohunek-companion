//! The owner-facing state of a session, derived from the daemon's facts.

use std::time::{Duration, Instant};

use pohunek_client::protocol::{
    AgentActivity, BackgroundStatus, RuntimeState, SessionInfo, SessionState, SubagentLifecycle,
};

/// Which owner state a session is in; the rules are the "Tray states" table in
/// `docs/architecture.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OwnerState {
    /// An agent waits for the owner (open attention notification or `blocked`).
    NeedsOwner,
    /// The agent works, or is idle while subagents or background tasks run.
    Working,
    /// Idle with no work for longer than the idle threshold.
    Idle,
    /// The session stopped, finished, failed, or lost its runtime.
    Ended,
    /// No agent activity is reported (for example a plain shell). Counts in
    /// no tray state.
    NoAgentState,
}

impl OwnerState {
    /// Short row label.
    pub fn label(self) -> &'static str {
        match self {
            Self::NeedsOwner => "Needs owner",
            Self::Working => "Working",
            Self::Idle => "Idle",
            Self::Ended => "Ended",
            Self::NoAgentState => "No agent state",
        }
    }
}

/// Whether the session is idle with no subagent or background work, the
/// condition whose duration `idle_since` measures.
pub(crate) fn is_idle_without_work(info: &SessionInfo) -> bool {
    info.activity == Some(AgentActivity::Idle) && !has_running_work(info) && !is_ended(info)
}

fn has_running_work(info: &SessionInfo) -> bool {
    let subagent = info
        .subagents
        .iter()
        .any(|subagent| subagent.lifecycle == SubagentLifecycle::Running);
    let background = info.background.as_ref().is_some_and(|background| {
        background.items.iter().any(|item| {
            matches!(
                item.status,
                BackgroundStatus::Running | BackgroundStatus::Pending
            )
        })
    });
    subagent || background
}

fn is_ended(info: &SessionInfo) -> bool {
    matches!(
        info.state,
        SessionState::Stopped | SessionState::Done | SessionState::Failed
    ) || info
        .runtime
        .as_ref()
        .is_some_and(|runtime| runtime.state == RuntimeState::Lost)
}

/// Derives the owner state. `idle_since` is when the idle-without-work
/// condition was first observed; without it an idle session counts as working,
/// because idleness is only claimed after a full threshold.
pub fn derive(
    info: &SessionInfo,
    open_attention: bool,
    idle_since: Option<Instant>,
    now: Instant,
    idle_threshold: Duration,
) -> OwnerState {
    if is_ended(info) {
        return OwnerState::Ended;
    }
    if open_attention || info.activity == Some(AgentActivity::Blocked) {
        return OwnerState::NeedsOwner;
    }
    match info.activity {
        Some(AgentActivity::Working) => OwnerState::Working,
        Some(AgentActivity::Idle) => {
            let idle_long_enough = !has_running_work(info)
                && idle_since
                    .is_some_and(|since| now.saturating_duration_since(since) >= idle_threshold);
            if idle_long_enough {
                OwnerState::Idle
            } else {
                OwnerState::Working
            }
        }
        _ => OwnerState::NoAgentState,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    const THRESHOLD: Duration = Duration::from_secs(60);

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
        for (key, value) in patch.as_object().expect("patch object") {
            base[key] = value.clone();
        }
        serde_json::from_value(base).expect("session fixture")
    }

    fn subagent(lifecycle: &str) -> Value {
        json!([{
            "id": "sub-1", "provider": "claude", "lifecycle": lifecycle, "revision": "1",
            "started_at_ms": 1, "updated_at_ms": 2,
        }])
    }

    fn background(status: &str) -> Value {
        json!({"revision": "1", "observed_at_ms": 1, "items": [{
            "id": "b-1", "kind": "shell", "status": status, "first_seen_ms": 1, "last_seen_ms": 2,
        }]})
    }

    fn derived(info: &SessionInfo, attention: bool, idle_for: Option<u64>) -> OwnerState {
        let now = Instant::now();
        let since = idle_for.and_then(|secs| now.checked_sub(Duration::from_secs(secs)));
        derive(info, attention, since, now, THRESHOLD)
    }

    #[test]
    fn open_attention_beats_working_and_blocked_is_the_fallback() {
        let working = info(json!({"activity": "working"}));
        assert_eq!(derived(&working, true, None), OwnerState::NeedsOwner);
        assert_eq!(derived(&working, false, None), OwnerState::Working);
        let blocked = info(json!({"activity": "blocked"}));
        assert_eq!(derived(&blocked, false, None), OwnerState::NeedsOwner);
    }

    #[test]
    fn idle_is_working_until_the_threshold_and_with_running_work() {
        let idle = info(json!({"activity": "idle"}));
        assert_eq!(derived(&idle, false, None), OwnerState::Working);
        assert_eq!(derived(&idle, false, Some(59)), OwnerState::Working);
        assert_eq!(derived(&idle, false, Some(60)), OwnerState::Idle);
        let with_subagent = info(json!({"activity": "idle", "subagents": subagent("running")}));
        assert_eq!(
            derived(&with_subagent, false, Some(600)),
            OwnerState::Working
        );
        let finished = info(json!({"activity": "idle", "subagents": subagent("completed")}));
        assert_eq!(derived(&finished, false, Some(600)), OwnerState::Idle);
        for status in ["running", "pending"] {
            let busy = info(json!({"activity": "idle", "background": background(status)}));
            assert_eq!(derived(&busy, false, Some(600)), OwnerState::Working);
        }
        let done = info(json!({"activity": "idle", "background": background("other")}));
        assert_eq!(derived(&done, false, Some(600)), OwnerState::Idle);
    }

    #[test]
    fn ended_and_lost_beat_everything() {
        for patch in [
            json!({"state": "stopped", "activity": "blocked"}),
            json!({"state": "done", "activity": "working"}),
            json!({"state": "failed", "activity": "idle"}),
            json!({"activity": "blocked", "runtime": {
                "state": "lost", "runtime_generation": "1", "worker_instance_id": "w-1"}}),
        ] {
            assert_eq!(derived(&info(patch), true, Some(600)), OwnerState::Ended);
        }
    }

    #[test]
    fn no_activity_is_no_agent_state() {
        assert_eq!(
            derived(&info(json!({})), false, None),
            OwnerState::NoAgentState
        );
    }

    #[test]
    fn idle_condition_excludes_work_and_ended_sessions() {
        assert!(is_idle_without_work(&info(json!({"activity": "idle"}))));
        let busy = info(json!({"activity": "idle", "subagents": subagent("running")}));
        assert!(!is_idle_without_work(&busy));
        let stopped = info(json!({"activity": "idle", "state": "stopped"}));
        assert!(!is_idle_without_work(&stopped));
    }
}
