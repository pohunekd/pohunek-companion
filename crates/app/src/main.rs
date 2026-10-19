mod reorder;

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use slint::{ComponentHandle, Image, ModelRc, Rgba8Pixel, SharedPixelBuffer, VecModel};

slint::include_modules!();

// Spike-only tunables; the real app reads them from companion.toml.
const APP_ID: &str = "pohunek-companion";
const ROWS_PER_GROUP: usize = 15;
const TRAY_TICK: Duration = Duration::from_secs(2);
const TRAY_ICON_SIZE: u32 = 32;
const TRAY_STATES: i32 = 4;

/// Drag payload carried in the `data-transfer` value: the stable row id.
#[derive(Clone, Copy)]
struct RowRef {
    id: i32,
}

fn tray_icon(count: i32) -> Image {
    let color = match count {
        0 => [0x6c, 0x75, 0x7d],
        1 => [0xf0, 0xa0, 0x30],
        _ => [0xe0, 0x40, 0x40],
    };
    let size = TRAY_ICON_SIZE;
    let mut buffer = SharedPixelBuffer::<Rgba8Pixel>::new(size, size);
    let center = (size as f32 - 1.0) / 2.0;
    let radius = size as f32 / 2.0 - 2.0;
    for (i, pixel) in buffer.make_mut_slice().iter_mut().enumerate() {
        let (x, y) = ((i as u32 % size) as f32, (i as u32 / size) as f32);
        let inside = (x - center).powi(2) + (y - center).powi(2) <= radius.powi(2);
        // Extra white bars encode the count, so the icon changes shape as well as colour.
        let bar = count > 0
            && (size / 4..size * 3 / 4).contains(&(y as u32))
            && (x as u32 * (2 * count as u32 + 1) / size) % 2 == 1;
        *pixel = match (inside, bar) {
            (true, true) => Rgba8Pixel::new(255, 255, 255, 255),
            (true, false) => Rgba8Pixel::new(color[0], color[1], color[2], 255),
            _ => Rgba8Pixel::new(0, 0, 0, 0),
        };
    }
    Image::from_rgba8(buffer)
}

fn rows(group: &str, id_base: usize) -> Vec<RowData> {
    (0..ROWS_PER_GROUP)
        .map(|i| RowData {
            id: (id_base + i) as i32,
            title: format!("{group} session {i}").into(),
        })
        .collect()
}

fn apply_move(
    groups: &RefCell<Vec<Vec<RowData>>>,
    models: &[Rc<VecModel<RowData>>],
    id: i32,
    to_group: usize,
    slot: usize,
) {
    let mut groups = groups.borrow_mut();
    let Some((from_group, from_index)) = groups
        .iter()
        .enumerate()
        .find_map(|(g, rows)| rows.iter().position(|r| r.id == id).map(|i| (g, i)))
    else {
        eprintln!("dropped row {id} not found");
        return;
    };
    let Some(landed) = reorder::move_item(&mut groups, from_group, from_index, to_group, slot)
    else {
        eprintln!("drop target out of range: group {to_group}, slot {slot}");
        return;
    };
    let row = models[from_group].remove(from_index);
    models[to_group].insert(landed, row);
}

fn main() -> Result<(), slint::PlatformError> {
    let window = AppWindow::new()?;
    // Compositor window rules match on this id (Wayland app_id); it needs the
    // platform, which the first component creates, and must precede show().
    slint::set_xdg_app_id(APP_ID)?;
    let tray = Tray::new()?;

    let groups = Rc::new(RefCell::new(vec![
        rows("Inbox", 0),
        rows("Review", ROWS_PER_GROUP),
    ]));
    let models: Rc<Vec<Rc<VecModel<RowData>>>> = Rc::new(
        groups
            .borrow()
            .iter()
            .map(|g| Rc::new(VecModel::from(g.clone())))
            .collect(),
    );
    window.set_group_a(ModelRc::from(models[0].clone()));
    window.set_group_b(ModelRc::from(models[1].clone()));

    window.on_payload(|row| {
        let mut transfer = slint::DataTransfer::default();
        let reference = RowRef { id: row.id };
        transfer.set_user_data(Rc::new(reference));
        transfer
    });

    {
        let groups = groups.clone();
        let models = models.clone();
        window.on_drop(move |transfer, to_group, slot| {
            let Some(reference) = transfer
                .user_data()
                .and_then(|data| data.downcast_ref::<RowRef>().copied())
            else {
                eprintln!("drop without a row payload");
                return;
            };
            // The dragged row is destroyed by the model change, so the change
            // runs after the drop callback has returned.
            let groups = groups.clone();
            let models = models.clone();
            slint::Timer::single_shot(Duration::ZERO, move || {
                apply_move(
                    &groups,
                    &models,
                    reference.id,
                    to_group as usize,
                    slot as usize,
                );
            });
        });
    }

    let count = Rc::new(RefCell::new(0i32));
    let refresh_tray = {
        let tray = tray.as_weak();
        let window = window.as_weak();
        move |count: i32| {
            let (Some(tray), Some(window)) = (tray.upgrade(), window.upgrade()) else {
                return;
            };
            tray.set_tray_icon(tray_icon(count));
            tray.set_tray_tooltip(format!("{count} sessions need you").into());
            tray.set_needs_label(format!("Needs you: {count}").into());
            tray.set_ticks_label("Bump counter".into());
            window.set_status(format!("Tray count: {count}").into());
        }
    };
    refresh_tray(0);

    {
        let window = window.as_weak();
        tray.on_show_window(move || {
            if let Some(window) = window.upgrade()
                && let Err(err) = window.show()
            {
                eprintln!("show window: {err}");
            }
        });
    }
    {
        let count = count.clone();
        let refresh = refresh_tray.clone();
        tray.on_bump(move || {
            let mut count = count.borrow_mut();
            *count = (*count + 1) % TRAY_STATES;
            refresh(*count);
        });
    }
    tray.on_quit(|| {
        if let Err(err) = slint::quit_event_loop() {
            eprintln!("quit: {err}");
        }
    });

    let timer = slint::Timer::default();
    {
        let count = count.clone();
        timer.start(slint::TimerMode::Repeated, TRAY_TICK, move || {
            let mut count = count.borrow_mut();
            *count = (*count + 1) % TRAY_STATES;
            refresh_tray(*count);
        });
    }

    window.show()?;
    slint::run_event_loop_until_quit()?;
    Ok(())
}
