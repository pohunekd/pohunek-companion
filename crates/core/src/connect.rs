use pohunek_client::protocol::SessionListParams;
use pohunek_client::protocol::method::SessionList;
use pohunek_client::{Client, ClientOptions, OriginSource};
use tracing::{error, info};

use crate::config::DaemonConfig;
use crate::error::CoreError;
use crate::session::SessionRow;
use crate::socket::local_socket_path;

/// Connects to the local daemon and returns every session it reports.
///
/// The origin is omitted so a companion started inside a Pohunek session does
/// not tag its requests with that session's identity.
pub async fn fetch_sessions(config: &DaemonConfig) -> Result<Vec<SessionRow>, CoreError> {
    let socket = local_socket_path().inspect_err(|err| {
        error!(error = %err, "cannot resolve the daemon socket");
    })?;
    info!(socket = %socket.display(), "connecting to the daemon");
    let options = ClientOptions::default()
        .with_origin_source(OriginSource::Omitted)
        .with_connect_timeout(config.connect_timeout())
        .with_request_timeout(config.request_timeout());
    let mut client = Client::connect_local_with_options(&socket, options)
        .await
        .inspect_err(|err| {
            error!(socket = %socket.display(), error = %err, "daemon connection failed");
        })?;
    info!(socket = %socket.display(), "connected to the daemon");
    let sessions = client
        .call::<SessionList>(SessionListParams::default())
        .await
        .inspect_err(|err| {
            error!(socket = %socket.display(), error = %err, "session list request failed");
        })?;
    info!(sessions = sessions.len(), "seeded the session list");
    Ok(sessions.into_iter().map(SessionRow::from_local).collect())
}
