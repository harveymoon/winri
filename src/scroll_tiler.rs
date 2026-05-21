use std::{
    collections::HashSet,
    ops::Sub,
    time::{Duration, Instant},
};

use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON};

use anyhow::Context;
use joy_error::log::ResultLogExt;
use log::{debug, info, warn};

use crate::{cast, utils::math::Size, window::Window};

/// Represents a window managed by the tiler.
#[derive(PartialEq)]
pub struct WindowItem {
    /// The managed window.
    pub inner: Window,
    /// The width that has been requested for the window.
    /// Should be handled and cleared during the next `handle_window_snapshot` call.
    /// If `None` during that call, the current window width will be used.
    pub requested_width: Option<f32>,
    /// Keep track of the current width of the window.
    /// Should be updated to always reflect the actual window width.
    pub width: f32,
    /// Last (x, y, width, height) that `layout_windows` issued for this
    /// window. Used to tell the difference between "actually moved" and
    /// "re-applied the same rect" so the focus-border fade isn't disturbed
    /// by passive snapshot updates (e.g. mouse hovers triggering events).
    last_layout: Option<(f32, f32, f32, f32)>,
    /// Most recent `SetWindowRgn` rect we applied to this window, in
    /// window-local coords (left, top, right, bottom). `None` means the
    /// window currently has no winri-managed clip (its full content
    /// renders). Used to skip redundant `SetWindowRgn` calls and to know
    /// when to call `SetWindowRgn(None)` on the transition back to full
    /// visibility.
    last_clip: Option<(i32, i32, i32, i32)>,
}

impl WindowItem {
    pub const fn new(inner: Window, width: f32) -> Self {
        Self {
            inner,
            requested_width: Some(width),
            width,
            last_layout: None,
            last_clip: None,
        }
    }

    fn request_width(&mut self, width: f32) {
        self.requested_width = Some(width);
    }

    fn requested_width(&mut self) -> Option<f32> {
        self.requested_width.take()
    }
}

/// Synchronously-queried "is the left mouse button currently down". Used
/// to detect window drags so the tiler doesn't fight the user mid-drag.
fn is_left_mouse_held() -> bool {
    let raw = unsafe { GetAsyncKeyState(i32::from(VK_LBUTTON.0)) };
    #[allow(clippy::cast_sign_loss)]
    let bits = raw as u16;
    bits & 0x8000 != 0
}

/// Once motion goes quiet for this long, the focus border fades back in.
pub const BORDER_SETTLE_GRACE: Duration = Duration::from_millis(500);
/// Fade duration after the settle grace expires.
pub const BORDER_FADE_IN: Duration = Duration::from_millis(250);

#[derive(Default)]
pub struct ScrollTiler {
    /// The windows managed by the tiler.
    windows: Vec<WindowItem>,
    /// The padding between windows and screen edges.
    padding: f32,
    /// The amount of pixels to resize a window when resizing by a step.
    resize_increment: f32,
    /// The current scroll offset. Used to scroll the tiler view horizontally.
    scroll_offset: f32,
    /// When `Some`, smoothing is active: the tiler is animating
    /// `scroll_offset` toward this value. Set by scroll/focus operations
    /// while [`smooth_scroll_enabled`] is `true`. Cleared once the animation
    /// settles.
    scroll_target: Option<f32>,
    /// Whether to animate scroll changes. Mirrors `tiling.smooth_scroll`.
    smooth_scroll_enabled: bool,
    /// Per-tick interpolation factor. Mirrors `tiling.smooth_scroll_factor`.
    smooth_scroll_factor: f32,
    /// The size of the screen where the tiler is applied.
    screen_size: Size,
    /// The index of the previously focused window. Used as a fallback when the focused window is not tiled.
    previously_focused_window_index: Option<usize>,
    /// Most recent time any window actually moved (scroll, layout, animation
    /// frame). Used to fade the focus border in only once the strip is still.
    last_motion: Option<Instant>,
    /// Was the left mouse button held during the previous snapshot tick?
    /// We compare against the current state to spot the moment a user-drag
    /// ends, which is when we re-check whether any tiled window has been
    /// pulled off the tiling monitor.
    was_mouse_held_last_snapshot: bool,
}

impl ScrollTiler {
    pub fn new(padding: f32, resize_increment: f32, screen_size: Size) -> Self {
        Self {
            padding,
            resize_increment,
            screen_size,
            smooth_scroll_factor: 0.25,
            ..Default::default()
        }
    }

    /// Configure smoothing — typically driven from `config.toml`.
    pub fn set_smoothing(&mut self, enabled: bool, factor: f32) {
        self.smooth_scroll_enabled = enabled;
        self.smooth_scroll_factor = factor.clamp(0.05, 1.0);
    }

    /// Whether a smoothing animation is currently in progress.
    pub const fn is_animating(&self) -> bool {
        self.scroll_target.is_some()
    }

    /// How long ago anything in the strip last moved. `None` until the
    /// first motion event of the session.
    pub fn time_since_motion(&self) -> Option<Duration> {
        self.last_motion.map(|t| t.elapsed())
    }

    /// True while either an animation is in flight or we're still inside the
    /// post-motion grace + fade window. Drives the iced redraw subscription
    /// so the border can fade in even when no other state is changing.
    pub fn wants_redraw(&self) -> bool {
        if self.is_animating() {
            return true;
        }
        self.time_since_motion()
            .map(|elapsed| elapsed < BORDER_SETTLE_GRACE + BORDER_FADE_IN)
            .unwrap_or(false)
    }

    fn mark_motion(&mut self) {
        self.last_motion = Some(Instant::now());
    }

    /// Scroll the strip so the focused window is horizontally centered in
    /// the viewport. No-op if no window is focused. Uses the same smoothing
    /// path as user scrolling.
    pub fn center_focused_window(&mut self) {
        let Some(focus_idx) = self.focus_index() else {
            return;
        };
        let positions = self.windows_positions();
        let focused_x = positions[focus_idx];
        let focused_width = self.windows[focus_idx].width;
        let target = focused_x + focused_width / 2.0 - self.screen_size.width() / 2.0;
        self.set_scroll_offset(target);
    }

    /// Total horizontal extent of the tiled strip, including padding before
    /// the first window and after the last. Used by the API to expose the
    /// scrollable range.
    pub fn total_strip_width(&self) -> f32 {
        if self.windows.is_empty() {
            return 0.0;
        }
        let inner: f32 = self
            .windows
            .iter()
            .map(|w| w.width + self.padding)
            .sum::<f32>()
            - self.padding;
        self.padding.mul_add(2.0, inner)
    }

    /// Advance the smoothing animation by one frame. Returns whether the
    /// animation is still running (so the caller can decide whether to keep
    /// ticking).
    pub fn tick_animation(&mut self) -> bool {
        let Some(target) = self.scroll_target else {
            return false;
        };
        let diff = target - self.scroll_offset;
        if diff.abs() < 0.5 {
            self.scroll_offset = target;
            self.scroll_target = None;
        } else {
            self.scroll_offset += diff * self.smooth_scroll_factor;
        }
        let positions = self.windows_positions();
        self.layout_windows(&positions);
        self.mark_motion();
        self.scroll_target.is_some()
    }

    fn focus_index(&self) -> Option<usize> {
        self.windows
            .iter()
            .position(|item| item.inner.is_focused().unwrap_or(false))
    }

    fn logged_focus_index(&self) -> Option<usize> {
        self.focus_index()
            .context("Focused window is not tiled")
            .info()
            .log_err()
            .ok()
    }

    fn focus_index_with_fallback_and_log(&self) -> Option<usize> {
        self.focus_index()
            .context("Focused window is not tiled, using previously focused window index")
            .info()
            .log_err()
            .ok()
            .or(self.previously_focused_window_index)
            .context("No previously focused window index available")
            .info()
            .log_err()
            .ok()
            .or_else(|| {
                if self.windows.is_empty() {
                    None
                } else {
                    info!("Defaulting to first window");
                    Some(0)
                }
            })
    }

    pub fn windows(&self) -> impl Iterator<Item = &WindowItem> {
        self.windows.iter()
    }

    /// Scroll the tile strip horizontally by `delta_px`. Positive values
    /// shift the viewport rightward (windows slide left). When smoothing is
    /// enabled, the target accumulates and the next animation tick moves
    /// toward it; otherwise the scroll snaps.
    pub fn scroll_by(&mut self, delta_px: f32) {
        if self.smooth_scroll_enabled {
            let base = self.scroll_target.unwrap_or(self.scroll_offset);
            self.scroll_target = Some(base + delta_px);
        } else {
            self.scroll_offset += delta_px;
            let positions = self.windows_positions();
            self.layout_windows(&positions);
        }
        self.mark_motion();
    }

    /// Set an absolute scroll offset.
    pub fn set_scroll_offset(&mut self, offset_px: f32) {
        if self.smooth_scroll_enabled {
            self.scroll_target = Some(offset_px);
        } else {
            self.scroll_offset = offset_px;
            let positions = self.windows_positions();
            self.layout_windows(&positions);
        }
        self.mark_motion();
    }

    pub const fn scroll_offset(&self) -> f32 {
        self.scroll_offset
    }

    pub const fn padding(&self) -> f32 {
        self.padding
    }

    /// Move `src` to occupy the slot currently held by `dst`. Used by
    /// overview's drag-to-reorder. No-op if either window isn't tiled or both
    /// indices match.
    pub fn reorder(&mut self, src: Window, dst: Window) {
        let Some(src_idx) = self.windows.iter().position(|w| w.inner == src) else {
            return;
        };
        let Some(dst_idx) = self.windows.iter().position(|w| w.inner == dst) else {
            return;
        };
        if src_idx == dst_idx {
            return;
        }
        let item = self.windows.remove(src_idx);
        // After remove, `dst_idx` may have shifted by one if it was past `src_idx`.
        let adjusted_dst = if dst_idx > src_idx { dst_idx - 1 } else { dst_idx };
        self.windows.insert(adjusted_dst, item);
    }

    pub fn swap_current_left(&mut self) {
        self.swap_current(-1);
    }

    pub fn swap_current_right(&mut self) {
        self.swap_current(1);
    }

    #[allow(
        clippy::cast_sign_loss,
        reason = "return value is guaranteed to be positive by the clamp call"
    )]
    fn compute_index_for_direction(&self, focus_index: usize, direction: i32) -> usize {
        cast! {
            focus_index => i32,
            self.windows.len() => i32 as windows_len,
        }
        (focus_index + direction).clamp(0, windows_len - 1) as usize
    }

    fn swap_current(&mut self, direction: i32) {
        if let Some(focus_index) = self.logged_focus_index() {
            let other_swap_index = self.compute_index_for_direction(focus_index, direction);
            self.windows.swap(focus_index, other_swap_index);
        }
    }

    pub fn focus_left(&self) {
        self.focus(-1);
    }

    pub fn focus_right(&self) {
        self.focus(1);
    }

    fn focus(&self, direction: i32) {
        if let Some(focus_index) = self.focus_index_with_fallback_and_log() {
            let new_focus_index = self.compute_index_for_direction(focus_index, direction);
            let window = self.windows[new_focus_index].inner;

            let _ = window
                .focus()
                .context(window.get_formatted_extensive_info())
                .context("Changing tiler focused window")
                .error()
                .log_err();
        }
    }

    pub fn set_current_window_fullscreen(&mut self) {
        if let Some(focus_index) = self.focus_index() {
            let width = self.max_screen_width();
            // -1 to avoid occupying the whole screen and causing scroll issues
            // TODO: find a better solution for this, the problem is that the scroll system
            // doesn't handle windows that are equals or bigger than the screen size well.
            // Fix for now: prevent windows width from being equal or bigger than screen size.
            self.windows[focus_index].request_width(width);
        }
    }

    pub fn set_current_window_halfscreen(&mut self) {
        if let Some(focus_index) = self.focus_index() {
            let screen_width = self.screen_size.width();
            self.windows[focus_index].request_width(self.padding.mul_add(-2.0, screen_width / 2.0));
        }
    }

    pub fn increment_current_window_width(&mut self) {
        self.resize_current_window_width_by_resize_increment(1);
    }

    pub fn decrement_current_window_width(&mut self) {
        self.resize_current_window_width_by_resize_increment(-1);
    }

    pub fn max_screen_width(&self) -> f32 {
        self.padding.mul_add(-2.0, self.screen_size.width()) - 1.0
    }

    /// Resize the current window width by the resize increment in the given direction.
    /// Direction should be 1 for increasing width and -1 for decreasing width.ze
    fn resize_current_window_width_by_resize_increment(&mut self, direction: i32) {
        if let Some(focus_index) = self.focus_index() {
            // TODO: check explanation in `set_current_window_fullscreen` about -1
            cast! {
                direction.signum() => f32 as direction,
            }
            let new_width = self
                .resize_increment
                .mul_add(direction, self.windows[focus_index].width)
                .clamp(
                    0.0,
                    self.padding.mul_add(-2.0, self.screen_size().width()) - 1.0,
                );
            self.windows[focus_index].request_width(new_width);
        }
    }

    pub fn focused_window(&self) -> Option<Window> {
        self.focus_index().map(|index| self.windows[index].inner)
    }

    /// One-shot startup seeding: tile every window in `snapshot`, ignoring
    /// the per-monitor add gate. Used by the initial consolidation pass so
    /// a fresh winri session always pulls everything onto the tiling
    /// monitor for the user to organise from.
    pub fn bulk_seed(&mut self, snapshot: &HashSet<Window>) {
        for window in snapshot {
            if !self.windows.iter().any(|item| item.inner == *window) {
                self.windows.push(WindowItem::new(*window, self.default_size()));
            }
        }
        let positions = self.windows_positions();
        self.layout_windows(&positions);
        log::info!("bulk_seed: {} window(s) now tiled", self.windows.len());
    }

    pub fn handle_window_snapshot(
        &mut self,
        windows_snapshot: &HashSet<Window>,
        tiling_hmonitor: isize,
    ) {
        // Drop tracked windows that no longer exist OR that the standard
        // tile-filter has rejected (e.g. user just added the app to the
        // ignore list). Crucially we DO NOT drop a window just because it
        // landed off the tiling monitor — that would falsely fire any time
        // a scroll pushed a window's center past the monitor edge. The
        // dedicated drag-end check below handles real "user moved this to
        // another monitor" events.
        self.windows
            .retain(|item| windows_snapshot.contains(&item.inner));

        // Drag-end detection. If the user was holding left mouse last
        // snapshot and isn't now, they just released. A tiled window
        // counts as "user-dragged" only if its actual on-screen position
        // has drifted from where we last placed it AND it's now on a
        // non-tiling monitor — otherwise we'd untile windows that simply
        // ended up on the secondary monitor because the strip overflows.
        let mouse_held_now = is_left_mouse_held();
        let drag_just_ended = self.was_mouse_held_last_snapshot && !mouse_held_now;
        self.was_mouse_held_last_snapshot = mouse_held_now;
        if drag_just_ended {
            /// How far a window must have drifted from its last laid-out
            /// position to count as "user dragged" rather than tiler-placed.
            const DRAG_DETECT_PX: f32 = 50.0;
            let before = self.windows.len();
            self.windows.retain(|item| {
                let on_tiling = item.inner.monitor() == tiling_hmonitor;
                if on_tiling {
                    return true;
                }
                let Some((lx, ly, _, _)) = item.last_layout else {
                    // No baseline → trust the existing behavior and keep
                    // the window. Will be reassessed next snapshot.
                    return true;
                };
                let drifted = match item.inner.desktop_manager_bounds().ok() {
                    Some(bounds) => {
                        let pos = bounds.position();
                        (pos.x() - lx).abs() > DRAG_DETECT_PX
                            || (pos.y() - ly).abs() > DRAG_DETECT_PX
                    }
                    None => false,
                };
                if drifted {
                    info!(
                        "Drag-end: window {:?} moved to monitor {} and drifted from layout; untiling",
                        item.inner.handle(),
                        item.inner.monitor(),
                    );
                    false
                } else {
                    true
                }
            });
            if before != self.windows.len() {
                info!(
                    "Drag-end pruned {} window(s); {} remain tiled",
                    before - self.windows.len(),
                    self.windows.len()
                );
            }
        }

        self.update_widths();

        self.append_new_windows(windows_snapshot, tiling_hmonitor);

        let windows_positions = self.windows_positions();

        // Only auto-scroll the viewport when focus actually changes — e.g.
        // Win+←/→, a click on a tile, a jump from overview. Adjusting on
        // every snapshot fights with user-initiated actions like manual
        // wheel scrolling and resize, which legitimately move windows
        // without expressing focus intent.
        let current_focus = self.focus_index();
        let focus_changed = current_focus != self.previously_focused_window_index;
        if focus_changed {
            let previous_scroll_offset = self.scroll_offset;
            self.ajust_scroll(&windows_positions);
            if (previous_scroll_offset - self.scroll_offset).abs() > 1.0 {
                debug!(
                    "Adjusted scroll on focus change: {} -> {}",
                    previous_scroll_offset, self.scroll_offset
                );
            }
        }
        self.layout_windows(&windows_positions);

        if let Some(new_focused_window_index) = self
            .focus_index()
            .filter(|i| Some(*i) != self.previously_focused_window_index)
        {
            self.previously_focused_window_index = Some(new_focused_window_index);
        }
    }

    /// Append new windows from the snapshot that are not already in the
    /// tiler. Gated by `tiling_hmonitor`: a window opening on a non-tiling
    /// monitor stays floating. If the focused window is tiled, new windows
    /// are appended after it; otherwise they are appended at the end.
    fn append_new_windows(
        &mut self,
        windows_snapshot: &HashSet<Window>,
        tiling_hmonitor: isize,
    ) {
        let is_addable = |w: &Window| -> bool {
            // Only auto-tile windows that are physically on the tiling
            // monitor. Otherwise leave them floating.
            w.monitor() == tiling_hmonitor
        };

        if !self.windows.is_empty()
            && let Some(focus_index) = self.focus_index().or(self.previously_focused_window_index)
            && focus_index < self.windows.len()
        {
            for window in windows_snapshot {
                if !self
                    .windows
                    .iter()
                    .any(|window_item| window_item.inner == *window)
                    && is_addable(window)
                {
                    log::info!("Auto-tile {:?} after focused {focus_index}", window.handle());
                    self.windows.insert(
                        focus_index + 1,
                        WindowItem::new(*window, self.default_size()),
                    );
                }
            }
        } else {
            for window in windows_snapshot {
                if !self
                    .windows
                    .iter()
                    .any(|window_item| window_item.inner == *window)
                    && is_addable(window)
                {
                    log::info!("Auto-tile {:?} at end", window.handle());
                    self.windows
                        .push(WindowItem::new(*window, self.default_size()));
                }
            }
        }
    }

    fn default_size(&self) -> f32 {
        // New tiled windows open at ~¼ of the screen width. Win+F (fullscreen)
        // and Win+C (half) are still available to grow them on demand.
        self.padding.mul_add(-2.0, self.screen_size.width() / 4.0)
    }

    fn layout_windows(&mut self, windows_positions: &[f32]) {
        // Don't fight the user. When the left mouse button is held the
        // user is most likely dragging a window — including possibly out
        // of the strip onto another monitor — and every `SetWindowPos`
        // we'd issue here would snap it back. We skip this whole pass and
        // pick up again on the snapshot after they release the button.
        if is_left_mouse_held() {
            return;
        }

        // A rect "actually changed" tolerance — DWM bounds round-trips can
        // jitter a fraction of a pixel; we don't want to count that as
        // motion and disturb the border fade.
        const MOTION_EPS: f32 = 0.5;

        // Tiling-monitor bounds in screen coords. We use these to clip each
        // window's rendered pixels so the strip never spills onto a
        // neighbouring monitor. The monitor is assumed to start at (0, 0)
        // — this matches `screen_size()` which returns the primary
        // monitor's work area.
        let monitor_width = self.screen_size.width();
        let monitor_height = self.screen_size.height();

        // Park coordinates for windows that are completely outside the
        // tiling monitor. Chosen to be well outside any realistic
        // virtual-screen layout.
        const PARK_X: f32 = 100_000.0;
        const PARK_Y: f32 = 100_000.0;

        let mut moved_any = false;
        for (window, x) in self.windows.iter_mut().zip(windows_positions) {
            let target_x = x - self.scroll_offset;
            let target_y = self.padding;
            let target_height = self.padding.mul_add(-2.0, self.screen_size.height());
            let target_width = window.width;

            // Screen-space intersection with the tiling monitor.
            let vis_left = target_x.max(0.0);
            let vis_top = target_y.max(0.0);
            let vis_right = (target_x + target_width).min(monitor_width);
            let vis_bottom = (target_y + target_height).min(monitor_height);
            let fully_offscreen = vis_right <= vis_left || vis_bottom <= vis_top;
            let fully_visible = target_x >= 0.0
                && target_y >= 0.0
                && target_x + target_width <= monitor_width
                && target_y + target_height <= monitor_height;

            // Decide where to physically place the window. Fully-off windows
            // get parked at far coords so they don't render on any monitor.
            let (place_x, place_y) = if fully_offscreen {
                (PARK_X, PARK_Y)
            } else {
                (target_x, target_y)
            };

            let changed = window
                .last_layout
                .map_or(true, |(px, py, pw, ph)| {
                    (px - place_x).abs() > MOTION_EPS
                        || (py - place_y).abs() > MOTION_EPS
                        || (pw - target_width).abs() > MOTION_EPS
                        || (ph - target_height).abs() > MOTION_EPS
                });

            if let Err(e) = window.inner.move_to(
                [place_x, place_y].into(),
                [target_width, target_height].into(),
            ) {
                warn!(
                    "Error while layouting window, skipping to next one (window might have been closed just after enumeration): {e}"
                );
                continue;
            }
            if changed {
                moved_any = true;
                window.last_layout = Some((place_x, place_y, target_width, target_height));
            }

            // Clip the rendered region so off-monitor pixels don't show up
            // on a neighbouring screen. State changes are logged so any
            // weirdness is easy to diagnose.
            if fully_offscreen {
                // Parked windows don't need a clip — they're already off
                // every monitor. Make sure any prior clip is removed.
                if window.last_clip.is_some() {
                    if let Err(e) = window.inner.clear_visible_region() {
                        warn!("clear_visible_region (park transition) failed: {e}");
                    }
                    log::debug!("clip: {:?} parked, cleared region", window.inner.handle());
                    window.last_clip = None;
                }
            } else if fully_visible {
                if window.last_clip.is_some() {
                    if let Err(e) = window.inner.clear_visible_region() {
                        warn!("clear_visible_region (fully-visible transition) failed: {e}");
                    }
                    log::debug!(
                        "clip: {:?} fully visible, cleared region",
                        window.inner.handle()
                    );
                    window.last_clip = None;
                }
            } else {
                // Partial overflow — clip to the visible window-local rect.
                #[allow(clippy::cast_possible_truncation)]
                let local_left = (vis_left - target_x) as i32;
                #[allow(clippy::cast_possible_truncation)]
                let local_top = (vis_top - target_y) as i32;
                #[allow(clippy::cast_possible_truncation)]
                let local_right = (vis_right - target_x) as i32;
                #[allow(clippy::cast_possible_truncation)]
                let local_bottom = (vis_bottom - target_y) as i32;
                let next_clip = (local_left, local_top, local_right, local_bottom);

                if window.last_clip != Some(next_clip) {
                    if let Err(e) = window.inner.set_visible_region(
                        local_left,
                        local_top,
                        local_right,
                        local_bottom,
                    ) {
                        warn!("set_visible_region failed: {e}");
                    } else {
                        log::debug!(
                            "clip: {:?} partial -> ({local_left}, {local_top}, {local_right}, {local_bottom})",
                            window.inner.handle()
                        );
                        window.last_clip = Some(next_clip);
                    }
                }
            }
        }
        if moved_any {
            self.mark_motion();
        }
    }

    /// Width
    fn update_widths(&mut self) {
        let max_screen_width = self.max_screen_width();
        for window in &mut self.windows {
            if let Some(requested_width) = window.requested_width() {
                window.width = requested_width;
            } else if let Ok(bounds) = window
                .inner
                .desktop_manager_bounds()
                .context("Updating widths")
                .error()
                .log_err()
            {
                window.width = bounds.size().width().min(max_screen_width);
            }
        }
    }

    fn ajust_scroll(&mut self, windows_positions: &[f32]) {
        if let Some((index, focused_window)) = self
            .windows
            .iter()
            .enumerate()
            .find(|(_, window_item)| window_item.inner.is_focused().unwrap_or(false))
        {
            // When an animation is already in flight, evaluate visibility
            // against where we're heading, not the in-between position —
            // otherwise we keep chasing our own tail every frame.
            let reference_offset = self.scroll_target.unwrap_or(self.scroll_offset);
            let focused_window_left = windows_positions[index] - self.padding - reference_offset;
            let focused_window_right = self
                .padding
                .mul_add(2.0, focused_window_left + focused_window.width);

            if focused_window_left >= 0.0 && focused_window_right <= self.screen_size.width() {
                return;
            }

            let window_left_to_screen_left = focused_window_left.abs();
            let window_right_to_screen_right =
                focused_window_right.sub(self.screen_size.width()).abs();

            let new_offset = if window_left_to_screen_left < window_right_to_screen_right {
                reference_offset - window_left_to_screen_left
            } else {
                reference_offset + window_right_to_screen_right
            };

            if self.smooth_scroll_enabled {
                self.scroll_target = Some(new_offset);
            } else {
                self.scroll_offset = new_offset;
            }
        }
    }

    pub fn windows_positions(&self) -> Vec<f32> {
        let mut positions = Vec::new();
        let mut current_position = 0.0;

        for window in &self.windows {
            current_position += self.padding;
            positions.push(current_position);
            current_position += window.width + self.padding;
        }

        positions
    }

    pub const fn screen_size(&self) -> Size {
        self.screen_size
    }
}
