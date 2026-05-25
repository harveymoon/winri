//! Local HTTP control API for winri.
//!
//! Exposes a small JSON-over-HTTP surface that external tools (Stream Deck
//! plugins, Python macro scripts, voice control, etc.) can drive winri with.
//! See `INTEGRATION.md` at the repo root for a usage guide.
//!
//! Concurrency model:
//! - A background thread runs `tiny_http` and accepts requests.
//! - Read endpoints (windows list, current state, thumbnails) consult a
//!   process-wide snapshot under [`ApiState`] that the main thread updates
//!   on every tiler change.
//! - Write endpoints (focus, scroll, actions) push a typed [`ApiCommand`]
//!   onto a channel that the main thread drains via the iced subscription
//!   system and dispatches as a `Message::Api(...)`.

mod capture;
mod events;
mod server;
mod types;

pub use server::launch;
pub use types::{ApiCommand, NamedAction, ScrollRequest};

use std::sync::{Mutex, OnceLock, RwLock, RwLockReadGuard};

use iced::futures::channel::mpsc::Sender;

use crate::app::Message;

#[derive(Debug, Clone, Default)]
pub struct ApiState {
    /// Current winri mode — `"tiler"`, `"overview"`, or `"exit"` (the last
    /// is transient and you'll basically never observe it).
    pub mode: String,
    pub windows: Vec<WindowSnapshot>,
    pub focused_window_id: Option<u64>,
    pub scroll_offset: f32,
    /// Total horizontal extent of all tiled windows + padding. Lets clients
    /// compute the meaningful scroll range as `0 .. (total_width -
    /// screen_width).max(0)`.
    pub total_width: f32,
    pub screen_width: f32,
    pub screen_height: f32,
    pub tiling_monitor_device_name: String,
    pub monitors: Vec<MonitorSnapshot>,
}

#[derive(Debug, Clone)]
pub struct MonitorSnapshot {
    pub index: usize,
    pub device_name: String,
    pub is_primary: bool,
    pub is_tiling: bool,
    pub work_area_x: i32,
    pub work_area_y: i32,
    pub work_area_width: i32,
    pub work_area_height: i32,
}

#[derive(Debug, Clone)]
pub struct WindowSnapshot {
    /// Raw Win32 HWND, used as the API's stable per-session window id.
    pub id: u64,
    pub title: String,
    pub process: String,
    pub class: String,
    /// Width allocated to the window in the tile strip, in pixels.
    pub width: f32,
    /// X position of the window's left edge in tile-strip coordinates
    /// (i.e. NOT screen-space; subtract scroll_offset to get on-screen X).
    pub x: f32,
    /// Device name of the monitor the window is currently on
    /// (e.g. `\\.\DISPLAY1`).
    pub monitor: String,
    /// `true` if the window is part of the tiler's strip, `false` if it's
    /// a free-floating window.
    pub tiled: bool,
    /// `true` if the window is currently minimized (`IsIconic`).
    pub minimized: bool,
    /// 1-indexed virtual-desktop number. `None` for windows the OS
    /// doesn't track via the public virtual-desktop API (some shell
    /// windows). See `virtual_desktop` module docs for indexing rules.
    pub desktop_id: Option<u32>,
}

static API_STATE: OnceLock<RwLock<ApiState>> = OnceLock::new();
static API_CMD_TX: OnceLock<Mutex<Sender<Message>>> = OnceLock::new();

fn state_slot() -> &'static RwLock<ApiState> {
    API_STATE.get_or_init(|| RwLock::new(ApiState::default()))
}

/// Update the snapshot read by API consumers. Called by the main thread
/// whenever the tiler state changes. Also fans out a serialized snapshot
/// to any open SSE subscribers — `events::publish` itself diff-suppresses
/// byte-identical payloads, so animation-tick spam at 60Hz doesn't reach
/// clients.
pub fn publish_state(state: ApiState) {
    // Build the public StateResponse before we drop ownership of `state`
    // — both /state's request handler and the SSE channel emit this
    // exact JSON.
    let response = build_state_response(&state);
    *state_slot()
        .write()
        .expect("api state rwlock poisoned") = state;
    if let Ok(json) = serde_json::to_string(&response) {
        events::publish(json);
    }
}

/// Map the internal `ApiState` to the public `StateResponse` JSON shape.
/// Shared by `GET /state` (request-time) and event publication so the
/// two never drift.
pub(crate) fn build_state_response(state: &ApiState) -> types::StateResponse {
    let windows = state
        .windows
        .iter()
        .map(|w| types::WindowDescriptor {
            id: w.id,
            title: w.title.clone(),
            process: w.process.clone(),
            class: w.class.clone(),
            width: w.width,
            x: w.x,
            focused: Some(w.id) == state.focused_window_id,
            monitor: w.monitor.clone(),
            tiled: w.tiled,
            minimized: w.minimized,
            desktop_id: w.desktop_id,
        })
        .collect();
    let monitors = state
        .monitors
        .iter()
        .map(|m| types::MonitorDescriptor {
            index: m.index,
            device_name: m.device_name.clone(),
            is_primary: m.is_primary,
            is_tiling: m.is_tiling,
            work_area: types::WorkArea {
                x: m.work_area_x,
                y: m.work_area_y,
                width: m.work_area_width,
                height: m.work_area_height,
            },
        })
        .collect();
    types::StateResponse {
        mode: state.mode.clone(),
        overview_active: state.mode == "overview",
        windows,
        focused_id: state.focused_window_id,
        scroll_offset: state.scroll_offset,
        total_width: state.total_width,
        screen_width: state.screen_width,
        screen_height: state.screen_height,
        tiling_monitor: state.tiling_monitor_device_name.clone(),
        monitors,
    }
}

pub fn current_state() -> RwLockReadGuard<'static, ApiState> {
    state_slot()
        .read()
        .expect("api state rwlock poisoned")
}

/// Register the iced message channel so command endpoints can dispatch to
/// the main loop. Called once at startup if the API is enabled.
pub fn set_command_sender(tx: Sender<Message>) {
    let _ = API_CMD_TX.set(Mutex::new(tx));
}

pub(crate) fn send_message(message: Message) -> Result<(), &'static str> {
    let Some(tx) = API_CMD_TX.get() else {
        return Err("api command sender not initialised");
    };
    let mut tx = tx.lock().map_err(|_| "api command sender mutex poisoned")?;
    tx.try_send(message).map_err(|_| "api command channel full or closed")
}
