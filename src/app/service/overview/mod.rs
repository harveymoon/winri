//! Overview mode — a single fullscreen window that shows live DWM thumbnails
//! of all tiled windows. Click a thumbnail to jump to it; Win+↓ to exit.
//!
//! Architecture: one iced window (`overview_window_id`) is opened on entry
//! and acts as the destination for N DWM thumbnail registrations — one per
//! source window. Each thumbnail's `rcDestination` places it inside the
//! overview window's client area. Mouse events are delivered to the overview
//! window normally (no passthrough); clicks are hit-tested against the
//! thumbnail rects to figure out which source window to jump to.

mod thumbnail;

use anyhow::Context;
use iced::Task;
use itertools::Itertools;

use crate::{
    app::{self, Mode, service::overview::thumbnail::ThumbnailId},
    window::Window,
};

pub use thumbnail::ThumbnailRect;

pub struct State {
    /// The single fullscreen iced window hosting the overview.
    pub overview_window_id: iced::window::Id,
    /// All bound DWM thumbnails, in display order.
    pub thumbnails: Vec<Thumbnail>,
    /// Last known cursor position inside the overview window (window coords).
    /// Used to hit-test release events.
    pub cursor_pos: Option<iced::Point>,
    /// Cursor position at the most recent left-mouse-down inside the overview
    /// window. Cleared on mouse-up. If the cursor moved more than
    /// `CLICK_DRAG_THRESHOLD_PX` between down and up, the gesture is treated
    /// as a drag (currently a no-op; drag-reorder lands in a later phase).
    pub press_pos: Option<iced::Point>,
    /// Index in `thumbnails` of the thumbnail under the cursor at mouse-down.
    /// `None` if the press wasn't on any thumbnail. Used to identify the
    /// drag source on the matching mouse-up.
    pub drag_source_idx: Option<usize>,
}

/// Maximum pixel distance the cursor may move between mouse-down and mouse-up
/// to still count as a click rather than a drag.
pub const CLICK_DRAG_THRESHOLD_PX: f32 = 5.0;

/// Turn a process executable name like "chrome.exe" into a display name
/// like "Chrome" for the overview label. We strip the `.exe` suffix and
/// capitalize the first character; CamelCase names are left intact
/// ("WindowsTerminal.exe" → "WindowsTerminal") since splitting them
/// reliably is more trouble than it's worth.
fn process_name_to_app_name(process_name: &str) -> String {
    let stem = process_name
        .strip_suffix(".exe")
        .or_else(|| process_name.strip_suffix(".EXE"))
        .unwrap_or(process_name);
    let mut chars = stem.chars();
    match chars.next() {
        Some(first) => first
            .to_uppercase()
            .chain(chars)
            .collect::<String>(),
        None => stem.to_owned(),
    }
}

/// Outcome of a left-mouse-up over the overview window.
#[derive(Debug, Clone, Copy)]
pub enum ReleaseOutcome {
    /// User clicked on this source window — jump to it.
    Click(Window),
    /// User dragged `src` onto `dst` — reorder so `src` takes `dst`'s slot.
    Reorder { src: Window, dst: Window },
}

pub struct Thumbnail {
    pub thumbnail_id: ThumbnailId,
    pub src: Window,
    pub rect: ThumbnailRect,
    /// Display name of the owning app, derived from the .exe name
    /// (e.g. "chrome.exe" → "Chrome"). Cached at overview entry.
    pub app_name: String,
    /// The source window's title at the time overview opened. Cached because
    /// `Window::title` is a Win32 round-trip we don't want to do on every
    /// canvas redraw.
    pub title: String,
}

#[derive(Debug, Clone)]
pub enum Message {
    /// Fired once the single overview window is created and we have its raw
    /// HWND. We register thumbnails and switch to overview mode in response.
    OverviewWindowCreated {
        id: iced::window::Id,
        raw_handle: u64,
    },
}

impl app::State {
    pub fn prepare_open_overview(&self) -> Task<app::Message> {
        if matches!(self.mode, Mode::Overview(_)) {
            log::warn!(
                "Overview operation requested in {} while already in Overview mode",
                crate::function!()
            );
            return Task::none();
        }

        if self.tiler.windows().next().is_none() {
            // Nothing to show — just no-op.
            return Task::none();
        }

        thumbnail::open_overview_window(self.tiler.screen_size())
    }

    pub fn handle_overview_message(&mut self, message: Message) -> anyhow::Result<()> {
        match message {
            Message::OverviewWindowCreated { id, raw_handle } => {
                self.finalize_open_overview(id, raw_handle)?;
            }
        }

        Ok(())
    }

    fn finalize_open_overview(
        &mut self,
        overview_window_id: iced::window::Id,
        raw_handle: u64,
    ) -> anyhow::Result<()> {
        let windows: Vec<thumbnail::WindowData> = self
            .tiler
            .windows()
            .map(|item| thumbnail::WindowData {
                inner: item.inner,
                width: item.width,
            })
            .collect_vec();

        let layouts = thumbnail::compute_thumbnail_layouts(
            &windows,
            self.tiler.screen_size(),
            10.0,
        );

        // Move all source windows offscreen so they don't compete with their
        // own thumbnails.
        for window in &windows {
            if let Err(e) = window.inner.move_offscreen() {
                log::warn!("Failed to move source window offscreen: {e:#}");
            }
        }

        let dest_window = Window::from_safe_hwnd(raw_handle)
            .context(raw_handle)
            .context("invalid hwnd from overview window")?;

        let mut thumbnails = Vec::new();
        for (window, layout) in windows.iter().zip(layouts.into_iter()) {
            match thumbnail::register_thumbnail(window.inner, dest_window, layout.rect) {
                Ok(thumbnail_id) => {
                    let title = window
                        .inner
                        .title()
                        .ok()
                        .flatten()
                        .unwrap_or_default();
                    let app_name = window
                        .inner
                        .process_name()
                        .ok()
                        .map_or_else(String::new, |p| process_name_to_app_name(&p));
                    thumbnails.push(Thumbnail {
                        thumbnail_id,
                        src: window.inner,
                        rect: layout.rect,
                        app_name,
                        title,
                    });
                }
                Err(e) => {
                    log::warn!("Failed to register DWM thumbnail: {e:#}");
                }
            }
        }

        // Enter Overview mode even if some Win32 calls below misbehave —
        // otherwise the user can get stuck unable to exit overview.
        log::info!(
            "switching to Overview mode ({} thumbnails bound)",
            thumbnails.len()
        );
        self.mode = Mode::Overview(State {
            overview_window_id,
            thumbnails,
            cursor_pos: None,
            press_pos: None,
            drag_source_idx: None,
        });

        // Let API clients see the mode flip immediately.
        self.publish_api_snapshot();

        Ok(())
    }

    pub fn close_overview(&mut self) -> anyhow::Result<Task<app::Message>> {
        let Mode::Overview(state) = &self.mode else {
            log::warn!(
                "Close overview requested in {} while not in Overview mode",
                crate::function!()
            );
            return Ok(Task::none());
        };

        // Best-effort unbind — log and continue on error so a single bad
        // thumbnail can't strand the others.
        for thumb in &state.thumbnails {
            if let Err(e) = thumbnail::unbind_thumbnail(thumb.thumbnail_id) {
                log::warn!("Failed to unbind thumbnail: {e:#}");
            }
        }
        let close_task = iced::window::close::<app::Message>(state.overview_window_id);

        self.switch_to_tiler_mode()?;

        Ok(close_task)
    }

    /// Returns the iced window id of the overview window, if currently in
    /// overview mode. Used by view/event dispatch to detect "are we in the
    /// overview window?".
    pub fn overview_window_id(&self) -> Option<iced::window::Id> {
        if let Mode::Overview(state) = &self.mode {
            Some(state.overview_window_id)
        } else {
            None
        }
    }

    /// Stash the latest cursor position from iced so the next click can be
    /// hit-tested against the thumbnail layout.
    pub fn handle_overview_cursor_moved(
        &mut self,
        window_id: iced::window::Id,
        pos: iced::Point,
    ) {
        if let Mode::Overview(state) = &mut self.mode
            && state.overview_window_id == window_id
        {
            state.cursor_pos = Some(pos);
        }
    }

    /// Record the cursor position at the start of a left-mouse-down on the
    /// overview window so the next mouse-up can decide click vs drag, and
    /// remember which thumbnail (if any) was under the cursor for drag-reorder.
    pub fn handle_overview_mouse_pressed(&mut self, window_id: iced::window::Id) {
        if let Mode::Overview(state) = &mut self.mode
            && state.overview_window_id == window_id
        {
            state.press_pos = state.cursor_pos;
            state.drag_source_idx = state.cursor_pos.and_then(|pos| {
                state.thumbnails.iter().position(|t| {
                    t.rect.contains(f64::from(pos.x), f64::from(pos.y))
                })
            });
        }
    }

    /// Decide what happened on left-mouse-up over the overview window.
    /// Always clears the press/drag-source markers.
    pub fn overview_release_outcome(
        &mut self,
        window_id: iced::window::Id,
    ) -> Option<ReleaseOutcome> {
        let Mode::Overview(state) = &mut self.mode else {
            return None;
        };
        if state.overview_window_id != window_id {
            return None;
        }
        let press = state.press_pos.take();
        let drag_source_idx = state.drag_source_idx.take();
        let current = state.cursor_pos?;
        let press = press?;

        let dx = current.x - press.x;
        let dy = current.y - press.y;
        let distance = (dx * dx + dy * dy).sqrt();

        if distance <= CLICK_DRAG_THRESHOLD_PX {
            // Click: jump to the thumbnail under the cursor.
            return state
                .thumbnails
                .iter()
                .find(|t| {
                    t.rect
                        .contains(f64::from(current.x), f64::from(current.y))
                })
                .map(|t| ReleaseOutcome::Click(t.src));
        }

        // Drag: reorder if the cursor was on a thumbnail at both ends.
        let src_idx = drag_source_idx?;
        let dst_idx = state.thumbnails.iter().position(|t| {
            t.rect
                .contains(f64::from(current.x), f64::from(current.y))
        })?;
        if src_idx == dst_idx {
            return None;
        }
        Some(ReleaseOutcome::Reorder {
            src: state.thumbnails[src_idx].src,
            dst: state.thumbnails[dst_idx].src,
        })
    }

    /// Reorder the tiler so `src` takes `dst`'s slot, then refresh DWM
    /// thumbnail rects in-place so the overview UI matches.
    pub fn reorder_overview(&mut self, src: Window, dst: Window) -> anyhow::Result<()> {
        self.tiler.reorder(src, dst);

        // Recompute layouts in the new order.
        let windows: Vec<thumbnail::WindowData> = self
            .tiler
            .windows()
            .map(|item| thumbnail::WindowData {
                inner: item.inner,
                width: item.width,
            })
            .collect();
        let layouts =
            thumbnail::compute_thumbnail_layouts(&windows, self.tiler.screen_size(), 10.0);

        let Mode::Overview(state) = &mut self.mode else {
            return Ok(());
        };

        for thumb in &mut state.thumbnails {
            let Some(idx) = windows.iter().position(|w| w.inner == thumb.src) else {
                continue;
            };
            let new_rect = layouts[idx].rect;
            thumb.rect = new_rect;
            if let Err(e) = thumbnail::update_thumbnail_rect(thumb.thumbnail_id, new_rect) {
                log::warn!("Failed to update DWM thumbnail rect during reorder: {e:#}");
            }
        }

        Ok(())
    }
}
