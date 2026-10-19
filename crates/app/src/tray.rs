//! Maps the core tray summary to the Slint tray component: icon pixels,
//! tooltip text, and menu entries.

use pohunek_companion_core::{OwnerState, TraySummary, clip_display};
use slint::{Color, Image, Rgba8Pixel, SharedPixelBuffer};

use crate::{Theme, Tray, TrayEntryData};

/// Digit glyphs, 3 columns by 5 rows, one bit per cell, most significant bit
/// first. Drawn scaled to the icon so the needs-owner count reads at a glance.
const DIGITS: [[u8; 5]; 10] = [
    [0b111, 0b101, 0b101, 0b101, 0b111],
    [0b010, 0b110, 0b010, 0b010, 0b111],
    [0b111, 0b001, 0b111, 0b100, 0b111],
    [0b111, 0b001, 0b111, 0b001, 0b111],
    [0b101, 0b101, 0b111, 0b001, 0b001],
    [0b111, 0b100, 0b111, 0b001, 0b111],
    [0b111, 0b100, 0b111, 0b101, 0b111],
    [0b111, 0b001, 0b001, 0b001, 0b001],
    [0b111, 0b101, 0b111, 0b101, 0b111],
    [0b111, 0b101, 0b111, 0b001, 0b111],
];
const GLYPH_COLUMNS: u32 = 3;
const GLYPH_ROWS: u32 = 5;
/// The icon draws one digit; larger counts show 9 and the tooltip has the
/// exact number.
const MAX_ICON_DIGIT: usize = 9;

/// Menu text when the daemon is followed and no session needs the owner.
const EMPTY_LABEL: &str = "No sessions need you";

fn state_color(tray: &Tray, urgent: Option<OwnerState>) -> Color {
    let theme = tray.global::<Theme<'_>>();
    match urgent {
        Some(OwnerState::NeedsOwner) => theme.get_owner_needs_owner(),
        Some(OwnerState::Idle) => theme.get_owner_idle(),
        Some(OwnerState::Working) => theme.get_owner_working(),
        _ => theme.get_owner_ended(),
    }
}

fn icon(tray: &Tray, summary: &TraySummary) -> Image {
    let theme = tray.global::<Theme<'_>>();
    let size = theme.get_tray_icon_size().max(GLYPH_ROWS as i32 * 2) as u32;
    let fill = state_color(tray, summary.urgent);
    let digit_color = theme.get_tray_digit();
    let rgba = |color: Color| Rgba8Pixel::new(color.red(), color.green(), color.blue(), 255);
    let digit = (summary.needs_owner > 0).then(|| summary.needs_owner.min(MAX_ICON_DIGIT));
    let scale = size * 3 / 4 / GLYPH_ROWS;
    let (glyph_w, glyph_h) = (GLYPH_COLUMNS * scale, GLYPH_ROWS * scale);
    let (left, top) = ((size - glyph_w) / 2, (size - glyph_h) / 2);

    let mut buffer = SharedPixelBuffer::<Rgba8Pixel>::new(size, size);
    let center = (size as f32 - 1.0) / 2.0;
    let radius = size as f32 / 2.0 - 1.0;
    for (i, pixel) in buffer.make_mut_slice().iter_mut().enumerate() {
        let (x, y) = (i as u32 % size, i as u32 / size);
        let inside = (x as f32 - center).powi(2) + (y as f32 - center).powi(2) <= radius.powi(2);
        let lit = digit.is_some_and(|d| {
            (left..left + glyph_w).contains(&x)
                && (top..top + glyph_h).contains(&y)
                && DIGITS[d][((y - top) / scale) as usize]
                    >> (GLYPH_COLUMNS - 1 - (x - left) / scale)
                    & 1
                    == 1
        });
        *pixel = match (inside, lit) {
            (true, true) => rgba(digit_color),
            (true, false) => rgba(fill),
            _ => Rgba8Pixel::new(0, 0, 0, 0),
        };
    }
    Image::from_rgba8(buffer)
}

fn tooltip(summary: &TraySummary) -> String {
    format!(
        "{} need you, {} idle, {} working",
        summary.needs_owner, summary.idle, summary.working
    )
}

/// Applies `summary` to the tray: icon, tooltip, and the needs-owner entries.
pub fn apply(tray: &Tray, summary: &TraySummary) {
    let max_chars = tray.global::<Theme<'_>>().get_tray_label_max_chars().max(1) as usize;
    let entries: Vec<TrayEntryData> = summary
        .entries
        .iter()
        .map(|entry| TrayEntryData {
            label: clip_display(
                &format!("{} ({})", entry.display_name, entry.agent),
                max_chars,
            )
            .into(),
            host: entry.key.host.clone().into(),
            id: entry.key.session_id.clone().into(),
            attach_refusal: entry.attach_refusal.clone().unwrap_or_default().into(),
        })
        .collect();
    tray.set_tray_icon(icon(tray, summary));
    tray.set_tray_tooltip(tooltip(summary).into());
    tray.set_entries(slint::ModelRc::new(slint::VecModel::from(entries)));
    tray.set_empty_label(EMPTY_LABEL.into());
}

/// Shows that no session data is current: grey icon, no entries, and the
/// reason in the tooltip and the menu.
pub fn apply_offline(tray: &Tray, reason: &str) {
    tray.set_tray_icon(icon(tray, &TraySummary::from_rows(&[])));
    tray.set_tray_tooltip(reason.into());
    tray.set_entries(slint::ModelRc::new(slint::VecModel::from(Vec::<
        TrayEntryData,
    >::new())));
    tray.set_empty_label(reason.into());
}
