//! Maps the core tray summary to the Slint tray component: icon pixels,
//! tooltip text, and menu entries.

use pohunek_companion_core::{OwnerState, TrayEntry, TraySummary, clip_display};
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
/// Opacity of the halo around the figure at full coverage.
const HALO_ALPHA: f32 = 0.7;

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

/// Coverage mask of the herder figure, `MASK_SIDE` x `MASK_SIDE` bytes
/// (`scripts/build-icons`).
const MASK: &[u8] = include_bytes!("../../../assets/icons/pohunek-tray-mask-128.gray");
const MASK_SIDE: u32 = 128;

/// Figure coverage (0.0 to 1.0) of output pixel (`x`, `y`) in a `size` icon:
/// the average of the mask cells the pixel spans.
fn coverage(x: u32, y: u32, size: u32) -> f32 {
    let span = |i: u32| (i * MASK_SIDE / size, ((i + 1) * MASK_SIDE).div_ceil(size));
    let ((x0, x1), (y0, y1)) = (span(x), span(y));
    let sum: u32 = (y0..y1)
        .flat_map(|row| (x0..x1).map(move |col| u32::from(MASK[(row * MASK_SIDE + col) as usize])))
        .sum();
    sum as f32 / (255 * (x1 - x0) * (y1 - y0)) as f32
}

/// Source-over blend of `over` (with `alpha`) onto `under`, straight alpha.
fn blend(over: Color, alpha: f32, under: Rgba8Pixel) -> Rgba8Pixel {
    let under_alpha = under.a as f32 / 255.0;
    let out_alpha = alpha + under_alpha * (1.0 - alpha);
    if out_alpha <= 0.0 {
        return Rgba8Pixel::new(0, 0, 0, 0);
    }
    let channel = |top: u8, bottom: u8| {
        ((top as f32 * alpha + bottom as f32 * under_alpha * (1.0 - alpha)) / out_alpha).round()
            as u8
    };
    Rgba8Pixel::new(
        channel(over.red(), under.r),
        channel(over.green(), under.g),
        channel(over.blue(), under.b),
        (out_alpha * 255.0).round() as u8,
    )
}

/// The herder figure in the state colour with a dark halo so it reads on light
/// and dark bars; a needs-owner count is a badge with one digit in the lower
/// right corner.
fn icon(tray: &Tray, summary: &TraySummary) -> Image {
    let theme = tray.global::<Theme<'_>>();
    let size = theme.get_tray_icon_size().max(GLYPH_ROWS as i32 * 2) as u32;
    let fill = state_color(tray, summary.urgent);
    let (halo, badge, digit_color) = (
        theme.get_tray_halo(),
        theme.get_tray_badge(),
        theme.get_tray_digit(),
    );
    let digit = (summary.needs_owner > 0).then(|| summary.needs_owner.min(MAX_ICON_DIGIT));
    let scale = (size / 3 / GLYPH_ROWS).max(1);
    let (glyph_w, glyph_h) = (GLYPH_COLUMNS * scale, GLYPH_ROWS * scale);
    let (pad, right, bottom) = (1, size - 1, size - 1);
    let (left, top) = (right - pad - glyph_w, bottom - pad - glyph_h);

    let cover: Vec<f32> = (0..size * size)
        .map(|i| coverage(i % size, i / size, size))
        .collect();
    let at = |x: i64, y: i64| {
        if (0..size as i64).contains(&x) && (0..size as i64).contains(&y) {
            cover[(y as u32 * size + x as u32) as usize]
        } else {
            0.0
        }
    };
    let mut buffer = SharedPixelBuffer::<Rgba8Pixel>::new(size, size);
    for (i, pixel) in buffer.make_mut_slice().iter_mut().enumerate() {
        let (x, y) = (i as u32 % size, i as u32 / size);
        let near = (-1..=1)
            .flat_map(|dy| (-1..=1).map(move |dx| (dx, dy)))
            .map(|(dx, dy)| at(x as i64 + dx, y as i64 + dy))
            .fold(0.0, f32::max);
        let figure = blend(halo, near * HALO_ALPHA, Rgba8Pixel::new(0, 0, 0, 0));
        *pixel = blend(fill, cover[i], figure);
        let Some(d) = digit else { continue };
        if (left - pad..right).contains(&x) && (top - pad..bottom).contains(&y) {
            let in_glyph = (left..left + glyph_w).contains(&x)
                && (top..top + glyph_h).contains(&y)
                && DIGITS[d][((y - top) / scale) as usize]
                    >> (GLYPH_COLUMNS - 1 - (x - left) / scale)
                    & 1
                    == 1;
            *pixel = blend(
                if in_glyph { digit_color } else { badge },
                1.0,
                Rgba8Pixel::new(0, 0, 0, 0),
            );
        }
    }
    Image::from_rgba8(buffer)
}

fn tooltip(summary: &TraySummary) -> String {
    let counts = format!(
        "{} need you, {} idle, {} working",
        summary.needs_owner, summary.idle, summary.working
    );
    match summary.unreachable_hosts {
        0 => counts,
        1 => format!("{counts}, 1 host unreachable"),
        n => format!("{counts}, {n} hosts unreachable"),
    }
}

/// Menu text of an entry; a remote session names its host.
fn entry_label(entry: &TrayEntry) -> String {
    match &entry.host_label {
        Some(host) => format!("{} ({}) - {host}", entry.display_name, entry.agent),
        None => format!("{} ({})", entry.display_name, entry.agent),
    }
}

/// Applies `summary` to the tray: icon, tooltip, and the needs-owner entries.
pub fn apply(tray: &Tray, summary: &TraySummary) {
    let max_chars = tray.global::<Theme<'_>>().get_tray_label_max_chars().max(1) as usize;
    let entries: Vec<TrayEntryData> = summary
        .entries
        .iter()
        .map(|entry| TrayEntryData {
            label: clip_display(&entry_label(entry), max_chars).into(),
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
    tray.set_tray_icon(icon(tray, &TraySummary::from_rows(&[], 0)));
    tray.set_tray_tooltip(reason.into());
    tray.set_entries(slint::ModelRc::new(slint::VecModel::from(Vec::<
        TrayEntryData,
    >::new())));
    tray.set_empty_label(reason.into());
}
