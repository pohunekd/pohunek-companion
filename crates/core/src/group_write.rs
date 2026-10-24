//! Writes the `companion.group` key of a session through `session.set_metadata`.

use std::collections::BTreeMap;

use pohunek_client::ClientError;
use pohunek_client::protocol::method::SessionSetMetadata;
use pohunek_client::protocol::{SessionId, SessionSetMetadataParams};
use tracing::{error, warn};

use crate::config::DaemonConfig;
use crate::connect::{connect_host, socket_for_host};
use crate::group_key::{GROUP_KEY, check_group_name};

/// Daemon refusals that mean the session cannot carry the key at all: an
/// external session, a runtime that is not launchable on the host, or a
/// daemon without the method.
const UNWRITABLE_CODES: [&str; 6] = [
    "session_external_read_only",
    "agent_kind_unsupported",
    "runtime_builtin_removed",
    "runtime_not_installed",
    "runtime_incompatible",
    "method_not_found",
];

/// Why `session.set_metadata` did not happen; the message is shown to the
/// owner as is.
#[derive(Debug, thiserror::Error)]
pub enum GroupWriteError {
    /// The daemon answered with a typed refusal.
    #[error("{message}")]
    Refused { code: String, message: String },
    #[error("{0}")]
    Failed(String),
}

impl GroupWriteError {
    /// The refusal says the session can never carry the key, so its group
    /// stays a local placement.
    pub fn is_unwritable(&self) -> bool {
        matches!(self, Self::Refused { code, .. } if UNWRITABLE_CODES.contains(&code.as_str()))
    }

    /// The daemon's error code, or `transport` when the call never got an answer.
    pub fn code(&self) -> &str {
        match self {
            Self::Refused { code, .. } => code,
            Self::Failed(_) => "transport",
        }
    }
}

/// Sets `companion.group` of a session to `name`, or removes the key for
/// `None`. The caller has checked the name (`check_group_name`). `host` is the
/// local host or the route of the remote host owning the session.
pub async fn set_group(
    daemon: &DaemonConfig,
    host: &str,
    session_id: &str,
    name: Option<&str>,
) -> Result<(), GroupWriteError> {
    let socket = socket_for_host(host).map_err(|err| GroupWriteError::Failed(err.to_string()))?;
    let mut client = connect_host(host, &socket, daemon)
        .await
        .map_err(|err| GroupWriteError::Failed(err.to_string()))?;
    let params = SessionSetMetadataParams {
        session_id: SessionId(session_id.to_owned()),
        metadata: BTreeMap::from([(GROUP_KEY.to_owned(), name.map(str::to_owned))]),
    };
    match client.call::<SessionSetMetadata>(params).await {
        Ok(_) => Ok(()),
        Err(ClientError::Protocol(err)) => Err(GroupWriteError::Refused {
            code: err.code,
            message: err.msg,
        }),
        Err(err) => Err(GroupWriteError::Failed(err.to_string())),
    }
}

/// A session a group write is meant for, with what the write needs to know.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteTarget {
    pub host: String,
    pub session_id: String,
    /// Display name of the host for messages.
    pub host_label: String,
    /// The host's loop is connected; a write to an unreachable host is not sent.
    pub reachable: bool,
    /// An external session; never sent a request.
    pub external: bool,
}

/// How the write of one session ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteOutcome {
    /// The daemon holds the key; the session event carries it back.
    Written,
    /// The session cannot carry the key: its placement stays local.
    Unwritable,
    /// The session was left as it was.
    Failed { host_label: String, reason: String },
}

/// Writes `name` (`None` removes the key) to one session and logs a failure
/// with the host, the session id and the error code, never the name.
pub async fn write_group(
    daemon: &DaemonConfig,
    target: &WriteTarget,
    name: Option<&str>,
    max_bytes: usize,
) -> WriteOutcome {
    let failed = |reason: String| WriteOutcome::Failed {
        host_label: target.host_label.clone(),
        reason,
    };
    if target.external {
        return WriteOutcome::Unwritable;
    }
    if let Some(name) = name
        && let Err(problem) = check_group_name(name, max_bytes)
    {
        warn!(host = %target.host, session = %target.session_id, reason = problem.code(), "group write skipped");
        return WriteOutcome::Unwritable;
    }
    if !target.reachable {
        warn!(host = %target.host, session = %target.session_id, code = "not_connected", "group write skipped");
        return failed("the host is not connected".to_owned());
    }
    match set_group(daemon, &target.host, &target.session_id, name).await {
        Ok(()) => WriteOutcome::Written,
        Err(err) if err.is_unwritable() => {
            warn!(host = %target.host, session = %target.session_id, code = err.code(), "group write refused as unwritable");
            WriteOutcome::Unwritable
        }
        Err(err) => {
            error!(host = %target.host, session = %target.session_id, code = err.code(), "group write failed");
            failed(err.to_string())
        }
    }
}

/// What a batch of writes came to.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WriteSummary {
    /// Sessions written or failed; unwritable ones are neither.
    pub attempted: usize,
    /// Failures in order: host label and reason.
    pub failures: Vec<(String, String)>,
    /// Indexes into the targets of the sessions that cannot carry the key.
    pub unwritable: Vec<usize>,
}

impl WriteSummary {
    /// The line for the owner, or `None` when every session that could be
    /// written was.
    pub fn message(&self, operation: &str) -> Option<String> {
        let (host, reason) = self.failures.first()?;
        Some(format!(
            "Cannot {operation}: {} of {} sessions were not updated; first failure on {host}: {reason}",
            self.failures.len(),
            self.attempted
        ))
    }
}

/// Writes `name` to every target in turn; a failure does not stop the others.
pub async fn write_all(
    daemon: &DaemonConfig,
    targets: &[WriteTarget],
    name: Option<&str>,
    max_bytes: usize,
) -> WriteSummary {
    let mut summary = WriteSummary::default();
    for (index, target) in targets.iter().enumerate() {
        match write_group(daemon, target, name, max_bytes).await {
            WriteOutcome::Written => summary.attempted += 1,
            WriteOutcome::Unwritable => summary.unwritable.push(index),
            WriteOutcome::Failed { host_label, reason } => {
                summary.attempted += 1;
                summary.failures.push((host_label, reason));
            }
        }
    }
    summary
}
