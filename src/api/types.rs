//! JSON request/response types and the command enum the API forwards into
//! the main loop.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ScrollRequest {
    /// Absolute scroll offset, in pixels. If both this and `delta` are
    /// provided, `offset` wins.
    #[serde(default)]
    pub offset: Option<f32>,
    /// Relative scroll, in pixels. Positive shifts the viewport rightward
    /// (windows slide left).
    #[serde(default)]
    pub delta: Option<f32>,
    /// Optional animation duration in ms. `0` (default) snaps instantly
    /// — backward compatible with every existing caller. A positive
    /// value tweens from the current `scroll_offset` to the requested
    /// target over the duration using the same ease-out-cubic and 16ms
    /// tick loop that drives smooth resize. Sending another /scroll
    /// while one is in flight **replaces** the in-flight target
    /// (latest wins, no queue) — ideal for pointermove-streamed scrub
    /// bars.
    #[serde(default)]
    pub animate_ms: u32,
}

/// Discrete, named actions equivalent to the keyboard shortcuts. The string
/// values are stable wire identifiers — match these in `INTEGRATION.md`.
#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum NamedAction {
    FocusPrev,
    FocusNext,
    SwapPrev,
    SwapNext,
    ResizeFullscreen,
    ResizeHalfscreen,
    WidthIncrement,
    WidthDecrement,
    Refresh,
    CenterFocused,
    OpenOverview,
    CloseOverview,
    OpenSettings,
    Exit,
}

/// All API operations that mutate winri state, packaged for the main loop.
/// The HTTP server constructs one of these per request and forwards it
/// through the iced message channel.
#[derive(Debug, Clone)]
pub enum ApiCommand {
    /// Focus the window with the given HWND, scrolling the strip if
    /// necessary (uses the normal focus path so all UX rules apply).
    Focus(u64),
    /// Apply an absolute scroll offset. `animate_ms == 0` snaps; positive
    /// values run a time-based scroll animation that supersedes any
    /// in-flight one.
    SetScrollOffset { offset: f32, animate_ms: u32 },
    /// Apply a relative scroll delta in pixels. Same `animate_ms`
    /// semantics as `SetScrollOffset`.
    ScrollBy { delta: f32, animate_ms: u32 },
    /// Run one of the named keyboard-equivalent actions.
    Action(NamedAction),
    /// Move the window with the given HWND to the monitor identified by
    /// device name (e.g. `\\.\DISPLAY2`).
    MoveToMonitor { hwnd: u64, device_name: String },
    /// Smoothly resize the tile width for the window with the given HWND.
    /// `animate_ms == 0` snaps instantly. If `center` is true, scroll
    /// smoothly so the resized window ends up centered in the viewport.
    ResizeWindow {
        hwnd: u64,
        target_width: f32,
        animate_ms: u32,
        center: bool,
    },
}

/// Returned by `GET /windows`.
#[derive(Debug, Clone, Serialize)]
pub struct WindowDescriptor {
    /// Stable per-session id (Win32 HWND).
    pub id: u64,
    pub title: String,
    pub process: String,
    pub class: String,
    pub width: f32,
    pub x: f32,
    pub focused: bool,
    pub monitor: String,
    pub tiled: bool,
    /// `true` if the window is currently minimized to the taskbar.
    pub minimized: bool,
    /// 1-indexed virtual-desktop number. Omitted when the OS doesn't
    /// track the window in any virtual desktop (rare shell windows).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub desktop_id: Option<u32>,
}

/// Returned by `GET /monitors`.
#[derive(Debug, Clone, Serialize)]
pub struct MonitorDescriptor {
    pub index: usize,
    pub device_name: String,
    pub is_primary: bool,
    pub is_tiling: bool,
    pub work_area: WorkArea,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkArea {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

/// Body for `POST /windows/{id}/move-to-monitor`.
#[derive(Debug, Clone, Deserialize)]
pub struct MoveToMonitorRequest {
    pub device_name: String,
}

/// Body for `POST /windows/{id}/resize`. `animate_ms` defaults to 0
/// (instant) when omitted; pass e.g. 250 for a quarter-second smooth
/// transition driven by the same 16ms tick loop as scroll smoothing.
///
/// `center` (default false) additionally starts a smooth scroll so the
/// resized window ends up centered in the viewport at its final width —
/// useful for a "fullscreen this app" button that should both grow the
/// tile and bring it on-screen in one motion.
#[derive(Debug, Clone, Deserialize)]
pub struct ResizeRequest {
    pub width: f32,
    #[serde(default)]
    pub animate_ms: u32,
    #[serde(default)]
    pub center: bool,
}

/// Returned by `GET /state`.
#[derive(Debug, Clone, Serialize)]
pub struct StateResponse {
    /// "tiler" | "overview" | "exit".
    pub mode: String,
    /// Convenience: true when `mode == "overview"`.
    pub overview_active: bool,
    pub windows: Vec<WindowDescriptor>,
    pub focused_id: Option<u64>,
    pub scroll_offset: f32,
    /// Total tile-strip width including padding.
    pub total_width: f32,
    pub screen_width: f32,
    pub screen_height: f32,
    pub tiling_monitor: String,
    pub monitors: Vec<MonitorDescriptor>,
}

/// Returned on errors.
#[derive(Debug, Clone, Serialize)]
pub struct ErrorBody {
    pub error: String,
}
