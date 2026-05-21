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
    /// Apply an absolute scroll offset.
    SetScrollOffset(f32),
    /// Apply a relative scroll delta in pixels.
    ScrollBy(f32),
    /// Run one of the named keyboard-equivalent actions.
    Action(NamedAction),
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
}

/// Returned on errors.
#[derive(Debug, Clone, Serialize)]
pub struct ErrorBody {
    pub error: String,
}
