//! The tray's view of the session list: the most urgent state, per-state
//! counts, and the sessions that need the owner.

use time::OffsetDateTime;

use crate::owner_state::OwnerState;
use crate::session::{SessionKey, SessionRow};

/// One menu entry: a session that needs the owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrayEntry {
    pub key: SessionKey,
    pub display_name: String,
    pub agent: String,
    /// Why the session cannot be attached; `None` when it can.
    pub attach_refusal: Option<String>,
}

/// What the tray shows. Ended and no-agent-state sessions never drive it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraySummary {
    /// Most urgent state present (needs owner > idle > working); `None` when
    /// every session is ended, has no agent state, or there are none.
    pub urgent: Option<OwnerState>,
    pub needs_owner: usize,
    pub idle: usize,
    pub working: usize,
    /// Sessions needing the owner: longest waiting first, then by display
    /// name, then by key.
    pub entries: Vec<TrayEntry>,
}

impl TraySummary {
    pub fn from_rows(rows: &[SessionRow]) -> Self {
        let count = |state| rows.iter().filter(|row| row.owner_state == state).count();
        let (needs_owner, idle, working) = (
            count(OwnerState::NeedsOwner),
            count(OwnerState::Idle),
            count(OwnerState::Working),
        );
        let urgent = if needs_owner > 0 {
            Some(OwnerState::NeedsOwner)
        } else if idle > 0 {
            Some(OwnerState::Idle)
        } else if working > 0 {
            Some(OwnerState::Working)
        } else {
            None
        };
        let mut needing: Vec<&SessionRow> = rows
            .iter()
            .filter(|row| row.owner_state == OwnerState::NeedsOwner)
            .collect();
        needing.sort_by(|a, b| needs_owner_order(a, b));
        let entries = needing
            .into_iter()
            .map(|row| TrayEntry {
                key: row.key.clone(),
                display_name: row.display_name.clone(),
                agent: row.agent.clone(),
                attach_refusal: row.attach_refusal.as_ref().map(ToString::to_string),
            })
            .collect();
        Self {
            urgent,
            needs_owner,
            idle,
            working,
            entries,
        }
    }
}

/// Order of sessions that need the owner: longest waiting first, then by
/// display name, then by key. Shared by the tray menu and the Needs-you section.
pub(crate) fn needs_owner_order(a: &SessionRow, b: &SessionRow) -> std::cmp::Ordering {
    wait_order(a.waiting_since, b.waiting_since)
        .then_with(|| a.display_name.cmp(&b.display_name))
        .then_with(|| a.key.host.cmp(&b.key.host))
        .then_with(|| a.key.session_id.cmp(&b.key.session_id))
}

/// Known waiting times first, oldest first; unknown ones after them.
fn wait_order(a: Option<OffsetDateTime>, b: Option<OffsetDateTime>) -> std::cmp::Ordering {
    match (a, b) {
        (Some(a), Some(b)) => a.cmp(&b),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    }
}

#[cfg(test)]
mod tests {
    use time::format_description::well_known::Rfc3339;

    use super::*;

    fn at(text: &str) -> OffsetDateTime {
        OffsetDateTime::parse(text, &Rfc3339).expect("fixture timestamp")
    }

    fn row(
        id: &str,
        name: &str,
        owner_state: OwnerState,
        waiting_since: Option<OffsetDateTime>,
    ) -> SessionRow {
        SessionRow {
            key: SessionKey {
                host: "local".into(),
                session_id: id.into(),
            },
            display_name: name.into(),
            agent: "claude".into(),
            state: "running".into(),
            owner_state,
            waiting_since,
            badge: None,
            project: None,
            work: None,
            attach_refusal: None,
            can_stop: false,
            can_remove: false,
            open: crate::open::OpenTargets::default(),
        }
    }

    #[test]
    fn needs_owner_beats_idle_and_working_and_counts_match() {
        let rows = [
            row("1", "a", OwnerState::Working, None),
            row("2", "b", OwnerState::Idle, None),
            row("3", "c", OwnerState::NeedsOwner, None),
            row("4", "d", OwnerState::NeedsOwner, None),
        ];
        let summary = TraySummary::from_rows(&rows);
        assert_eq!(summary.urgent, Some(OwnerState::NeedsOwner));
        assert_eq!(summary.needs_owner, 2);
        assert_eq!(summary.entries.len(), 2);
        assert_eq!((summary.idle, summary.working), (1, 1));
        let rest = TraySummary::from_rows(&rows[..2]);
        assert_eq!(rest.urgent, Some(OwnerState::Idle));
        assert_eq!(
            TraySummary::from_rows(&rows[..1]).urgent,
            Some(OwnerState::Working)
        );
    }

    #[test]
    fn ended_and_no_agent_state_never_set_the_icon() {
        let rows = [
            row("1", "a", OwnerState::Ended, None),
            row("2", "b", OwnerState::NoAgentState, None),
        ];
        let summary = TraySummary::from_rows(&rows);
        assert_eq!(summary.urgent, None);
        assert_eq!(
            (summary.needs_owner, summary.idle, summary.working),
            (0, 0, 0)
        );
        assert!(summary.entries.is_empty());
        assert_eq!(TraySummary::from_rows(&[]).urgent, None);
    }

    #[test]
    fn entries_order_longest_waiting_first_then_name_and_key() {
        let early = Some(at("2026-01-01T00:00:00Z"));
        let late = Some(at("2026-01-01T01:00:00Z"));
        let rows = [
            row("5", "zed", OwnerState::NeedsOwner, None),
            row("4", "beta", OwnerState::NeedsOwner, late),
            row("3", "same", OwnerState::NeedsOwner, None),
            row("2", "same", OwnerState::NeedsOwner, None),
            row("1", "omega", OwnerState::NeedsOwner, early),
        ];
        let ids = |rows: &[SessionRow]| -> Vec<String> {
            TraySummary::from_rows(rows)
                .entries
                .into_iter()
                .map(|entry| entry.key.session_id)
                .collect()
        };
        assert_eq!(ids(&rows), ["1", "4", "2", "3", "5"]);
        let mut shuffled = rows.to_vec();
        shuffled.reverse();
        assert_eq!(ids(&shuffled), ids(&rows));
    }
}
