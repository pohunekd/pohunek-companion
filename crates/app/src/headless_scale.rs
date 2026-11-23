//! Applies `SLINT_SCALE_FACTOR` on the headless backend.
//!
//! Only the winit backend reads the variable itself, so a headless run
//! (`SLINT_BACKEND=headless-software`, what `scripts/ui-agent` uses) would
//! always render at 1x and its screenshots could not be 2x.

/// Tells `window` to render at the scale factor in `SLINT_SCALE_FACTOR`.
///
/// It must run before the window is shown: the headless backend sizes the
/// physical window once, in the same call `show()` makes.
pub fn apply(window: &slint::Window) -> Result<(), slint::PlatformError> {
    let headless = std::env::var("SLINT_BACKEND")
        .is_ok_and(|name| name.to_lowercase().starts_with("headless"));
    if !headless {
        return Ok(());
    }
    let Ok(value) = std::env::var("SLINT_SCALE_FACTOR") else {
        return Ok(());
    };
    let scale_factor = value.trim().parse::<f32>().ok();
    let Some(scale_factor) = scale_factor.filter(|factor| factor.is_finite() && *factor > 0.0)
    else {
        return Err(slint::PlatformError::Other(format!(
            "SLINT_SCALE_FACTOR must be a positive number, got '{value}'"
        )));
    };
    window.dispatch_event(slint::platform::WindowEvent::ScaleFactorChanged { scale_factor });
    Ok(())
}
