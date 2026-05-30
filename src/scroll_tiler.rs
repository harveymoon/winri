use std::{
    collections::{HashMap, HashSet},
    ops::Sub,
    time::{Duration, Instant},
};

use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON};

use anyhow::Context;
use joy_error::log::ResultLogExt;
use log::{debug, info, warn};

use crate::{cast, utils::math::Size, window::Window};

/// In-flight smooth width transition. Driven by `ScrollTiler::tick_animation`
/// in the same 16ms loop that scroll smoothing uses. Cleared when elapsed
/// exceeds duration; `target_width` is then committed exactly.
#[derive(Debug, Clone, PartialEq)]
pub struct WidthAnimation {
    pub start_width: f32,
    pub target_width: f32,
    pub start_time: Instant,
    pub duration: Duration,
}

/// Time-based scroll animation set by the API. Same shape as
/// [`WidthAnimation`]; ticked in the same `tick_animation` pass with the
/// same ease-out-cubic so composed resize+scroll calls (e.g. "fly the
/// viewport to this thumbnail") feel coherent.
#[derive(Debug, Clone, PartialEq)]
pub struct ScrollAnimation {
    pub start_offset: f32,
    pub target_offset: f32,
    pub start_time: Instant,
    pub duration: Duration,
}

/// Per-scroll-animation frame throttle for known-slow processes.
/// Value `N` means "emit `SetWindowPos` to this process every `N`-th
/// animation frame" — at 60Hz a throttle of 3 means 20 moves/sec
/// instead of 60. We always issue a final `SetWindowPos` at scroll-end
/// regardless of throttle so windows land at the correct rest
/// position. Populate this table from the `ScrollProfile` diagnostic
/// logs after a few scrolls.
const SLOW_PROCESS_FRAME_THROTTLE: &[(&str, u32)] = &[
    // File Explorer's WPF/XAML UI thread can't keep up with
    // 60 SetWindowPos/sec — per-scroll diagnostic logs showed
    // 200–2000+px lag at scroll-end during fast scrolls, and
    // 1–2px lag during slow scrolls. Cutting to ~20fps lets
    // the WM_SIZE / repaint pipeline drain in time, and the
    // user sees smooth in-sync motion instead of catch-up
    // slide. Other apps (Chrome, Claude, Discord, Photon Fleet,
    // TouchDesigner) measured at 0–2px lag and don't need it.
    ("explorer.exe", 3),
];

fn frame_throttle_for(process_name: &str) -> u32 {
    for (name, throttle) in SLOW_PROCESS_FRAME_THROTTLE {
        if process_name.eq_ignore_ascii_case(name) {
            return *throttle;
        }
    }
    1
}

/// Diagnostic counter for `SetWindowPos` activity during a scroll
/// animation. Tracks per-HWND emit counts, total scroll duration, and
/// total animation frames; logged as a single summary at scroll-end so
/// we can tell which apps got how many position updates and how much
/// they lag the rest of the strip when the scroll settles.
#[derive(Default)]
struct ScrollProfile {
    start: Option<Instant>,
    frame_count: u32,
    counts: HashMap<u64, u32>,
}

struct ScrollProfileSnapshot {
    duration: Duration,
    frame_count: u32,
    /// Sorted descending by emit count.
    counts: Vec<(u64, u32)>,
}

impl ScrollProfile {
    fn start(&mut self) {
        self.start = Some(Instant::now());
        self.frame_count = 0;
        self.counts.clear();
    }
    fn tick_frame(&mut self) {
        if self.start.is_some() {
            self.frame_count = self.frame_count.saturating_add(1);
        }
    }
    fn record(&mut self, hwnd: u64) {
        if self.start.is_some() {
            *self.counts.entry(hwnd).or_insert(0) += 1;
        }
    }
    fn finish(&mut self) -> Option<ScrollProfileSnapshot> {
        let start = self.start.take()?;
        let counts = std::mem::take(&mut self.counts);
        let mut sorted: Vec<(u64, u32)> = counts.into_iter().collect();
        sorted.sort_unstable_by(|a, b| b.1.cmp(&a.1));
        Some(ScrollProfileSnapshot {
            duration: start.elapsed(),
            frame_count: self.frame_count,
            counts: sorted,
        })
    }
}

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
    /// Active smooth-resize animation, if any. Set by
    /// `ScrollTiler::animate_window_width` (driven from the HTTP API's
    /// `POST /windows/<id>/resize`). Advanced each frame by `tick_animation`.
    width_animation: Option<WidthAnimation>,
    /// Actual on-screen position observed at the previous snapshot tick.
    /// Used by the position-divergence defense to distinguish "window
    /// just moved (and is now divergent from our cache)" from "window
    /// has been stuck in its divergent position since last tick" — we
    /// only invalidate `last_layout` on the transition, never in a busy
    /// loop, so an app that persistently rejects our SetWindowPos can't
    /// keep us calling SetWindowPos every snapshot.
    last_observed_pos: Option<(f32, f32)>,
    /// Resolved once at construction from `Window::is_clip_unsafe()`.
    /// When `true`, the clip pass skips `SetWindowRgn` for this window
    /// entirely (Chromium-based windows lose their swap chain on rapid
    /// region changes — see `Window::is_clip_unsafe` for the rationale).
    /// Cached on the item so we don't re-classify the window on every
    /// layout tick.
    skip_clipping: bool,
    /// `is_iconic()` result captured at the end of the previous snapshot
    /// tick. Used to detect the minimize/restore transition so we can
    /// snapshot the tile width before minimize and re-assert it on
    /// restore. Without this, some apps (Chromium-class especially)
    /// restore at a sliver-sized rect; we'd cache that and never
    /// recover the original tile width.
    was_iconic_last_tick: bool,
    /// Width the tile had immediately before the window went iconic.
    /// Cleared as soon as it's been re-applied on restore.
    pre_minimize_width: Option<f32>,
    /// Per-scroll-animation frame throttle, resolved once at
    /// construction from `frame_throttle_for(process_name)`. Default 1
    /// means "emit every frame"; values >1 cause the layout pass to
    /// skip this window on `(scroll_frame_index % throttle) != 0`
    /// during animations. The final settle frame always emits.
    scroll_frame_throttle: u32,
}

impl WindowItem {
    pub fn new(inner: Window, width: f32) -> Self {
        let skip_clipping = inner.is_clip_unsafe().unwrap_or(false);
        let scroll_frame_throttle = inner
            .process_name()
            .ok()
            .as_deref()
            .map_or(1, frame_throttle_for);
        Self {
            inner,
            requested_width: Some(width),
            width,
            last_layout: None,
            last_clip: None,
            width_animation: None,
            last_observed_pos: None,
            skip_clipping,
            was_iconic_last_tick: false,
            pre_minimize_width: None,
            scroll_frame_throttle,
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
    /// When `Some`, exponential smoothing is active: the tiler is
    /// animating `scroll_offset` toward this value at
    /// `smooth_scroll_factor` per frame. Set by user-driven scroll
    /// (Win+wheel, focus shifts) while [`smooth_scroll_enabled`] is
    /// `true`. Cleared once the animation settles. Mutually exclusive
    /// with [`scroll_animation`] — starting one clears the other.
    scroll_target: Option<f32>,
    /// Time-based scroll animation driven by the HTTP API's
    /// `/scroll {animate_ms: N}`. Independent of the exponential
    /// smoothing path so callers (e.g. a tablet scrub bar streaming
    /// pointermove targets) can fire-and-forget at a known duration
    /// rather than rely on the config-tunable smoothing factor. Mutually
    /// exclusive with [`scroll_target`].
    scroll_animation: Option<ScrollAnimation>,
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
    /// Animation-frame counter; resets to 0 at scroll-start, increments
    /// once per `layout_windows` call while `is_animating()`. Used to
    /// implement per-process scroll-frame throttling.
    scroll_frame_index: u32,
    /// `is_animating()` result from the previous `layout_windows` call,
    /// used to detect scroll-start (false→true) and scroll-end
    /// (true→false) edges for the profile + throttle logic.
    was_animating_last_layout: bool,
    /// Diagnostic counter; see `ScrollProfile`.
    scroll_profile: ScrollProfile,
}

impl ScrollTiler {
    pub fn new(padding: f32, resize_increment: f32, screen_size: Size) -> Self {
        Self {
            padding,
            resize_increment,
            screen_size,
            smooth_scroll_factor: 0.25,
            // Seed from the actual key state so a mouse that's already
            // held when winri starts doesn't trigger a spurious drag-end
            // on the very first snapshot.
            was_mouse_held_last_snapshot: is_left_mouse_held(),
            ..Default::default()
        }
    }

    /// Configure smoothing — typically driven from `config.toml`.
    pub fn set_smoothing(&mut self, enabled: bool, factor: f32) {
        self.smooth_scroll_enabled = enabled;
        self.smooth_scroll_factor = factor.clamp(0.05, 1.0);
    }

    /// Whether any smoothing animation is currently in progress —
    /// exponential scroll smoothing, time-based scroll animation, or any
    /// per-window width animation. Drives the 16ms iced redraw
    /// subscription so `tick_animation` keeps firing until everything
    /// settles.
    pub fn is_animating(&self) -> bool {
        self.scroll_target.is_some()
            || self.scroll_animation.is_some()
            || self.windows.iter().any(|w| w.width_animation.is_some())
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

    /// Advance every in-flight animation (scroll smoothing + per-window
    /// width animations) by one frame. Returns whether any animation is
    /// still running so the caller can decide whether to keep ticking.
    pub fn tick_animation(&mut self) -> bool {
        let mut moved = false;

        // Time-based scroll animation (API-driven, ease-out-cubic over
        // the requested duration). Mutually exclusive with `scroll_target`
        // — `animate_scroll_to` clears scroll_target when set.
        if let Some(anim) = self.scroll_animation.clone() {
            let elapsed = Instant::now().saturating_duration_since(anim.start_time);
            if elapsed >= anim.duration {
                self.scroll_offset = anim.target_offset;
                self.scroll_animation = None;
            } else {
                let dur = anim.duration.as_secs_f32().max(1e-3);
                let t = (elapsed.as_secs_f32() / dur).clamp(0.0, 1.0);
                let eased = 1.0 - (1.0 - t).powi(3);
                self.scroll_offset =
                    anim.start_offset + (anim.target_offset - anim.start_offset) * eased;
            }
            moved = true;
        } else if let Some(target) = self.scroll_target {
            // Exponential scroll smoothing (user-driven; mirrors config).
            let diff = target - self.scroll_offset;
            if diff.abs() < 0.5 {
                self.scroll_offset = target;
                self.scroll_target = None;
            } else {
                self.scroll_offset += diff * self.smooth_scroll_factor;
            }
            moved = true;
        }

        // Per-window width animations. Time-based (Instant) so they finish
        // in the requested wall-clock duration regardless of frame jitter.
        let now = Instant::now();
        for item in &mut self.windows {
            let Some(anim) = item.width_animation.clone() else {
                continue;
            };
            let elapsed = now.saturating_duration_since(anim.start_time);
            if elapsed >= anim.duration {
                // Snap to target and clear.
                item.width = anim.target_width;
                item.requested_width = Some(anim.target_width);
                item.width_animation = None;
            } else {
                let dur = anim.duration.as_secs_f32().max(1e-3);
                let t = (elapsed.as_secs_f32() / dur).clamp(0.0, 1.0);
                // Ease-out-cubic: quick start, soft landing.
                let eased = 1.0 - (1.0 - t).powi(3);
                let new_width = anim.start_width + (anim.target_width - anim.start_width) * eased;
                item.width = new_width;
                item.requested_width = Some(new_width);
            }
            moved = true;
        }

        if moved {
            let positions = self.windows_positions();
            self.layout_windows(&positions);
            self.mark_motion();
        }

        self.is_animating()
    }

    /// Begin a time-based smooth scroll to `target_offset` over
    /// `duration_ms`. Used by the HTTP API's
    /// `POST /scroll {offset|delta, animate_ms}`. Calling again while
    /// another scroll animation is in flight **replaces** the in-flight
    /// target — there's no queue. `duration_ms == 0` snaps immediately
    /// and cancels any in-flight animation. Clears `scroll_target` so
    /// the exponential smoothing path doesn't compete with the
    /// time-based one.
    pub fn animate_scroll_to(&mut self, target_offset: f32, duration_ms: u32) {
        self.scroll_target = None;
        if duration_ms == 0 {
            self.scroll_offset = target_offset;
            self.scroll_animation = None;
            let positions = self.windows_positions();
            self.layout_windows(&positions);
            self.mark_motion();
        } else {
            self.scroll_animation = Some(ScrollAnimation {
                start_offset: self.scroll_offset,
                target_offset,
                start_time: Instant::now(),
                duration: Duration::from_millis(u64::from(duration_ms)),
            });
            self.mark_motion();
        }
    }

    /// Begin a smooth scroll so the window with the given HWND ends up
    /// centered in the viewport at `assumed_width`. Use `assumed_width =
    /// target_width` when chaining with `animate_window_width` so the
    /// final centered position is correct even though the live width is
    /// still mid-interpolation. Returns `false` if the HWND isn't tracked.
    pub fn center_window_at_width(&mut self, hwnd_raw: u64, assumed_width: f32) -> bool {
        let Some(idx) = self
            .windows
            .iter()
            .position(|w| w.inner.handle().0 as u64 == hwnd_raw)
        else {
            return false;
        };
        let positions = self.windows_positions();
        let Some(&strip_x) = positions.get(idx) else {
            return false;
        };
        let max_w = self.max_screen_width();
        let clamped = assumed_width.clamp(50.0, max_w);
        let target = strip_x + clamped / 2.0 - self.screen_size.width() / 2.0;
        self.set_scroll_offset(target);
        true
    }

    /// Begin a smooth width animation for the window with the given HWND.
    /// `duration_ms == 0` applies the new width immediately and cancels
    /// any in-flight animation for that window. The target is clamped to
    /// `[50, max_screen_width]` so callers can't accidentally hide or
    /// over-grow a tile. Returns `false` if no tiled window with that
    /// HWND exists.
    pub fn animate_window_width(
        &mut self,
        hwnd_raw: u64,
        target_width: f32,
        duration_ms: u32,
    ) -> bool {
        let max_w = self.max_screen_width();
        let target = target_width.clamp(50.0, max_w);
        let Some(item) = self
            .windows
            .iter_mut()
            .find(|w| w.inner.handle().0 as u64 == hwnd_raw)
        else {
            return false;
        };
        if duration_ms == 0 {
            item.width = target;
            item.requested_width = Some(target);
            item.width_animation = None;
        } else {
            item.width_animation = Some(WidthAnimation {
                start_width: item.width,
                target_width: target,
                start_time: Instant::now(),
                duration: Duration::from_millis(u64::from(duration_ms)),
            });
        }
        true
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

    /// Forget every tracked window's last laid-out rect. Call after some
    /// code path outside `layout_windows` has moved the windows (e.g.
    /// overview's `move_offscreen` parks every tile when the user opens
    /// overview). Without this, the next `layout_windows` pass sees
    /// `last_layout` matches the new target and short-circuits the move
    /// — leaving the windows wherever the external code last put them.
    pub fn invalidate_last_layouts(&mut self) {
        for w in &mut self.windows {
            w.last_layout = None;
        }
    }

    /// Scroll the tile strip horizontally by `delta_px`. Positive values
    /// shift the viewport rightward (windows slide left). When smoothing is
    /// enabled, the target accumulates and the next animation tick moves
    /// toward it; otherwise the scroll snaps.
    pub fn scroll_by(&mut self, delta_px: f32) {
        // User-driven scroll overrides any in-flight time-based API
        // animation — last-input-wins.
        self.scroll_animation = None;
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
        self.scroll_animation = None;
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

    /// Walk `direction` (-1 or +1) from `start` skipping any iconic or
    /// cloaked windows. Used for focus stepping so Win+←/→ never lands on
    /// a window the user can't actually see — landing on a cloaked window
    /// causes the OS to switch virtual desktops on `SetForegroundWindow`,
    /// which the user explicitly does *not* want when cycling focus on
    /// the current desktop's strip. Returns `None` if no visible neighbor
    /// exists in that direction (in which case caller should no-op).
    fn next_visible_index(&self, start: usize, direction: i32) -> Option<usize> {
        if self.windows.is_empty() {
            return None;
        }
        let len = self.windows.len() as i32;
        let mut idx = start as i32 + direction;
        while idx >= 0 && idx < len {
            let item = &self.windows[idx as usize];
            if !item.inner.is_iconic() && !item.inner.is_cloaked().unwrap_or(false) {
                return Some(idx as usize);
            }
            idx += direction;
        }
        None
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
            let Some(new_focus_index) = self.next_visible_index(focus_index, direction) else {
                // Nothing visible in that direction — no-op rather than
                // wrap or land on something the user can't see.
                return;
            };
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
        //
        // Virtual-desktop survival: a window that cloaked because the
        // user switched to another Windows virtual desktop (Win+Ctrl+→,
        // 3-finger touchpad swipe, Task View click-through) drops out of
        // the snapshot. Without this we'd lose every tiled window on a
        // desktop switch and re-append them in arbitrary order with
        // default widths when the user came back. Keep cloaked-but-valid
        // HWNDs in the tiler so positions, widths, and order all survive
        // the round trip.
        self.windows.retain(|item| {
            if windows_snapshot.contains(&item.inner) {
                return true;
            }
            // Still a real HWND and only "missing" because cloaked? Keep.
            item.inner.is_valid().unwrap_or(false)
                && item.inner.is_cloaked().unwrap_or(false)
        });

        // Iconic transition: snapshot the tile width on entry to iconic,
        // re-assert it on exit. Restored windows (especially Chromium-
        // based) often come back at a sliver-sized rect that the next
        // tick's update_widths could otherwise pick up; pinning
        // `requested_width` here forces the layout pass to re-issue
        // SetWindowPos at the original tile width before the user sees
        // the sliver. `last_layout = None` defeats the diff-skip so the
        // SetWindowPos actually fires even if the cache says we're
        // already there.
        for item in &mut self.windows {
            let now_iconic = item.inner.is_iconic();
            if !item.was_iconic_last_tick && now_iconic {
                // Just minimized — capture intent.
                item.pre_minimize_width = Some(item.width);
            } else if item.was_iconic_last_tick && !now_iconic {
                // Just restored — re-apply intent.
                if let Some(w) = item.pre_minimize_width.take() {
                    item.requested_width = Some(w);
                    item.last_layout = None;
                }
            }
            item.was_iconic_last_tick = now_iconic;
        }

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
            /// A human can only drag one window per mouse-release. If many
            /// windows simultaneously look "off-monitor + drifted" the
            /// layout system is desynced from reality (e.g. SetWindowPos
            /// was silently failing all session) — refuse to prune and let
            /// the next snapshot re-stabilise. Empirically chosen: 2
            /// allows a small amount of noise but rejects the runaway case
            /// that flattened the tiler from 14→0 in one click.
            const DRAG_PRUNE_CAP: usize = 2;

            // First pass: collect candidates without mutating self.windows.
            let candidates: Vec<usize> = self
                .windows
                .iter()
                .enumerate()
                .filter_map(|(idx, item)| {
                    if item.inner.monitor() == tiling_hmonitor {
                        return None;
                    }
                    // Cloaked windows are on another virtual desktop —
                    // their position is meaningless to the drag-end check.
                    if item.inner.is_cloaked().unwrap_or(false) {
                        return None;
                    }
                    let (lx, ly, _, _) = item.last_layout?;
                    let bounds = item.inner.desktop_manager_bounds().ok()?;
                    let pos = bounds.position();
                    let drifted = (pos.x() - lx).abs() > DRAG_DETECT_PX
                        || (pos.y() - ly).abs() > DRAG_DETECT_PX;
                    drifted.then_some(idx)
                })
                .collect();

            if candidates.len() > DRAG_PRUNE_CAP {
                warn!(
                    "Drag-end: {} window(s) appear off-tiling-monitor + drifted (cap {}). \
                     Treating as layout desync, not user drag; no windows untiled. \
                     HWNDs: {:?}",
                    candidates.len(),
                    DRAG_PRUNE_CAP,
                    candidates
                        .iter()
                        .map(|&i| self.windows[i].inner.handle())
                        .collect::<Vec<_>>(),
                );
            } else if !candidates.is_empty() {
                for &idx in &candidates {
                    let item = &self.windows[idx];
                    info!(
                        "Drag-end: window {:?} moved to monitor {} and drifted from layout; untiling",
                        item.inner.handle(),
                        item.inner.monitor(),
                    );
                }
                // Remove highest-indexed first so earlier indices stay valid.
                let mut to_remove = candidates;
                to_remove.sort_unstable_by(|a, b| b.cmp(a));
                let before = self.windows.len();
                for idx in to_remove {
                    self.windows.remove(idx);
                }
                info!(
                    "Drag-end pruned {} window(s); {} remain tiled",
                    before - self.windows.len(),
                    self.windows.len()
                );
            }

            // (Left-edge resize detection follows.)
            //
            // Below this block (still inside the `drag_just_ended` arm) we'll
            // also pick up width-only resizes by the right-edge handler — but
            // that one doesn't need scroll comp, so it's just the existing
            // update_widths + layout_windows flow.

            // Left-edge resize detection. The strip is left-anchored, so if
            // the user dragged the left edge of the focused window we'd
            // normally re-pin its left edge and let the right edge fly off.
            // The user wants the right edge to stay visually fixed instead.
            // Pattern: width changed AND `actual.x` shifted by approximately
            // `-dw` (i.e. right edge stayed put). Compensate by nudging
            // scroll_offset by `dw` so layout_windows re-places the window
            // at the exact bounds the user left it at — zero visible jump.
            // Neighbors are NEVER squished or expanded by user choice.
            if let Some(focus_idx) = self.focus_index() {
                if let Some((lx, _ly, lw, _lh)) = self.windows[focus_idx].last_layout {
                    if let Ok(bounds) =
                        self.windows[focus_idx].inner.desktop_manager_bounds()
                    {
                        let pw = bounds.size().width();
                        let px = bounds.position().x();
                        let dw = pw - lw;
                        let dx = px - lx;
                        // Right edge stays put iff dx ≈ -dw. Noise floor
                        // catches sub-pixel/DWM jitter without burning on
                        // borderline gestures.
                        const RESIZE_NOISE_PX: f32 = 5.0;
                        let is_left_edge_resize =
                            dw.abs() > RESIZE_NOISE_PX && (dx + dw).abs() < RESIZE_NOISE_PX;
                        if is_left_edge_resize {
                            /// Smallest tile width allowed. A user dragging
                            /// past this floor snaps to it; the right edge
                            /// still doesn't move (scroll compensates by
                            /// the clamped delta, not the raw drag delta).
                            const MIN_TILE_WIDTH: f32 = 200.0;
                            let max_w = self.max_screen_width();
                            let clamped = pw.clamp(MIN_TILE_WIDTH, max_w);
                            let effective_dw = clamped - lw;
                            // Direct (not smooth) scroll so the right edge
                            // doesn't visibly drift while a smoothing
                            // animation chases the target.
                            self.scroll_offset += effective_dw;
                            self.scroll_target = None;
                            self.windows[focus_idx].requested_width = Some(clamped);
                            info!(
                                "Left-edge resize: focused HWND {:?} width {:.0} -> {:.0} (drag-actual {:.0}), scroll compensated {:+.0}",
                                self.windows[focus_idx].inner.handle(),
                                lw,
                                clamped,
                                pw,
                                effective_dw,
                            );
                        }
                    }
                }
            }
        }

        // Position-divergence defense.
        //
        // The tiler's `last_layout` cache trusts that the OS applied our
        // SetWindowPos calls. Multiple bugs this session were the same
        // shape: the OS silently rejected or clamped a move (Chrome's
        // i16 coordinate clamp, app-side WM_WINDOWPOSCHANGING handlers,
        // user dragging a tile slightly within the strip), our cache
        // said "already at target" and the diff-skip in layout_windows
        // never re-issued the move.
        //
        // Catch every flavour of this: on every snapshot tick where the
        // mouse isn't held AND no animation is running, compare each
        // non-iconic non-cloaked window's actual screen position against
        // `last_layout`. If they differ by more than the noise floor AND
        // the window has *moved since last tick*, invalidate
        // `last_layout` so the next pass actually re-issues the
        // SetWindowPos.
        //
        // Skipping during animations matters: an animation tick at 60Hz
        // updates `last_layout` to interpolated positions while Chrome
        // (et al.) is still applying the previous frame's SetWindowPos.
        // A snapshot landing mid-animation would see the lag, flag it as
        // divergence, and pile a redundant SetWindowPos onto a target
        // window's already-busy message queue. Compositors that fall
        // behind on this can stop producing frames entirely (Chrome
        // blank-window symptom). The divergence check is for restoring
        // *settled* state, not chasing in-flight motion.
        //
        // The "moved since last tick" gate is critical: an app that
        // persistently rejects our move (stuck divergent state) won't
        // trigger a busy-loop of SetWindowPos every tick. We only act
        // on the transition.
        if !mouse_held_now && !self.is_animating() {
            const DIVERGENCE_PX: f32 = 5.0;
            const MOVEMENT_NOISE_PX: f32 = 1.0;
            for item in &mut self.windows {
                if item.inner.is_iconic() || item.inner.is_cloaked().unwrap_or(false) {
                    // Don't track observed positions for hidden windows —
                    // their last_observed_pos is meaningless and could
                    // trigger a spurious "moved" detection on un-cloak.
                    item.last_observed_pos = None;
                    continue;
                }
                let Ok(bounds) = item.inner.desktop_manager_bounds() else {
                    continue;
                };
                let pos = bounds.position();
                let now_pos = (pos.x(), pos.y());
                let last_obs = item.last_observed_pos;
                item.last_observed_pos = Some(now_pos);

                let moved_since_last = match last_obs {
                    Some((ox, oy)) => {
                        (now_pos.0 - ox).abs() > MOVEMENT_NOISE_PX
                            || (now_pos.1 - oy).abs() > MOVEMENT_NOISE_PX
                    }
                    None => false,
                };
                if !moved_since_last {
                    continue;
                }

                let Some((lx, ly, _lw, _lh)) = item.last_layout else {
                    continue;
                };
                let diverged = (now_pos.0 - lx).abs() > DIVERGENCE_PX
                    || (now_pos.1 - ly).abs() > DIVERGENCE_PX;
                if diverged {
                    debug!(
                        "Position divergence on {:?}: actual=({:.0},{:.0}) cache=({:.0},{:.0}) — forcing re-issue",
                        item.inner.handle(),
                        now_pos.0,
                        now_pos.1,
                        lx,
                        ly,
                    );
                    item.last_layout = None;
                }
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
            // When the strip has been fundamentally repacked (e.g. user
            // just switched virtual desktops and a different subset of
            // windows is now non-cloaked), the new focused window is
            // typically far outside the viewport the old scroll_offset
            // was set for. `adjust_scroll` would nudge incrementally and
            // potentially land on empty desktop. Detect "focused window
            // is more than half a viewport from any visible edge" and
            // center on it instead.
            let should_center = current_focus.is_some_and(|idx| {
                if idx >= windows_positions.len() {
                    return false;
                }
                let pos = windows_positions[idx];
                let w = self.windows[idx].width;
                let view_left = self.scroll_offset;
                let view_right = self.scroll_offset + self.screen_size.width();
                let half_screen = self.screen_size.width() / 2.0;
                pos > view_right + half_screen || (pos + w) < view_left - half_screen
            });
            if should_center {
                if let Some(idx) = current_focus {
                    let pos = windows_positions[idx];
                    let w = self.windows[idx].width;
                    let target = pos + w / 2.0 - self.screen_size.width() / 2.0;
                    self.set_scroll_offset(target);
                    debug!(
                        "Recentered scroll on focus change (post-repack): {} -> {}",
                        previous_scroll_offset, self.scroll_offset
                    );
                }
            } else {
                self.ajust_scroll(&windows_positions);
                if (previous_scroll_offset - self.scroll_offset).abs() > 1.0 {
                    debug!(
                        "Adjusted scroll on focus change: {} -> {}",
                        previous_scroll_offset, self.scroll_offset
                    );
                }
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

        // Animation transition detection. `is_animating()` covers
        // exponential scroll smoothing, time-based scroll animations,
        // and per-window width animations — anything that should be
        // throttled / profiled as a single scroll event.
        let animating_now = self.is_animating();
        let scroll_just_started = !self.was_animating_last_layout && animating_now;
        let scroll_just_ended = self.was_animating_last_layout && !animating_now;
        if scroll_just_started {
            self.scroll_frame_index = 0;
            self.scroll_profile.start();
            // Re-resolve throttle classification for any window still
            // marked as "every frame". `process_name()` can transiently
            // fail at item construction (just-spawned process not yet
            // queryable, integrity-level race) and lock us into
            // throttle=1 forever. Re-checking here is cheap (one Win32
            // call per previously-unclassified window per scroll) and
            // self-heals on the next scroll after the process settles.
            for item in &mut self.windows {
                if item.scroll_frame_throttle == 1 {
                    if let Ok(name) = item.inner.process_name() {
                        let t = frame_throttle_for(&name);
                        if t > 1 {
                            item.scroll_frame_throttle = t;
                        }
                    }
                }
            }
        }
        if animating_now {
            self.scroll_frame_index = self.scroll_frame_index.saturating_add(1);
            self.scroll_profile.tick_frame();
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
        // tiling monitor. Chosen to be:
        //   - Well outside any realistic virtual-screen layout (≥ 25k px
        //     comfortably exceeds 4× 8K monitors side-by-side),
        //   - **Inside `i16::MAX` (32767)** because Chromium-based apps
        //     (Chrome, Discord, VS Code, Electron, etc.) clamp SetWindowPos
        //     coordinates to signed-16-bit via WM_WINDOWPOSCHANGING — even
        //     when we set SWP_NOSENDCHANGING the app's internal hooks
        //     still rebound the move. A park value > 32767 silently lands
        //     the window at (32767, 32767) while our `last_layout` cache
        //     records the intended (100000, 100000); the window is then
        //     stranded because the cache reports "no change needed" on
        //     every subsequent layout pass.
        const PARK_X: f32 = 25_000.0;
        const PARK_Y: f32 = 25_000.0;

        // Per-frame decision for each window: what rect to move it to,
        // whether to apply a clip, and what clip rect. We compute these
        // first, then issue a *single* batched `BeginDeferWindowPos` to
        // move them all in one DWM composition pass, and finally apply
        // clip changes (only when not animating — clipping is expensive
        // and we'd rather catch up at settle than fight DWM 60×/sec).
        struct Decision {
            idx: usize,
            place_x: f32,
            place_y: f32,
            target_w: f32,
            target_h: f32,
            // None = fully visible (no clip needed)
            // Some((l,t,r,b)) = partial overflow, set clip
            // The "fully offscreen" case is encoded by parking + None
            // (no clip needed since the window is parked at PARK_X/Y).
            clip: Option<(i32, i32, i32, i32)>,
            fully_offscreen: bool,
        }

        let animating = self.scroll_target.is_some();
        let mut decisions: Vec<Decision> = Vec::with_capacity(self.windows.len());

        for (idx, (window, x)) in self.windows.iter().zip(windows_positions).enumerate() {
            if window.inner.is_iconic() {
                continue;
            }
            // Cloaked = on another virtual desktop. Skip layout entirely
            // — moving an invisible window wastes a SetWindowPos call and
            // can race with the shell's own cloak-driven repositioning.
            if window.inner.is_cloaked().unwrap_or(false) {
                continue;
            }
            let target_x = x - self.scroll_offset;
            let target_y = self.padding;
            let target_height = self.padding.mul_add(-2.0, self.screen_size.height());
            let target_width = window.width;

            let vis_left = target_x.max(0.0);
            let vis_top = target_y.max(0.0);
            let vis_right = (target_x + target_width).min(monitor_width);
            let vis_bottom = (target_y + target_height).min(monitor_height);
            let fully_offscreen = vis_right <= vis_left || vis_bottom <= vis_top;
            let fully_visible = target_x >= 0.0
                && target_y >= 0.0
                && target_x + target_width <= monitor_width
                && target_y + target_height <= monitor_height;

            let (place_x, place_y) = if fully_offscreen {
                (PARK_X, PARK_Y)
            } else {
                (target_x, target_y)
            };

            let clip = if fully_offscreen || fully_visible {
                None
            } else {
                #[allow(clippy::cast_possible_truncation)]
                let l = (vis_left - target_x) as i32;
                #[allow(clippy::cast_possible_truncation)]
                let t = (vis_top - target_y) as i32;
                #[allow(clippy::cast_possible_truncation)]
                let r = (vis_right - target_x) as i32;
                #[allow(clippy::cast_possible_truncation)]
                let b = (vis_bottom - target_y) as i32;
                Some((l, t, r, b))
            };

            decisions.push(Decision {
                idx,
                place_x,
                place_y,
                target_w: target_width,
                target_h: target_height,
                clip,
                fully_offscreen,
            });
        }

        // Build the batch — skip windows that haven't drifted past
        // `MOTION_EPS` from their last laid-out rect.
        let mut batch: Vec<crate::window::BatchMove> = Vec::with_capacity(decisions.len());
        let mut moved_any = false;
        for d in &decisions {
            let item = &self.windows[d.idx];
            let changed = item.last_layout.map_or(true, |(px, py, pw, ph)| {
                (px - d.place_x).abs() > MOTION_EPS
                    || (py - d.place_y).abs() > MOTION_EPS
                    || (pw - d.target_w).abs() > MOTION_EPS
                    || (ph - d.target_h).abs() > MOTION_EPS
            });
            if !changed {
                continue;
            }
            // Per-process scroll-frame throttle. While scrolling,
            // known-slow processes get SetWindowPos every Nth frame
            // instead of every frame so their UI thread can keep up
            // with the repaint cost. At rest (!animating_now,
            // including the scroll_just_ended frame), emit
            // unconditionally so the window lands at its real
            // resting position.
            if animating_now
                && item.scroll_frame_throttle > 1
                && self.scroll_frame_index % item.scroll_frame_throttle != 0
            {
                continue;
            }
            let hwnd = item.inner.handle();
            let hwnd_raw = hwnd.0 as u64;
            let padded = item.inner.padded_rect(
                [d.place_x, d.place_y].into(),
                [d.target_w, d.target_h].into(),
            );
            match padded {
                Ok((x, y, w, h)) => {
                    batch.push(crate::window::BatchMove {
                        hwnd,
                        x,
                        y,
                        width: w,
                        height: h,
                    });
                    moved_any = true;
                    self.scroll_profile.record(hwnd_raw);
                }
                Err(e) => warn!("padded_rect failed during layout: {e}"),
            }
        }

        if let Err(e) = crate::window::batch_move_windows(&batch) {
            warn!("batch_move_windows failed; falling back to per-window moves: {e}");
            for d in &decisions {
                let item = &self.windows[d.idx];
                if let Err(e2) = item.inner.move_to(
                    [d.place_x, d.place_y].into(),
                    [d.target_w, d.target_h].into(),
                ) {
                    warn!("per-window move_to fallback failed: {e2}");
                }
            }
        }

        // Update last_layout for every window we just decided on (even
        // those we skipped via the eps gate — they're still at the
        // expected rect).
        for d in &decisions {
            self.windows[d.idx].last_layout =
                Some((d.place_x, d.place_y, d.target_w, d.target_h));
        }

        // Clip pass.
        //
        // During animation we actively *clear* every existing clip on the
        // first frame so windows render full-width while the strip is
        // moving — the previous behaviour ("skip clip changes while
        // animating") left stale clip rects in place, which the user saw
        // as a "clipped plane scrolling inwards then snapping the second
        // half of the window into view at settle". Per explicit user
        // direction we accept transient strip spillover onto the
        // secondary monitor during the slide and handle the overflow
        // some other way later.
        //
        // After the first animation frame, every `last_clip` is `None`
        // and the inner check makes subsequent frames effectively
        // no-ops. On settle (`!animating`), we re-evaluate and apply the
        // correct clip for the resting state.
        if animating {
            for d in &decisions {
                let item = &mut self.windows[d.idx];
                if item.last_clip.is_some() {
                    if let Err(e) = item.inner.clear_visible_region() {
                        warn!("clear_visible_region (scroll-start clear) failed: {e}");
                    }
                    item.last_clip = None;
                }
            }
        } else {
            for d in &decisions {
                let item = &mut self.windows[d.idx];
                // Chromium-class windows: never apply (or re-apply) a
                // clip. If a previous winri version (or this session,
                // before the window was classified) left one on, scrub
                // it once and move on. See `Window::is_clip_unsafe`.
                if item.skip_clipping {
                    if item.last_clip.is_some() {
                        if let Err(e) = item.inner.clear_visible_region() {
                            warn!("clear_visible_region (chromium scrub) failed: {e}");
                        }
                        item.last_clip = None;
                    }
                    continue;
                }
                if d.fully_offscreen {
                    if item.last_clip.is_some() {
                        if let Err(e) = item.inner.clear_visible_region() {
                            warn!("clear_visible_region (park transition) failed: {e}");
                        }
                        item.last_clip = None;
                    }
                } else if d.clip.is_none() {
                    if item.last_clip.is_some() {
                        if let Err(e) = item.inner.clear_visible_region() {
                            warn!("clear_visible_region (fully-visible transition) failed: {e}");
                        }
                        item.last_clip = None;
                    }
                } else if let Some(next_clip) = d.clip {
                    if item.last_clip != Some(next_clip) {
                        let (l, t, r, b) = next_clip;
                        if let Err(e) = item.inner.set_visible_region(l, t, r, b) {
                            warn!("set_visible_region failed: {e}");
                        } else {
                            item.last_clip = Some(next_clip);
                        }
                    }
                }
            }
        }
        if moved_any {
            self.mark_motion();
        }

        // Update animation-state transition tracking and, on the
        // scroll-end frame, log the per-app SetWindowPos profile.
        self.was_animating_last_layout = animating_now;
        if scroll_just_ended {
            if let Some(snap) = self.scroll_profile.finish() {
                let parts: Vec<String> = snap
                    .counts
                    .iter()
                    .take(15)
                    .map(|(hwnd_raw, count)| {
                        let item = self
                            .windows
                            .iter()
                            .find(|w| w.inner.handle().0 as u64 == *hwnd_raw);
                        let process = item
                            .map(|w| w.inner.process_name().unwrap_or_default())
                            .unwrap_or_default();
                        let lag_px = item
                            .and_then(|w| {
                                let (lx, _ly, _lw, _lh) = w.last_layout?;
                                let bounds = w.inner.desktop_manager_bounds().ok()?;
                                Some((bounds.position().x() - lx).abs())
                            })
                            .unwrap_or(0.0);
                        format!("{process}={count}/{}@{lag_px:.0}px", snap.frame_count)
                    })
                    .collect();
                info!(
                    "ScrollProfile: {}ms, {} frames, {} apps moved: {}",
                    snap.duration.as_millis(),
                    snap.frame_count,
                    snap.counts.len(),
                    parts.join(", ")
                );
            }
        }
    }

    /// Width
    fn update_widths(&mut self) {
        /// Max width delta from cached `window.width` that we trust
        /// from `desktop_manager_bounds` while the mouse isn't held.
        /// Above this, assume the OS silently rejected our last
        /// SetWindowPos (Chromium i16 clamp, app-side
        /// WM_GETMINMAXINFO, modal apps with strict size constraints)
        /// and the bounds value is the stale pre-resize / app-natural
        /// width. Without this clamp, those rejections silently
        /// rewrite our tile width to whatever the app preferred,
        /// producing the "huge active width on desktop 2" effect:
        /// freshly-tiled apps stomp default_size with their natural
        /// 1500–2000 px size on the very first snapshot.
        ///
        /// 250px is generous enough to admit a fast legitimate
        /// drag-resize (~500 px/s × ~200 ms snapshot = ~100 px) while
        /// catching the typical "Chrome stayed at 1900, we wanted 1270"
        /// case.
        const MAX_UNTRUSTED_DELTA: f32 = 250.0;

        // When the mouse is held, the user is most likely actively
        // resizing — large per-tick deltas are expected and should be
        // trusted. The clamp only kicks in at rest, where bounds-vs-
        // cache disagreement implies an unobserved rejection.
        let mouse_held = is_left_mouse_held();

        let max_screen_width = self.max_screen_width();
        for window in &mut self.windows {
            if let Some(requested_width) = window.requested_width() {
                window.width = requested_width;
            } else if window.inner.is_cloaked().unwrap_or(false) {
                // Cloaked = on another virtual desktop. desktop_manager_bounds
                // for these can return stale/zeroed data; preserve the
                // last-known visible width so it's correct when un-cloaked.
                continue;
            } else if window.inner.is_iconic() {
                // Iconic windows return the icon-strip rect (~200 px) from
                // desktop_manager_bounds — never trust that as the tile
                // width. The pre-minimize snapshot in handle_window_snapshot
                // re-asserts the real intent on restore.
                continue;
            } else if let Ok(bounds) = window
                .inner
                .desktop_manager_bounds()
                .context("Updating widths")
                .error()
                .log_err()
            {
                let new_w = bounds.size().width().min(max_screen_width);
                if !mouse_held && (new_w - window.width).abs() > MAX_UNTRUSTED_DELTA {
                    // Suspect bounds — keep cached intent. The next
                    // layout pass will re-issue SetWindowPos with our
                    // width; whether it sticks or not, we won't have
                    // overwritten our intent in the meantime.
                    continue;
                }
                window.width = new_w;
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

        // Cloaked (other virtual desktop) and iconic (minimized) windows
        // contribute zero strip space so the visible windows pack tight
        // — no empty gap where a minimized tile used to live. We still
        // emit one position per WindowItem (sentinel = current cursor)
        // so the returned vector indices stay aligned with `self.windows`.
        for window in &self.windows {
            if window.inner.is_cloaked().unwrap_or(false) || window.inner.is_iconic() {
                positions.push(current_position);
                continue;
            }
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
