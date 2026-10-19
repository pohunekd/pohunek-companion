use pohunek_client::LOCAL_HOST;
use pohunek_client::protocol::SessionInfo;

/// Identity of a session across hosts. Only the local daemon is used so far.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionKey {
    pub host: String,
    pub session_id: String,
}

/// The facts the sidebar list shows for one session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRow {
    pub key: SessionKey,
    /// Owner-set name, else the session id.
    pub display_name: String,
    pub agent: String,
    pub state: String,
}

impl SessionRow {
    pub(crate) fn from_local(info: SessionInfo) -> Self {
        let display_name = info.name.unwrap_or_else(|| info.id.0.clone());
        Self {
            key: SessionKey {
                host: LOCAL_HOST.to_owned(),
                session_id: info.id.0,
            },
            display_name,
            agent: info.agent,
            state: info.state.as_str().to_owned(),
        }
    }
}
