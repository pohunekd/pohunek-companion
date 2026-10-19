//! Session and attention state reconciled from a daemon seed plus events.
//!
//! The store holds only what the daemon sent: `session.list` and
//! `notification.list` seed it, `protocol` events keep it current. It performs
//! no I/O and has no UI dependency.

use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

use pohunek_client::LOCAL_HOST;
use pohunek_client::protocol::{
    AgentStateEvent, BackgroundStateEvent, Event, NotificationCreatedEvent,
    NotificationDeletedEvent, NotificationId, NotificationKind, NotificationRecord,
    NotificationStatus, NotificationUpdatedEvent, SessionEvent, SessionId, SessionInfo,
    SessionNativeRecoveredEvent, SessionRuntimeIdentity, SubagentStateEvent, event,
};
use serde::de::DeserializeOwned;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::owner_state::{OwnerState, derive, is_idle_without_work};
use crate::session::SessionKey;

/// Why an event left the store unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum IgnoreReason {
    /// The event names a session the store does not hold.
    UnknownSession,
    /// The event needs a runtime identity and carries none.
    MissingRuntime,
    /// The event's runtime id or generation differs from the stored session's.
    RuntimeMismatch,
    /// An `agent_state` event carries no revision to order it by.
    MissingRevision,
    /// The revision is not newer than the last applied one.
    StaleRevision,
    /// A session snapshot older than the stored one.
    StaleSnapshot,
    /// The payload does not decode as the event it names.
    MalformedPayload,
    /// An event kind the store does not model.
    NotModelled,
}

/// What `apply_event` did with one event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Applied {
    Applied,
    Ignored(IgnoreReason),
}

/// Counts of applied and ignored events since the store was created or the
/// report was last taken, for the caller to log.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReconcileReport {
    pub seeds: u32,
    /// Applied events per event name.
    pub applied: BTreeMap<String, u32>,
    /// Ignored events per event name and reason.
    pub ignored: BTreeMap<(String, IgnoreReason), u32>,
    /// Snapshot events applied although the stored or carried `updated_at`
    /// could not be parsed, so their order could not be checked.
    pub unordered_snapshots: u32,
}

/// What a seed changed compared with the state it replaced.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SeedChange {
    /// Sessions in the seed that the store did not hold.
    pub added: usize,
    /// Sessions the store held that the seed lacks.
    pub removed: usize,
    /// Sessions in both whose snapshot differs, ignoring `updated_at` and
    /// `native_last_activity_at`.
    pub changed: usize,
    /// Attention notifications added, removed, or differing.
    pub attention: usize,
}

impl SeedChange {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// Whether two snapshots of one session differ beyond the activity timestamps,
/// which the daemon advances while a session works without an event for each
/// step.
fn differs_materially(old: &SessionInfo, new: &SessionInfo) -> bool {
    let mut aligned = new.clone();
    aligned.updated_at.clone_from(&old.updated_at);
    aligned
        .native_last_activity_at
        .clone_from(&old.native_last_activity_at);
    *old != aligned
}

/// Last applied `agent_state` evidence for one session.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ActivityBaseline {
    epoch: Option<String>,
    runtime: (String, u64),
    revision: u64,
}

#[derive(Debug, Clone)]
struct Tracked {
    info: SessionInfo,
    baseline: Option<ActivityBaseline>,
    /// When the idle-without-work condition was first observed; `None` while
    /// the condition does not hold.
    idle_since: Option<Instant>,
}

impl Tracked {
    fn new(info: SessionInfo, now: Instant) -> Self {
        let mut tracked = Self {
            info,
            baseline: None,
            idle_since: None,
        };
        tracked.refresh_idle(now);
        tracked
    }

    /// Starts the idle clock when the condition becomes true, keeps it while
    /// it stays true, and clears it when it ends.
    fn refresh_idle(&mut self, now: Instant) {
        if is_idle_without_work(&self.info) {
            self.idle_since.get_or_insert(now);
        } else {
            self.idle_since = None;
        }
    }
}

/// Current sessions and open attention notifications of one daemon.
#[derive(Debug, Clone)]
pub struct SessionStore {
    host: String,
    sessions: HashMap<SessionKey, Tracked>,
    attention: HashMap<NotificationId, NotificationRecord>,
    report: ReconcileReport,
}

impl Default for SessionStore {
    fn default() -> Self {
        Self::new(LOCAL_HOST)
    }
}

impl SessionStore {
    pub fn new(host: impl Into<String>) -> Self {
        Self {
            host: host.into(),
            sessions: HashMap::new(),
            attention: HashMap::new(),
            report: ReconcileReport::default(),
        }
    }

    pub fn session(&self, id: &SessionId) -> Option<&SessionInfo> {
        self.sessions
            .get(&self.key(id))
            .map(|tracked| &tracked.info)
    }

    pub fn sessions(&self) -> impl Iterator<Item = (&SessionKey, &SessionInfo)> {
        self.sessions
            .iter()
            .map(|(key, tracked)| (key, &tracked.info))
    }

    /// Open attention notifications (`agent_blocked`, `approval_required`).
    pub fn attention(&self) -> impl Iterator<Item = &NotificationRecord> {
        self.attention.values()
    }

    /// Every session with its owner state at `now`.
    pub fn sessions_with_state(
        &self,
        now: Instant,
        idle_threshold: Duration,
    ) -> impl Iterator<Item = (&SessionKey, &SessionInfo, OwnerState)> {
        self.sessions.iter().map(move |(key, tracked)| {
            let state = self.derive_state(tracked, now, idle_threshold);
            (key, &tracked.info, state)
        })
    }

    /// Owner state of one session at `now`; `None` for an unknown session.
    pub fn owner_state(
        &self,
        id: &SessionId,
        now: Instant,
        idle_threshold: Duration,
    ) -> Option<OwnerState> {
        self.sessions
            .get(&self.key(id))
            .map(|tracked| self.derive_state(tracked, now, idle_threshold))
    }

    fn derive_state(
        &self,
        tracked: &Tracked,
        now: Instant,
        idle_threshold: Duration,
    ) -> OwnerState {
        let open_attention = self
            .attention
            .values()
            .any(|record| record.session_id.as_ref() == Some(&tracked.info.id));
        derive(
            &tracked.info,
            open_attention,
            tracked.idle_since,
            now,
            idle_threshold,
        )
    }

    /// The earliest future instant at which a session still inside its idle
    /// threshold turns Idle; `None` when no session is waiting on the
    /// threshold, so no timer is needed.
    pub fn next_idle_deadline(&self, now: Instant, idle_threshold: Duration) -> Option<Instant> {
        self.sessions
            .values()
            .filter_map(|tracked| tracked.idle_since)
            .filter_map(|since| since.checked_add(idle_threshold))
            .filter(|deadline| *deadline > now)
            .min()
    }

    pub fn report(&self) -> &ReconcileReport {
        &self.report
    }

    /// Returns the counts accumulated so far and starts a new tally.
    pub fn take_report(&mut self) -> ReconcileReport {
        std::mem::take(&mut self.report)
    }

    /// Replaces all state with the `session.list` and `notification.list`
    /// results. A session keeps its `agent_state` baseline while its runtime
    /// identity is unchanged, because the seed carries no activity revision
    /// and a buffered older event must not replace newer activity. Returns
    /// what differs from the state that was replaced.
    pub fn apply_seed(
        &mut self,
        sessions: impl IntoIterator<Item = SessionInfo>,
        notifications: impl IntoIterator<Item = NotificationRecord>,
    ) -> SeedChange {
        self.apply_seed_at(sessions, notifications, Instant::now())
    }

    fn apply_seed_at(
        &mut self,
        sessions: impl IntoIterator<Item = SessionInfo>,
        notifications: impl IntoIterator<Item = NotificationRecord>,
        now: Instant,
    ) -> SeedChange {
        let previous = std::mem::take(&mut self.sessions);
        let previous_attention = std::mem::take(&mut self.attention);
        self.sessions = sessions
            .into_iter()
            .map(|info| {
                let key = self.key(&info.id);
                let mut tracked = Tracked::new(info, now);
                tracked.baseline = previous
                    .get(&key)
                    .and_then(|old| old.baseline.clone())
                    .filter(|baseline| runs_on(&tracked.info, &baseline.runtime));
                // A re-seed must not restart the idle clock of a session that
                // has stayed idle.
                if let (Some(since), Some(old)) = (
                    &mut tracked.idle_since,
                    previous.get(&key).and_then(|old| old.idle_since),
                ) {
                    *since = old;
                }
                (key, tracked)
            })
            .collect();
        for record in notifications {
            self.track_attention(record);
        }
        self.report.seeds += 1;

        let mut change = SeedChange::default();
        for (key, tracked) in &self.sessions {
            match previous.get(key) {
                None => change.added += 1,
                Some(old) if differs_materially(&old.info, &tracked.info) => change.changed += 1,
                Some(_) => {}
            }
        }
        change.removed = previous
            .keys()
            .filter(|key| !self.sessions.contains_key(key))
            .count();
        change.attention = self
            .attention
            .iter()
            .filter(|(id, record)| previous_attention.get(id) != Some(record))
            .count()
            + previous_attention
                .keys()
                .filter(|id| !self.attention.contains_key(id))
                .count();
        change
    }

    /// Applies one subscription event and records the outcome in the report.
    pub fn apply_event(&mut self, event: &Event) -> Applied {
        self.apply_event_at(event, Instant::now())
    }

    fn apply_event_at(&mut self, event: &Event, now: Instant) -> Applied {
        let outcome = self.dispatch(event, now);
        let name = event.event().to_owned();
        match outcome {
            Applied::Applied => *self.report.applied.entry(name).or_default() += 1,
            Applied::Ignored(reason) => {
                *self.report.ignored.entry((name, reason)).or_default() += 1;
            }
        }
        outcome
    }

    fn dispatch(&mut self, event: &Event, now: Instant) -> Applied {
        match event.event() {
            event::SESSION_CREATED => decode(event, |payload: SessionEvent| {
                self.insert_snapshot(payload.session, now)
            }),
            event::SESSION_UPDATED
            | event::SESSION_STOPPED
            | event::SESSION_RUNTIME_RECONNECTED
            | event::SESSION_RUNTIME_LOST
            | event::SESSION_RUNTIME_CONFLICT => decode(event, |payload: SessionEvent| {
                self.replace_snapshot(payload.session, now)
            }),
            event::SESSION_NATIVE_RECOVERED => {
                decode(event, |payload: SessionNativeRecoveredEvent| {
                    self.replace_snapshot(payload.session, now)
                })
            }
            event::SESSION_REMOVED => decode(event, |payload: SessionEvent| {
                match self.sessions.remove(&self.key(&payload.session.id)) {
                    Some(_) => Applied::Applied,
                    None => Applied::Ignored(IgnoreReason::UnknownSession),
                }
            }),
            event::AGENT_STATE => decode(event, |payload| self.apply_agent_state(payload, now)),
            event::SUBAGENT_STATE => {
                decode(event, |payload| self.apply_subagent_state(payload, now))
            }
            event::BACKGROUND_STATE => {
                decode(event, |payload| self.apply_background_state(payload, now))
            }
            event::NOTIFICATION_CREATED => decode(event, |payload: NotificationCreatedEvent| {
                self.track_attention(payload.record);
                Applied::Applied
            }),
            event::NOTIFICATION_UPDATED => decode(event, |payload: NotificationUpdatedEvent| {
                self.track_attention(payload.record);
                Applied::Applied
            }),
            event::NOTIFICATION_DELETED => decode(event, |payload: NotificationDeletedEvent| {
                self.attention.remove(&payload.notification_id);
                Applied::Applied
            }),
            _ => Applied::Ignored(IgnoreReason::NotModelled),
        }
    }

    fn key(&self, id: &SessionId) -> SessionKey {
        SessionKey {
            host: self.host.clone(),
            session_id: id.0.clone(),
        }
    }

    /// Stores the snapshot of a created session; an existing entry is
    /// handled like any other snapshot.
    fn insert_snapshot(&mut self, info: SessionInfo, now: Instant) -> Applied {
        if self.sessions.contains_key(&self.key(&info.id)) {
            return self.replace_snapshot(info, now);
        }
        self.sessions
            .insert(self.key(&info.id), Tracked::new(info, now));
        Applied::Applied
    }

    /// Replaces a known session's snapshot unless it is older than the stored
    /// one.
    fn replace_snapshot(&mut self, info: SessionInfo, now: Instant) -> Applied {
        let key = self.key(&info.id);
        let Some(stored) = self.sessions.get_mut(&key) else {
            return Applied::Ignored(IgnoreReason::UnknownSession);
        };
        match is_older(&info.updated_at, &stored.info.updated_at) {
            Some(true) => return Applied::Ignored(IgnoreReason::StaleSnapshot),
            Some(false) => {}
            None => self.report.unordered_snapshots += 1,
        }
        stored.info = info;
        stored.refresh_idle(now);
        Applied::Applied
    }

    /// Finds the session an observation event targets and checks that the
    /// event's runtime is the one the stored snapshot names.
    fn runtime_scoped(
        &mut self,
        id: &SessionId,
        runtime: Option<&SessionRuntimeIdentity>,
    ) -> Result<&mut Tracked, IgnoreReason> {
        let key = self.key(id);
        let tracked = self
            .sessions
            .get_mut(&key)
            .ok_or(IgnoreReason::UnknownSession)?;
        let runtime = runtime.ok_or(IgnoreReason::MissingRuntime)?;
        if runs_on(
            &tracked.info,
            &(
                runtime.worker_instance_id().to_owned(),
                runtime.runtime_generation().get(),
            ),
        ) {
            Ok(tracked)
        } else {
            Err(IgnoreReason::RuntimeMismatch)
        }
    }

    fn apply_agent_state(&mut self, payload: AgentStateEvent, now: Instant) -> Applied {
        let tracked = match self.runtime_scoped(&payload.session_id, payload.runtime.as_ref()) {
            Ok(tracked) => tracked,
            Err(reason) => return Applied::Ignored(reason),
        };
        let (Some(runtime), Some(revision)) = (payload.runtime.as_ref(), payload.revision) else {
            return Applied::Ignored(IgnoreReason::MissingRevision);
        };
        let next = ActivityBaseline {
            epoch: payload.activity_epoch,
            runtime: (
                runtime.worker_instance_id().to_owned(),
                runtime.runtime_generation().get(),
            ),
            revision: revision.get(),
        };
        // A different epoch or runtime starts a new revision sequence.
        if let Some(last) = &tracked.baseline
            && last.epoch == next.epoch
            && last.runtime == next.runtime
            && last.revision >= next.revision
        {
            return Applied::Ignored(IgnoreReason::StaleRevision);
        }
        tracked.info.activity = Some(payload.activity);
        tracked.baseline = Some(next);
        tracked.refresh_idle(now);
        Applied::Applied
    }

    fn apply_subagent_state(&mut self, payload: SubagentStateEvent, now: Instant) -> Applied {
        let tracked = match self.runtime_scoped(&payload.session_id, payload.runtime.as_ref()) {
            Ok(tracked) => tracked,
            Err(reason) => return Applied::Ignored(reason),
        };
        let incoming = payload.subagent;
        let existing = tracked
            .info
            .subagents
            .iter_mut()
            .find(|stored| stored.provider == incoming.provider && stored.id == incoming.id);
        match existing {
            Some(stored) if stored.revision >= incoming.revision => {
                return Applied::Ignored(IgnoreReason::StaleRevision);
            }
            Some(stored) => *stored = incoming,
            None => tracked.info.subagents.push(incoming),
        }
        tracked.refresh_idle(now);
        Applied::Applied
    }

    fn apply_background_state(&mut self, payload: BackgroundStateEvent, now: Instant) -> Applied {
        let tracked = match self.runtime_scoped(&payload.session_id, payload.runtime.as_ref()) {
            Ok(tracked) => tracked,
            Err(reason) => return Applied::Ignored(reason),
        };
        if let (Some(stored), Some(incoming)) = (&tracked.info.background, &payload.background)
            && stored.revision >= incoming.revision
        {
            return Applied::Ignored(IgnoreReason::StaleRevision);
        }
        tracked.info.background = payload.background;
        tracked.refresh_idle(now);
        Applied::Applied
    }

    /// Keeps a record in the attention set while it is an open attention
    /// notification and drops it otherwise.
    fn track_attention(&mut self, record: NotificationRecord) {
        let is_attention = matches!(
            record.kind,
            NotificationKind::AgentBlocked | NotificationKind::ApprovalRequired
        );
        let is_open = matches!(
            record.status,
            NotificationStatus::Unread | NotificationStatus::Read
        );
        if is_attention && is_open {
            self.attention.insert(record.id.clone(), record);
        } else {
            self.attention.remove(&record.id);
        }
    }
}

/// Whether the session's snapshot names the given worker instance and runtime
/// generation.
fn runs_on(info: &SessionInfo, runtime: &(String, u64)) -> bool {
    info.runtime.as_ref().is_some_and(|stored| {
        stored.worker_instance_id.as_deref() == Some(runtime.0.as_str())
            && stored.runtime_generation.get() == runtime.1
    })
}

fn decode<T: DeserializeOwned>(event: &Event, apply: impl FnOnce(T) -> Applied) -> Applied {
    match serde_json::from_value::<T>(event.payload().clone()) {
        Ok(payload) => apply(payload),
        Err(_) => Applied::Ignored(IgnoreReason::MalformedPayload),
    }
}

/// Whether `candidate` is strictly older than `stored`; `None` when either is
/// not an RFC 3339 timestamp.
fn is_older(candidate: &str, stored: &str) -> Option<bool> {
    let candidate = OffsetDateTime::parse(candidate, &Rfc3339).ok()?;
    let stored = OffsetDateTime::parse(stored, &Rfc3339).ok()?;
    Some(candidate < stored)
}

#[cfg(test)]
mod tests {
    use pohunek_client::protocol::{AgentActivity, ProtocolVersion, SubagentLifecycle};
    use serde_json::{Value, json};

    use super::*;

    const WORKER: &str = "w-1";

    fn session_json(id: &str, generation: u64, updated_at: &str) -> Value {
        json!({
            "id": id,
            "capabilities": {"resume": false, "fork": false},
            "agent": "claude",
            "agent_base": "claude",
            "cwd": "/tmp",
            "pid": 1,
            "runtime": {
                "state": "live",
                "runtime_generation": generation.to_string(),
                "worker_instance_id": WORKER,
            },
            "cols": 80,
            "rows": 24,
            "state": "running",
            "state_source": "screen",
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": updated_at,
        })
    }

    fn session(id: &str, generation: u64, updated_at: &str) -> SessionInfo {
        serde_json::from_value(session_json(id, generation, updated_at)).expect("session fixture")
    }

    fn event(name: &str, payload: Value) -> Event {
        Event::new(ProtocolVersion::new(4).expect("version"), name, payload).expect("event fixture")
    }

    fn runtime(generation: u64) -> Value {
        json!({"worker_instance_id": WORKER, "runtime_generation": generation.to_string()})
    }

    fn agent_state(
        id: &str,
        generation: u64,
        epoch: &str,
        revision: &str,
        activity: &str,
    ) -> Event {
        event(
            "agent_state",
            json!({
                "session_id": id,
                "activity": activity,
                "source": "screen",
                "runtime": runtime(generation),
                "activity_epoch": epoch,
                "revision": revision,
            }),
        )
    }

    fn subagent(revision: &str, lifecycle: &str) -> Value {
        json!({
            "id": "sub-1",
            "provider": "claude",
            "lifecycle": lifecycle,
            "revision": revision,
            "started_at_ms": 1,
            "updated_at_ms": 2,
        })
    }

    fn subagent_state(id: &str, generation: u64, subagent: Value) -> Event {
        event(
            "subagent_state",
            json!({"session_id": id, "subagent": subagent, "runtime": runtime(generation)}),
        )
    }

    fn store_with(info: SessionInfo) -> SessionStore {
        let mut store = SessionStore::default();
        store.apply_seed([info], []);
        store
    }

    fn activity(store: &SessionStore, id: &str) -> Option<AgentActivity> {
        store
            .session(&SessionId(id.to_owned()))
            .and_then(|info| info.activity)
    }

    #[test]
    fn agent_state_with_other_runtime_generation_is_ignored() {
        let mut store = store_with(session("s-1", 2, "2026-01-01T00:00:00Z"));
        let outcome = store.apply_event(&agent_state("s-1", 1, "e", "1", "working"));
        assert_eq!(outcome, Applied::Ignored(IgnoreReason::RuntimeMismatch));
        assert_eq!(activity(&store, "s-1"), None);
    }

    #[test]
    fn agent_state_without_runtime_or_for_unknown_session_is_ignored() {
        let mut store = store_with(session("s-1", 1, "2026-01-01T00:00:00Z"));
        let bare = event(
            "agent_state",
            json!({"session_id": "s-1", "activity": "working", "source": "screen"}),
        );
        assert_eq!(
            store.apply_event(&bare),
            Applied::Ignored(IgnoreReason::MissingRuntime)
        );
        assert_eq!(
            store.apply_event(&agent_state("s-9", 1, "e", "1", "working")),
            Applied::Ignored(IgnoreReason::UnknownSession)
        );
        assert_eq!(store.report().ignored.len(), 2);
    }

    #[test]
    fn older_or_equal_revision_is_ignored() {
        let mut store = store_with(session("s-1", 1, "2026-01-01T00:00:00Z"));
        assert_eq!(
            store.apply_event(&agent_state("s-1", 1, "e", "5", "working")),
            Applied::Applied
        );
        for revision in ["5", "4"] {
            assert_eq!(
                store.apply_event(&agent_state("s-1", 1, "e", revision, "idle")),
                Applied::Ignored(IgnoreReason::StaleRevision)
            );
        }
        assert_eq!(activity(&store, "s-1"), Some(AgentActivity::Working));
    }

    #[test]
    fn revisions_compare_numerically() {
        let mut store = store_with(session("s-1", 1, "2026-01-01T00:00:00Z"));
        store.apply_event(&agent_state("s-1", 1, "e", "9", "working"));
        assert_eq!(
            store.apply_event(&agent_state("s-1", 1, "e", "10", "idle")),
            Applied::Applied
        );
        assert_eq!(activity(&store, "s-1"), Some(AgentActivity::Idle));
    }

    #[test]
    fn new_epoch_restarts_the_revision_sequence() {
        let mut store = store_with(session("s-1", 1, "2026-01-01T00:00:00Z"));
        store.apply_event(&agent_state("s-1", 1, "e1", "50", "working"));
        assert_eq!(
            store.apply_event(&agent_state("s-1", 1, "e2", "1", "blocked")),
            Applied::Applied
        );
        assert_eq!(activity(&store, "s-1"), Some(AgentActivity::Blocked));
    }

    #[test]
    fn a_reseed_keeps_the_activity_baseline_of_an_unchanged_runtime() {
        let mut store = store_with(session("s-1", 1, "2026-01-01T00:00:00Z"));
        store.apply_event(&agent_state("s-1", 1, "e", "100", "working"));
        store.apply_seed([session("s-1", 1, "2026-01-01T00:00:01Z")], []);
        assert_eq!(
            store.apply_event(&agent_state("s-1", 1, "e", "3", "idle")),
            Applied::Ignored(IgnoreReason::StaleRevision)
        );
        assert_eq!(
            store.apply_event(&agent_state("s-1", 1, "e", "101", "idle")),
            Applied::Applied
        );
    }

    #[test]
    fn a_reseed_drops_the_activity_baseline_when_the_runtime_changed() {
        let mut store = store_with(session("s-1", 1, "2026-01-01T00:00:00Z"));
        store.apply_event(&agent_state("s-1", 1, "e", "100", "working"));
        store.apply_seed([session("s-1", 2, "2026-01-01T00:00:01Z")], []);
        assert_eq!(
            store.apply_event(&agent_state("s-1", 2, "e", "1", "idle")),
            Applied::Applied
        );
    }

    #[test]
    fn background_state_not_newer_than_the_seeded_snapshot_is_ignored() {
        let mut seeded = session_json("s-1", 1, "2026-01-01T00:00:00Z");
        seeded["background"] = json!({"revision": "5", "observed_at_ms": 1, "items": []});
        let mut store = store_with(serde_json::from_value(seeded).expect("session"));
        let background = |revision: &str| {
            event(
                "background_state",
                json!({
                    "session_id": "s-1",
                    "background": {"revision": revision, "observed_at_ms": 2, "items": []},
                    "runtime": runtime(1),
                }),
            )
        };
        for revision in ["5", "4"] {
            assert_eq!(
                store.apply_event(&background(revision)),
                Applied::Ignored(IgnoreReason::StaleRevision)
            );
        }
        assert_eq!(store.apply_event(&background("6")), Applied::Applied);
    }

    #[test]
    fn newer_subagent_revision_reopens_a_terminal_subagent() {
        let mut seeded = session_json("s-1", 1, "2026-01-01T00:00:00Z");
        seeded["subagents"] = json!([subagent("4", "completed")]);
        let mut store = store_with(serde_json::from_value(seeded).expect("session with subagent"));
        let stale = subagent_state("s-1", 1, subagent("4", "running"));
        assert_eq!(
            store.apply_event(&stale),
            Applied::Ignored(IgnoreReason::StaleRevision)
        );
        let reopened = subagent_state("s-1", 1, subagent("5", "running"));
        assert_eq!(store.apply_event(&reopened), Applied::Applied);
        let info = store
            .session(&SessionId("s-1".to_owned()))
            .expect("session");
        assert_eq!(info.subagents.len(), 1);
        assert_eq!(info.subagents[0].lifecycle, SubagentLifecycle::Running);
    }

    #[test]
    fn background_null_clears_the_snapshot() {
        let mut store = store_with(session("s-1", 1, "2026-01-01T00:00:00Z"));
        let set = event(
            "background_state",
            json!({
                "session_id": "s-1",
                "background": {"revision": "1", "observed_at_ms": 1, "items": []},
                "runtime": runtime(1),
            }),
        );
        assert_eq!(store.apply_event(&set), Applied::Applied);
        let id = SessionId("s-1".to_owned());
        assert!(store.session(&id).expect("session").background.is_some());
        let clear = event(
            "background_state",
            json!({"session_id": "s-1", "background": null, "runtime": runtime(1)}),
        );
        assert_eq!(store.apply_event(&clear), Applied::Applied);
        assert!(store.session(&id).expect("session").background.is_none());
    }

    #[test]
    fn session_removed_drops_the_session() {
        let mut store = store_with(session("s-1", 1, "2026-01-01T00:00:00Z"));
        let removed = event(
            "session_removed",
            json!({"session": session_json("s-1", 1, "2026-01-01T00:00:00Z")}),
        );
        assert_eq!(store.apply_event(&removed), Applied::Applied);
        assert_eq!(store.sessions().count(), 0);
    }

    #[test]
    fn buffered_session_updated_older_than_the_seed_is_dropped_and_a_newer_one_applies() {
        let mut store = store_with(session("s-1", 1, "2026-01-01T00:00:10Z"));
        let mut older = session_json("s-1", 1, "2026-01-01T00:00:05Z");
        older["name"] = json!("older");
        let outcome = store.apply_event(&event("session_updated", json!({"session": older})));
        assert_eq!(outcome, Applied::Ignored(IgnoreReason::StaleSnapshot));
        let mut newer = session_json("s-1", 1, "2026-01-01T00:00:20Z");
        newer["name"] = json!("newer");
        assert_eq!(
            store.apply_event(&event("session_updated", json!({"session": newer}))),
            Applied::Applied
        );
        let info = store
            .session(&SessionId("s-1".to_owned()))
            .expect("session");
        assert_eq!(info.name.as_deref(), Some("newer"));
    }

    #[test]
    fn open_attention_notifications_are_tracked_until_resolved() {
        let record = |status: &str, kind: &str| {
            json!({
                "id": "n-1",
                "source": {"provider": "p", "provider_event": "e", "host_local_source_id": "x"},
                "kind": kind,
                "severity": "action_required",
                "status": status,
                "title": "t",
                "body": "b",
                "created_at": "2026-01-01T00:00:00Z",
            })
        };
        let mut store = SessionStore::default();
        store.apply_event(&event(
            "notification_created",
            json!({"record": record("unread", "turn_completed")}),
        ));
        assert_eq!(store.attention().count(), 0);
        store.apply_event(&event(
            "notification_created",
            json!({"record": record("unread", "approval_required")}),
        ));
        assert_eq!(store.attention().count(), 1);
        store.apply_event(&event(
            "notification_updated",
            json!({"record": record("acknowledged", "approval_required")}),
        ));
        assert_eq!(store.attention().count(), 0);
    }

    #[test]
    fn snapshot_and_removal_events_for_unknown_sessions_are_ignored() {
        let mut store = SessionStore::default();
        let payload = json!({"session": session_json("s-1", 1, "2026-01-01T00:00:00Z")});
        for name in ["session_updated", "session_runtime_lost", "session_removed"] {
            assert_eq!(
                store.apply_event(&event(name, payload.clone())),
                Applied::Ignored(IgnoreReason::UnknownSession)
            );
        }
        assert_eq!(store.sessions().count(), 0);
        assert_eq!(
            store.apply_event(&event("session_created", payload)),
            Applied::Applied
        );
        assert_eq!(store.sessions().count(), 1);
    }

    #[test]
    fn observation_events_need_the_stored_worker_instance() {
        let mut store = store_with(session("s-1", 1, "2026-01-01T00:00:00Z"));
        let other_worker = json!({"worker_instance_id": "w-2", "runtime_generation": "1"});
        let events = [
            event(
                "agent_state",
                json!({"session_id": "s-1", "activity": "idle", "source": "screen",
                       "runtime": other_worker, "activity_epoch": "e", "revision": "1"}),
            ),
            event(
                "subagent_state",
                json!({"session_id": "s-1", "subagent": subagent("1", "running"),
                       "runtime": other_worker}),
            ),
            event(
                "background_state",
                json!({"session_id": "s-1", "background": null, "runtime": other_worker}),
            ),
        ];
        for event in &events {
            assert_eq!(
                store.apply_event(event),
                Applied::Ignored(IgnoreReason::RuntimeMismatch)
            );
        }
        let bare = event(
            "subagent_state",
            json!({"session_id": "s-1", "subagent": subagent("1", "running")}),
        );
        assert_eq!(
            store.apply_event(&bare),
            Applied::Ignored(IgnoreReason::MissingRuntime)
        );
    }

    #[test]
    fn unmodelled_and_malformed_events_are_counted() {
        let mut store = SessionStore::default();
        let attach = event(
            "attach_opened",
            json!({"session_id": "s-1", "stream_id": "x"}),
        );
        assert_eq!(
            store.apply_event(&attach),
            Applied::Ignored(IgnoreReason::NotModelled)
        );
        let bad = event("session_updated", json!({"session": 1}));
        assert_eq!(
            store.apply_event(&bad),
            Applied::Ignored(IgnoreReason::MalformedPayload)
        );
        let report = store.take_report();
        assert_eq!(report.ignored.values().sum::<u32>(), 2);
        assert!(store.report().ignored.is_empty());
    }

    #[test]
    fn seed_reports_added_removed_and_changed_sessions() {
        let mut store = SessionStore::default();
        let first = store.apply_seed(
            [
                session("s-1", 1, "2026-01-01T00:00:01Z"),
                session("s-2", 1, "2026-01-01T00:00:01Z"),
            ],
            [],
        );
        assert_eq!((first.added, first.removed, first.changed), (2, 0, 0));
        let same = store.apply_seed(
            [
                session("s-1", 1, "2026-01-01T00:00:05Z"),
                session("s-2", 1, "2026-01-01T00:00:01Z"),
            ],
            [],
        );
        assert!(same.is_empty());
        let next = store.apply_seed(
            [
                session("s-1", 2, "2026-01-01T00:00:09Z"),
                session("s-3", 1, "2026-01-01T00:00:01Z"),
            ],
            [],
        );
        assert_eq!((next.added, next.removed, next.changed), (1, 1, 1));
    }

    #[test]
    fn idle_clock_starts_once_and_resets_when_the_condition_ends() {
        let threshold = Duration::from_secs(60);
        let t0 = Instant::now();
        let mut store = SessionStore::default();
        let id = SessionId("s-1".to_owned());
        let mut seeded = session_json("s-1", 1, "2026-01-01T00:00:00Z");
        seeded["activity"] = json!("idle");
        store.apply_seed_at(
            [serde_json::from_value(seeded).expect("idle session")],
            [],
            t0,
        );
        let at = |secs| t0 + Duration::from_secs(secs);
        assert_eq!(store.next_idle_deadline(at(1), threshold), Some(at(60)));
        assert_eq!(
            store.owner_state(&id, at(59), threshold),
            Some(OwnerState::Working)
        );
        assert_eq!(
            store.owner_state(&id, at(60), threshold),
            Some(OwnerState::Idle)
        );
        assert_eq!(store.next_idle_deadline(at(60), threshold), None);

        // A re-seed of a still-idle session keeps the original idle clock.
        let mut reseeded = session_json("s-1", 1, "2026-01-01T00:00:30Z");
        reseeded["activity"] = json!("idle");
        store.apply_seed_at(
            [serde_json::from_value(reseeded).expect("idle session")],
            [],
            at(30),
        );
        assert_eq!(
            store.owner_state(&id, at(60), threshold),
            Some(OwnerState::Idle)
        );

        // A running subagent ends the condition; finishing it restarts the clock.
        let running = subagent_state("s-1", 1, subagent("1", "running"));
        store.apply_event_at(&running, at(70));
        assert_eq!(store.next_idle_deadline(at(71), threshold), None);
        let done = subagent_state("s-1", 1, subagent("2", "completed"));
        store.apply_event_at(&done, at(100));
        assert_eq!(store.next_idle_deadline(at(101), threshold), Some(at(160)));
    }
}
