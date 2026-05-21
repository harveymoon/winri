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
}

static API_STATE: OnceLock<RwLock<ApiState>> = OnceLock::new();
static API_CMD_TX: OnceLock<Mutex<Sender<Message>>> = OnceLock::new();

fn state_slot() -> &'static RwLock<ApiState> {
    API_STATE.get_or_init(|| RwLock::new(ApiState::default()))
}

/// Update the snapshot read by API consumers. Called by the main thread
/// whenever the tiler state changes.
pub fn publish_state(state: ApiState) {
    *state_slot()
        .write()
        .expect("api state rwlock poisoned") = state;
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

fn send_message(message: Message) -> Result<(), &'static str> {
    let Some(tx) = API_CMD_TX.get() else {
        return Err("api command sender not initialised");
    };
    let mut tx = tx.lock().map_err(|_| "api command sender mutex poisoned")?;
    tx.try_send(message).map_err(|_| "api command channel full or closed")
}
