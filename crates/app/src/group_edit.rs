//! Layout edits that write `companion.group` to the sessions: they run on the
//! layout worker thread, write first without holding the layout lock, and
//! change the layout only for what the daemon accepted.

use std::future::Future;

use pohunek_companion_core::{
    DaemonConfig, GroupError, INBOX_VALUE, MoveError, Placement, SessionKey, SessionRow,
    WriteOutcome, WriteSummary, WriteTarget, check_group_name, write_all, write_group,
};
use tracing::warn;

use super::{LayoutRequest, LayoutState, Refusal, SharedLayout, group_refusal, lock};

/// Runs the daemon calls of the layout worker.
pub(super) struct Writer {
    runtime: Result<tokio::runtime::Runtime, String>,
}

impl Writer {
    pub(super) fn new() -> Self {
        Self {
            runtime: tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|err| format!("cannot start the background runtime: {err}")),
        }
    }

    fn run<T>(&self, future: impl Future<Output = T>) -> Result<T, String> {
        self.runtime
            .as_ref()
            .map(|runtime| runtime.block_on(future))
            .map_err(Clone::clone)
    }
}

/// Whether `request` writes session metadata and so goes through [`apply`].
pub(super) fn writes_metadata(request: &LayoutRequest) -> bool {
    matches!(
        request,
        LayoutRequest::Drop { .. }
            | LayoutRequest::CreateGroupAndMove { .. }
            | LayoutRequest::ReturnToRule(_)
            | LayoutRequest::RenameGroup { .. }
            | LayoutRequest::DeleteGroup(_)
    )
}

pub(super) fn apply(layout: &SharedLayout, writer: &Writer, request: LayoutRequest) -> Refusal {
    let drop = |message: Option<String>| Refusal {
        drop: message,
        group: None,
    };
    let group = |message: Option<String>| Refusal {
        group: message,
        drop: None,
    };
    match request {
        LayoutRequest::Drop {
            key,
            group: target,
            anchor,
        } => drop(place(layout, writer, &key, &target, anchor.as_ref())),
        LayoutRequest::CreateGroupAndMove { name, key } => {
            let id = {
                let mut state = lock(layout);
                if !state.rows.iter().any(|row| row.key == key) {
                    return drop(Some(format!(
                        "Cannot create the group: {} is no longer listed",
                        key.session_id
                    )));
                }
                match checked_name(&state, &name).and_then(|name| state.store.create_group(&name)) {
                    Ok(id) => id,
                    Err(err) => return group(group_refusal("create the group", "new", &err)),
                }
            };
            drop(place(layout, writer, &key, &id, None))
        }
        LayoutRequest::ReturnToRule(key) => drop(return_to_rule(layout, writer, &key)),
        LayoutRequest::RenameGroup { id, name } => rename(layout, writer, &id, &name),
        LayoutRequest::DeleteGroup(id) => delete(layout, writer, &id),
        _ => {
            warn!("a request that writes no metadata reached the group writer");
            Refusal::default()
        }
    }
}

/// The name trimmed and checked against the configured limit.
pub(super) fn checked_name(state: &LayoutState, name: &str) -> Result<String, GroupError> {
    check_group_name(name, state.group_name_max_bytes)
        .map(str::to_owned)
        .map_err(GroupError::from)
}

pub(super) fn target_of(row: &SessionRow) -> WriteTarget {
    WriteTarget {
        host: row.key.host.clone(),
        session_id: row.key.session_id.clone(),
        host_label: row.host_label.clone().unwrap_or_else(|| "Local".to_owned()),
        reachable: !row.host_unreachable,
        external: row.external,
    }
}

fn targets_of(state: &LayoutState, keys: &[SessionKey]) -> Vec<WriteTarget> {
    keys.iter()
        .filter_map(|key| state.rows.iter().find(|row| row.key == *key))
        .map(target_of)
        .collect()
}

fn daemon_of(state: &LayoutState) -> Result<DaemonConfig, String> {
    state.daemon.clone()
}

/// Moves `key` into group `group_id`. Within its own group nothing is
/// written; into another group the daemon is asked first. Returns the message
/// for the owner when the session did not move.
fn place(
    layout: &SharedLayout,
    writer: &Writer,
    key: &SessionKey,
    group_id: &str,
    anchor: Option<&(SessionKey, bool)>,
) -> Option<String> {
    let refuse = |message: String| {
        warn!(session = %key.session_id, group = %group_id, error = %message, "drop refused");
        Some(format!("Cannot move {}: {message}", key.session_id))
    };
    let (daemon, max_bytes, target, name) = {
        let mut state = lock(layout);
        let row = state.rows.iter().find(|row| row.key == *key).cloned();
        let item_group = state
            .store
            .layout()
            .items()
            .iter()
            .find(|item| item.host == key.host && item.session_id == key.session_id)
            .map(|item| item.group.clone());
        let name = state
            .store
            .layout()
            .groups()
            .iter()
            .find(|group| group.id == group_id)
            .map(|group| group.name.clone());
        if item_group.as_deref() == Some(group_id) || name.is_none() || row.is_none() {
            // The session stays in its group, or the layout refuses the move
            // with its own reason: nothing to write. A key is the record of
            // its session's group, so only a keyless session is pinned.
            let manual = row.as_ref().is_none_or(|row| row.group.is_none());
            let outcome = apply_placement(&mut state, key, group_id, anchor, manual);
            return outcome.err().and_then(|err| refuse(err.to_string()));
        }
        let (Some(row), Some(name)) = (row, name) else {
            return None;
        };
        let daemon = match daemon_of(&state) {
            Ok(daemon) => daemon,
            Err(message) => return refuse(message),
        };
        (daemon, state.group_name_max_bytes, target_of(&row), name)
    };
    let outcome = match writer.run(write_group(&daemon, &target, Some(&name), max_bytes)) {
        Ok(outcome) => outcome,
        Err(message) => return refuse(message),
    };
    let mut state = lock(layout);
    let manual = match outcome {
        WriteOutcome::Written => false,
        WriteOutcome::Unwritable => {
            state.store.mark_not_shared([key.clone()]);
            true
        }
        WriteOutcome::Failed { host_label, reason } => {
            return refuse(format!("{reason} ({host_label})"));
        }
    };
    apply_placement(&mut state, key, group_id, anchor, manual)
        .err()
        .and_then(|err| refuse(err.to_string()))
}

/// Stores the drop position; an anchor that left the group meanwhile puts the
/// session at the group's end.
fn apply_placement(
    state: &mut LayoutState,
    key: &SessionKey,
    group_id: &str,
    anchor: Option<&(SessionKey, bool)>,
    manual: bool,
) -> Result<bool, MoveError> {
    let placement = match anchor {
        Some((anchor, false)) => Placement::Before(anchor),
        Some((anchor, true)) => Placement::After(anchor),
        None => Placement::End,
    };
    match state.store.place_session(key, group_id, placement, manual) {
        Err(MoveError::AnchorNotInGroup(_)) => {
            state
                .store
                .place_session(key, group_id, Placement::End, manual)
        }
        other => other,
    }
}

/// Removes the key of a session that has one, then hands the session back to
/// the placement rules. Returns the message for the owner on failure.
fn return_to_rule(layout: &SharedLayout, writer: &Writer, key: &SessionKey) -> Option<String> {
    let refuse = |message: String| {
        warn!(session = %key.session_id, error = %message, "return to rule refused");
        Some(format!(
            "Cannot return {} to rule placement: {message}",
            key.session_id
        ))
    };
    let (row, daemon, max_bytes) = {
        let state = lock(layout);
        let Some(row) = state.rows.iter().find(|row| row.key == *key).cloned() else {
            return refuse(MoveError::UnknownSession(key.session_id.clone()).to_string());
        };
        (row, daemon_of(&state), state.group_name_max_bytes)
    };
    if row.group.is_some() || row.group_problem.is_some() {
        let daemon = match daemon {
            Ok(daemon) => daemon,
            Err(message) => return refuse(message),
        };
        match writer.run(write_group(&daemon, &target_of(&row), None, max_bytes)) {
            Ok(WriteOutcome::Written) => {}
            Ok(WriteOutcome::Unwritable) => {
                return refuse("the daemon does not accept changes to this session".to_owned());
            }
            Ok(WriteOutcome::Failed { host_label, reason }) => {
                return refuse(format!("{reason} ({host_label})"));
            }
            Err(message) => return refuse(message),
        }
    }
    let mut state = lock(layout);
    let LayoutState { store, rules, .. } = &mut *state;
    store
        .return_to_rule(&row, rules)
        .err()
        .and_then(|err| refuse(err.to_string()))
}

/// What a rename or delete needs, taken under the lock.
struct MemberWrite {
    daemon: DaemonConfig,
    max_bytes: usize,
    targets: Vec<WriteTarget>,
}

fn member_write(state: &LayoutState, group_id: &str) -> Result<MemberWrite, String> {
    let keys = state.store.layout().group_members(group_id, &state.rows);
    Ok(MemberWrite {
        daemon: daemon_of(state)?,
        max_bytes: state.group_name_max_bytes,
        targets: targets_of(state, &keys),
    })
}

/// Writes `name` to the members, marks the ones that cannot carry it as not
/// shared and returns the failure message, if any.
fn write_members(
    layout: &SharedLayout,
    writer: &Writer,
    members: &MemberWrite,
    name: &str,
    operation: &str,
) -> Option<String> {
    let summary: WriteSummary = match writer.run(write_all(
        &members.daemon,
        &members.targets,
        Some(name),
        members.max_bytes,
    )) {
        Ok(summary) => summary,
        Err(message) => return Some(format!("Cannot {operation}: {message}")),
    };
    let unwritable: Vec<SessionKey> = summary
        .unwritable
        .iter()
        .filter_map(|&index| members.targets.get(index))
        .map(|target| SessionKey {
            host: target.host.clone(),
            session_id: target.session_id.clone(),
        })
        .collect();
    if !unwritable.is_empty() {
        lock(layout).store.mark_not_shared(unwritable);
    }
    let message = summary.message(operation);
    if let Some(message) = &message {
        warn!(
            operation,
            failed = summary.failures.len(),
            attempted = summary.attempted,
            "group members not updated"
        );
        return Some(message.clone());
    }
    None
}

/// Writes the new name to every member, then renames the group here. A
/// failed write leaves the group's name, so repeating the rename writes only
/// to the members that still carry the old name.
fn rename(layout: &SharedLayout, writer: &Writer, id: &str, input: &str) -> Refusal {
    let refuse_group = |err: &GroupError| Refusal {
        group: group_refusal("rename the group", id, err),
        drop: None,
    };
    let (name, members) = {
        let state = lock(layout);
        let name = match checked_name(&state, input) {
            Ok(name) => name,
            Err(err) => return refuse_group(&err),
        };
        match state.store.layout().clone().rename_group(id, &name) {
            Err(err) => return refuse_group(&err),
            Ok(false) => return Refusal::default(),
            Ok(true) => {}
        }
        match member_write(&state, id) {
            Ok(members) => (name, members),
            Err(message) => {
                return Refusal {
                    drop: Some(format!("Cannot rename the group: {message}")),
                    group: None,
                };
            }
        }
    };
    if let Some(message) = write_members(layout, writer, &members, &name, "rename the group") {
        return Refusal {
            drop: Some(message),
            group: None,
        };
    }
    match lock(layout).store.rename_group(id, &name) {
        Ok(_) => Refusal::default(),
        Err(err) => refuse_group(&err),
    }
}

/// Writes `Inbox` to every member, then deletes the group here; a failed
/// write leaves the group and its remaining members as they are.
fn delete(layout: &SharedLayout, writer: &Writer, id: &str) -> Refusal {
    let members = {
        let state = lock(layout);
        let known = state
            .store
            .layout()
            .groups()
            .iter()
            .any(|group| group.id == id);
        if !known || id == super::INBOX_ID {
            drop(state);
            return Refusal {
                group: lock(layout)
                    .store
                    .delete_group(id)
                    .err()
                    .and_then(|err| group_refusal("delete the group", id, &err)),
                drop: None,
            };
        }
        match member_write(&state, id) {
            Ok(members) => members,
            Err(message) => {
                return Refusal {
                    drop: Some(format!("Cannot delete the group: {message}")),
                    group: None,
                };
            }
        }
    };
    if let Some(message) = write_members(layout, writer, &members, INBOX_VALUE, "delete the group")
    {
        return Refusal {
            drop: Some(message),
            group: None,
        };
    }
    Refusal {
        group: lock(layout)
            .store
            .delete_group(id)
            .err()
            .and_then(|err| group_refusal("delete the group", id, &err)),
        drop: None,
    }
}
