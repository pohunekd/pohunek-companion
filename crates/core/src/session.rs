use crate::attach::AttachRefusal;
use crate::badge::WorkBadge;
use crate::owner_state::OwnerState;
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
    /// What the owner should know at a glance.
    pub owner_state: OwnerState,
    /// Pohunek Work link and role, when the metadata carries a link id.
    pub badge: Option<WorkBadge>,
    /// Why the session cannot be attached; `None` when it can.
    pub attach_refusal: Option<AttachRefusal>,
}

impl SessionRow {
    pub(crate) fn from_local(info: SessionInfo, owner_state: OwnerState) -> Self {
        let display_name = info.name.unwrap_or_else(|| info.id.0.clone());
        Self {
            key: SessionKey {
                host: LOCAL_HOST.to_owned(),
                session_id: info.id.0,
            },
            display_name,
            agent: info.agent,
            state: info.state.as_str().to_owned(),
            owner_state,
            badge: WorkBadge::from_metadata(&info.metadata),
            attach_refusal: AttachRefusal::for_session(info.external, info.state),
        }
    }
}
