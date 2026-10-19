use pohunek_client::protocol::SessionListParams;
use pohunek_client::protocol::method::SessionList;
use pohunek_client::{Client, ClientOptions, OriginSource};

use crate::config::DaemonConfig;
use crate::error::CoreError;
use crate::session::SessionRow;
use crate::socket::local_socket_path;

/// Connects to the local daemon and returns every session it reports.
///
/// The origin is omitted so a companion started inside a Pohunek session does
/// not tag its requests with that session's identity.
pub async fn fetch_sessions(config: &DaemonConfig) -> Result<Vec<SessionRow>, CoreError> {
    let socket = local_socket_path()?;
    let options = ClientOptions::default()
        .with_origin_source(OriginSource::Omitted)
        .with_connect_timeout(config.connect_timeout())
        .with_request_timeout(config.request_timeout());
    let mut client = Client::connect_local_with_options(&socket, options).await?;
    let sessions = client
        .call::<SessionList>(SessionListParams::default())
        .await?;
    Ok(sessions.into_iter().map(SessionRow::from_local).collect())
}
