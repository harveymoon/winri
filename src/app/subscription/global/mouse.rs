//! Low-level Win32 mouse hook for Win+wheel horizontal scrolling.
//!
//! rdev's `_grab` on Windows only delivers keyboard events, so we install a
//! separate `WH_MOUSE_LL` hook to observe wheel events. When the Win key is
//! held during a wheel tick, we emit a `HorizontalScroll` message back to the
//! app and **suppress** the original event so the underlying window doesn't
//! also scroll.

use std::{
    sync::{
        Mutex, OnceLock,
        atomic::Ordering,
    },
    thread,
};

use iced::futures::channel::mpsc::Sender;
use windows::Win32::{
    Foundation::{LPARAM, LRESULT, WPARAM},
    UI::WindowsAndMessaging::{
        CallNextHookEx, GetMessageW, HHOOK, MSG, MSLLHOOKSTRUCT, SetWindowsHookExW, WH_MOUSE_LL,
        WM_MOUSEWHEEL,
    },
};

use crate::app::subscription::global::{GlobalMessage, WIN_DOWN};

/// One wheel notch is `WHEEL_DELTA` (120). Scale to roughly 100 px per notch.
const SCROLL_PX_PER_WHEEL_DELTA: f32 = 100.0 / 120.0;

static MOUSE_TX: OnceLock<Mutex<Sender<GlobalMessage>>> = OnceLock::new();

fn is_win_down() -> bool {
    // rdev's keyboard hook swallows Win-key events, which on modern Windows
    // also suppresses the GetAsyncKeyState bit. So we mirror the Win state
    // ourselves in `WIN_DOWN` from the keyboard hook and read it here.
    WIN_DOWN.load(Ordering::Relaxed)
}

unsafe extern "system" fn mouse_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 && wparam.0 == WM_MOUSEWHEEL as usize {
        let info = unsafe { &*(lparam.0 as *const MSLLHOOKSTRUCT) };
        // The wheel delta is the signed high word of `mouseData`.
        #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
        let raw_delta = (info.mouseData >> 16) as i16;
        if is_win_down() {
            // Wheel up (positive delta) should slide content rightward — i.e.
            // shift the viewport leftward (decrease scroll_offset). Flip sign.
            let delta_px = -f32::from(raw_delta) * SCROLL_PX_PER_WHEEL_DELTA;

            if let Some(tx) = MOUSE_TX.get()
                && let Ok(mut sender) = tx.lock()
            {
                let _ = sender.try_send(GlobalMessage::HorizontalScroll { delta_px });
            }

            // Suppress: don't pass the event to the focused app, since the
            // user clearly meant to scroll winri's tile strip, not the
            // window underneath.
            return LRESULT(1);
        }
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

/// Install the global low-level mouse hook on a dedicated thread. The thread
/// runs a Windows message loop forever so the hook keeps firing; it exits
/// only when winri does.
pub fn launch(tx: Sender<GlobalMessage>) {
    if MOUSE_TX.set(Mutex::new(tx)).is_err() {
        log::warn!("Mouse hook already initialised; skipping re-launch");
        return;
    }

    let _ = thread::Builder::new()
        .name("global-mouse-hook".into())
        .spawn(|| {
            let hook: HHOOK = match unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), None, 0) } {
                Ok(h) => h,
                Err(e) => {
                    log::error!("SetWindowsHookExW(WH_MOUSE_LL) failed: {e}");
                    return;
                }
            };
            log::info!("Mouse hook installed ({hook:?})");
            let mut msg = MSG::default();
            // Pump messages so the OS keeps calling the hook proc. GetMessageW
            // returns 0 on WM_QUIT, -1 on error.
            loop {
                let r = unsafe { GetMessageW(&mut msg, None, 0, 0) };
                if r.0 <= 0 {
                    break;
                }
            }
        });
}
