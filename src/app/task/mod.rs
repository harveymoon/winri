/// This module contains asynchronous `task`s for the iced runtime.
use anyhow::Context;
use iced::Task;

use crate::{
    app::{self},
    system,
    window::Window,
};

pub fn ensure_overlay_not_focused(overlay_window_id: iced::window::Id) -> Task<app::Message> {
    iced::window::raw_id::<app::Message>(overlay_window_id).then(|raw_id| {
        // Failure here is benign and recurring (transient
        // GetForegroundWindow nulls during focus transitions). The old
        // .error().log_err() path emitted ERROR-level lines on every
        // miss — ~100 per session for a fundamentally OK condition,
        // crowding out actual errors. Log at debug.
        if let Err(e) = unfocus_window(raw_id) {
            log::debug!("ensure_overlay_not_focused: {e:#}");
        }
        Task::none()
    })
}

fn unfocus_window(raw_id: u64) -> anyhow::Result<()> {
    let focused_window = Window::focused().context("getting focused window")?;

    let overlay_window = Window::from_safe_hwnd(raw_id).context(format!(
        "given raw id ({raw_id}) is invalid: expected overlay raw id (aka. HWND)"
    ))?;

    let desktop_window = system::get_desktop_window().context("getting desktop window")?;
    if focused_window == overlay_window {
        desktop_window.focus()?;
    }

    Ok(())
}
