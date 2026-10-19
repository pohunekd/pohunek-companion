use std::path::{Path, PathBuf};

use pohunek_client::protocol::Method;
use pohunek_client::protocol::method::{SessionList, Subscribe};
use pohunek_client::protocol::{
    NotificationKind, NotificationListParams, NotificationRecord, Request, SessionInfo,
    SessionListParams,
};
use pohunek_client::{Client, ClientError, ClientOptions, OriginSource, Subscription};
use tracing::{error, info};

use crate::config::{DaemonConfig, NewSessionConfig};
use crate::error::CoreError;
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

/// The origin is omitted so a companion started inside a Pohunek session does
/// not tag its requests with that session's identity.
pub(crate) async fn connect(socket: &Path, config: &DaemonConfig) -> Result<Client, CoreError> {
    let options = ClientOptions::default()
        .with_origin_source(OriginSource::Omitted)
        .with_connect_timeout(config.connect_timeout())
        .with_request_timeout(config.request_timeout());
    Ok(Client::connect_local_with_options(socket, options).await?)
}

/// Connection for one `session.new`: its request timeout is the creation
/// timeout, because creation runs project hooks and waits for the agent.
pub(crate) async fn connect_for_create(
    socket: &Path,
    daemon: &DaemonConfig,
    config: &NewSessionConfig,
) -> Result<Client, CoreError> {
    let options = ClientOptions::default()
        .with_origin_source(OriginSource::Omitted)
        .with_connect_timeout(daemon.connect_timeout())
        .with_request_timeout(config.create_timeout());
    Ok(Client::connect_local_with_options(socket, options).await?)
}

/// Opens a connection and turns it into the daemon's event stream.
pub(crate) async fn subscribe(
    socket: &Path,
    config: &DaemonConfig,
) -> Result<Subscription, CoreError> {
    let client = connect(socket, config).await?;
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
pub(crate) async fn fetch_seed(socket: &Path, config: &DaemonConfig) -> Result<Seed, CoreError> {
    let mut client = connect(socket, config).await?;
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
