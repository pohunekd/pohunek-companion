use std::path::{Path, PathBuf};

use pohunek_client::protocol::Method;
use pohunek_client::protocol::method::{SessionList, Subscribe};
use pohunek_client::protocol::{
    NotificationKind, NotificationListParams, NotificationRecord, Request, SessionInfo,
    SessionListParams,
};
use pohunek_client::{
    Client, ClientError, ClientOptions, LOCAL_HOST, OriginSource, Subscription, is_local_host,
    names_this_machine,
};
use tracing::{error, info};

use crate::config::{DaemonConfig, NewSessionConfig};
use crate::error::CoreError;
use crate::routes::route_for;
use crate::socket::local_socket_path;

/// Error code of a `notification.list` page that exceeds the daemon's response
/// limit (class `runtime`).
const NOTIFICATION_LIST_TOO_LARGE: &str = "notification_list_too_large";
const SUBSCRIBE_REQUEST_ID: &str = "companion-subscribe";
/// Notification kinds that can need the owner.
const ATTENTION_KINDS: [NotificationKind; 2] = [
    NotificationKind::AgentBlocked,
    NotificationKind::ApprovalRequired,
];

/// Sessions and open-or-closed attention notifications as the daemon reported
/// them at one moment.
pub(crate) struct Seed {
    pub sessions: Vec<SessionInfo>,
    pub notifications: Vec<NotificationRecord>,
}

/// Resolves and verifies the local daemon socket.
pub(crate) fn resolve_socket() -> Result<PathBuf, CoreError> {
    local_socket_path().map_err(|err| {
        error!(error = %err, "cannot resolve the daemon socket");
        CoreError::from(err)
    })
}

/// The socket a connection to `host` needs: the local daemon's for the local
/// host, none for a remote one (its overlay route is resolved on every
/// connection).
pub(crate) fn socket_for_host(host: &str) -> Result<PathBuf, CoreError> {
    if is_local_host(host) {
        resolve_socket()
    } else {
        Ok(PathBuf::new())
    }
}

/// Connects to the local daemon (`host` is `local`) or to the remote host
/// route `host`; `socket` is only used for the local daemon. The origin is
/// omitted so a companion started inside a Pohunek session does not tag its
/// requests with that session's identity.
pub(crate) async fn connect_host(
    host: &str,
    socket: &Path,
    config: &DaemonConfig,
) -> Result<Client, CoreError> {
    let options = ClientOptions::default()
        .with_origin_source(OriginSource::Omitted)
        .with_connect_timeout(config.connect_timeout())
        .with_request_timeout(config.request_timeout());
    Ok(Client::connect_with_options(&route_for(host), socket, options).await?)
}

/// Refuses a configured host that names this machine, which would list the
/// local sessions twice. Returns the first such target.
pub async fn host_naming_this_machine(
    targets: &[String],
    config: &DaemonConfig,
) -> Result<Option<String>, CoreError> {
    for target in targets {
        if names_this_machine(&route_for(target), config.connect_timeout()).await? {
            return Ok(Some(target.clone()));
        }
    }
    Ok(None)
}

/// Client for `session.new` on `host` (`local` or a remote route): its request
/// timeout is the creation timeout, because creation runs project hooks and
/// waits for the agent. `socket` is only used for the local daemon.
pub(crate) async fn connect_for_create(
    host: &str,
    socket: &Path,
    daemon: &DaemonConfig,
    config: &NewSessionConfig,
) -> Result<Client, CoreError> {
    let options = ClientOptions::default()
        .with_origin_source(OriginSource::Omitted)
        .with_connect_timeout(daemon.connect_timeout())
        .with_request_timeout(config.create_timeout());
    Ok(Client::connect_with_options(&route_for(host), socket, options).await?)
}

/// Client for `session.stop` and `session.remove` on the local daemon or the
/// remote host route `host`, whose request timeout is the longer
/// session-action timeout; `socket` is only used for the local daemon.
pub(crate) async fn connect_for_session_action(
    host: &str,
    socket: &Path,
    config: &DaemonConfig,
) -> Result<Client, CoreError> {
    let options = ClientOptions::default()
        .with_origin_source(OriginSource::Omitted)
        .with_connect_timeout(config.connect_timeout())
        .with_request_timeout(config.session_action_timeout());
    Ok(Client::connect_with_options(&route_for(host), socket, options).await?)
}

/// Client for `host.discover` on the local daemon, whose request timeout is
/// the longer discovery timeout because the daemon probes every peer first.
pub(crate) async fn connect_discover(
    socket: &Path,
    config: &DaemonConfig,
) -> Result<Client, CoreError> {
    let options = ClientOptions::default()
        .with_origin_source(OriginSource::Omitted)
        .with_connect_timeout(config.connect_timeout())
        .with_request_timeout(config.discover_timeout());
    Ok(Client::connect_with_options(LOCAL_HOST, socket, options).await?)
}

/// Opens a connection and turns it into the daemon's event stream.
pub(crate) async fn subscribe(
    host: &str,
    socket: &Path,
    config: &DaemonConfig,
) -> Result<Subscription, CoreError> {
    let client = connect_host(host, socket, config).await?;
    let request = Request::new(
        SUBSCRIBE_REQUEST_ID,
        Subscribe::NAME,
        serde_json::Value::Null,
    )
    .map_err(ClientError::from)?;
    Ok(client.subscribe(&request).await?)
}

/// Reads the session list and the attention notifications over a second
/// connection (`subscribe` consumes its own).
pub(crate) async fn fetch_seed(
    host: &str,
    socket: &Path,
    config: &DaemonConfig,
) -> Result<Seed, CoreError> {
    let mut client = connect_host(host, socket, config).await?;
    let sessions = client
        .call::<SessionList>(SessionListParams::default())
        .await?;
    let mut notifications = Vec::new();
    for kind in ATTENTION_KINDS {
        list_notifications(
            &mut client,
            kind,
            config.notification_page_size,
            &mut notifications,
        )
        .await?;
    }
    Ok(Seed {
        sessions,
        notifications,
    })
}

/// Pages through the notifications of `kind`, halving the page size whenever
/// the daemon reports a page as too large.
async fn list_notifications(
    client: &mut Client,
    kind: NotificationKind,
    initial_limit: u32,
    out: &mut Vec<NotificationRecord>,
) -> Result<(), CoreError> {
    let mut limit = initial_limit;
    let mut cursor = None;
    loop {
        let params = NotificationListParams {
            kind: Some(kind),
            limit: Some(limit),
            cursor: cursor.clone(),
            ..Default::default()
        };
        match client.list_notifications(params).await {
            Ok(page) => {
                out.extend(page.notifications);
                match page.next_cursor {
                    Some(next) => cursor = Some(next),
                    None => return Ok(()),
                }
            }
            Err(ClientError::Protocol(err)) if err.code == NOTIFICATION_LIST_TOO_LARGE => {
                if limit <= 1 {
                    return Err(CoreError::NotificationPageTooLarge);
                }
                limit /= 2;
                info!(
                    kind = kind.as_str(),
                    limit, "notification page too large, halving"
                );
            }
            Err(err) => return Err(err.into()),
        }
    }
}
