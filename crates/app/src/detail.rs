//! The session detail view: fills the window's detail properties from the
//! session's snapshot when the sidebar asks for it.

use pohunek_companion_core::{SessionKey, SessionRow, clip_middle};
use slint::{ComponentHandle, ModelRc, VecModel};
use tracing::{info, warn};

use crate::{AppWindow, DetailFieldData, DetailSectionData, SharedLayout, lock, owner_state_kind};

/// Longest value shown on a detail line; the copied text is never clipped.
const VALUE_MAX_CHARS: usize = 32;

fn section_data(row: &SessionRow) -> Vec<DetailSectionData> {
    row.details
        .sections
        .iter()
        .map(|section| DetailSectionData {
            title: section.title.into(),
            fields: ModelRc::new(VecModel::from(
                section
                    .fields
                    .iter()
                    .map(|field| DetailFieldData {
                        label: field.label.into(),
                        shown: clip_middle(&field.value, VALUE_MAX_CHARS).into(),
                        value: field.value.as_str().into(),
                        link: field.link,
                    })
                    .collect::<Vec<_>>(),
            )),
        })
        .collect()
}

/// Wires the sidebar's details request to the detail view.
pub fn install(window: &AppWindow, layout: SharedLayout) {
    let weak = window.as_weak();
    window.on_request_details(move |host, id| {
        let Some(window) = weak.upgrade() else { return };
        let key = SessionKey {
            host: host.to_string(),
            session_id: id.to_string(),
        };
        let row = lock(&layout)
            .rows
            .iter()
            .find(|row| row.key == key)
            .cloned();
        let Some(row) = row else {
            warn!(host = %host, session = %id, "details requested for a session that is gone");
            return;
        };
        info!(host = %host, session = %id, "session details opened");
        window.set_detail_name(row.display_name.as_str().into());
        window.set_detail_host_label(row.host_label.clone().unwrap_or_default().into());
        window.set_detail_owner_state(owner_state_kind(row.owner_state));
        window.set_detail_owner_label(row.owner_state.label().into());
        window.set_detail_can_resume(row.resume.is_available() && !row.host_unreachable);
        window.set_detail_resume_note(row.resume.note().into());
        window.set_detail_host(host);
        window.set_detail_id(id);
        window.set_detail_copy_all(row.details.copy_all(&row.display_name).into());
        window.set_detail_sections(ModelRc::new(VecModel::from(section_data(&row))));
        window.set_detail_open(true);
    });
}
