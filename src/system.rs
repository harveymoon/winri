use std::ffi::c_void;

use log::warn;
use windows::Win32::{
    Foundation::{HWND, RECT},
    Graphics::Gdi::{COLOR_HIGHLIGHT, GetSysColor},
    UI::{
        Input::KeyboardAndMouse::{
            GetKeyState, VIRTUAL_KEY, VK_CONTROL, VK_LCONTROL, VK_LSHIFT, VK_LWIN, VK_MENU,
            VK_RWIN, VK_SHIFT,
        },
        WindowsAndMessaging::{
            FindWindowW, GetDesktopWindow, GetSystemMetrics, GetWindowRect, IDYES, IsWindowVisible,
            MB_OK, MB_YESNO, SM_CXSCREEN, SM_CYSCREEN, SPI_GETWORKAREA,
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SystemParametersInfoW,
        },
    },
};
use windows_strings::w;

use crate::{
    utils::math::{Position, Size},
    winapi, wincall_into_result,
    window::{self, Window},
};

/// Returns the dimensions of the primary monitor's work area — i.e. the
/// monitor rect minus the taskbar and other appbars. Tiled windows and the
/// winri overlay should be laid out within these bounds so they don't
/// overlap the taskbar.
///
/// Handling on Windows 11:
/// - `SPI_GETWORKAREA` is the primary source of truth, but it sometimes still
///   includes the taskbar strip (e.g. with auto-hide, or with third-party
///   shells like ExplorerPatcher). So we also locate `Shell_TrayWnd` and
///   subtract its full height from the bottom/right edges of the work area.
/// - For an auto-hide taskbar we still reserve its full slide-out size. The
///   "let it overlay tiles" approach would conflict with winri's topmost
///   transparent overlay, so we leave an empty strip the bar can slide into
///   instead.
/// - We only subtract from the bottom/right edges; the rest of winri assumes
///   the work area starts at (0, 0). A top/left taskbar would require
///   threading an origin offset through the tiler and overlay, which is left
///   as future work — we log a warning in that case.
pub fn screen_size() -> anyhow::Result<Size> {
    use anyhow::Context;

    let mut work = RECT::default();
    unsafe {
        SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            Some(&raw mut work as *mut c_void),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    }
    .context("SystemParametersInfoW(SPI_GETWORKAREA)")?;

    let screen = primary_screen_rect();

    if let Some(tray) = taskbar_rect() {
        let bar_width = tray.right - tray.left;
        let bar_height = tray.bottom - tray.top;
        let screen_mid_x = (screen.left + screen.right) / 2;
        let screen_mid_y = (screen.top + screen.bottom) / 2;

        if bar_width >= bar_height {
            // Horizontal taskbar — top or bottom edge.
            let bar_center_y = (tray.top + tray.bottom) / 2;
            if bar_center_y >= screen_mid_y {
                // Bottom: reserve the bar's full height regardless of whether
                // it's currently shown (covers auto-hide).
                work.bottom = work.bottom.min(screen.bottom - bar_height);
            } else {
                warn!(
                    "Taskbar appears to be on the top edge; winri does not yet shift its layout \
                     origin and tiled windows will overlap it."
                );
            }
        } else {
            let bar_center_x = (tray.left + tray.right) / 2;
            if bar_center_x >= screen_mid_x {
                work.right = work.right.min(screen.right - bar_width);
            } else {
                warn!(
                    "Taskbar appears to be on the left edge; winri does not yet shift its layout \
                     origin and tiled windows will overlap it."
                );
            }
        }
    }

    if work.left != 0 || work.top != 0 {
        warn!(
            "Work area does not start at (0, 0): origin = ({}, {}). \
             Tiled windows may not be positioned correctly.",
            work.left, work.top
        );
    }

    #[allow(
        clippy::cast_precision_loss,
        reason = "The values will stay within screen size orders of magnitude"
    )]
    Ok(Size([
        (work.right - work.left) as f32,
        (work.bottom - work.top) as f32,
    ]))
}

/// Locate the primary taskbar window (`Shell_TrayWnd`) and return its screen
/// rect, or `None` if it can't be found or isn't visible.
fn taskbar_rect() -> Option<RECT> {
    let hwnd: HWND = unsafe { FindWindowW(w!("Shell_TrayWnd"), None) }.ok()?;
    if !unsafe { IsWindowVisible(hwnd) }.as_bool() {
        return None;
    }
    let mut rect = RECT::default();
    unsafe { GetWindowRect(hwnd, &raw mut rect) }.ok()?;
    Some(rect)
}

/// The rect of the primary monitor in screen coordinates. The window-rs API
/// doesn't expose a single call for this, but `SM_CXSCREEN`/`SM_CYSCREEN` give
/// the primary monitor's dimensions and its origin is always (0, 0).
fn primary_screen_rect() -> RECT {
    RECT {
        left: 0,
        top: 0,
        right: unsafe { GetSystemMetrics(SM_CXSCREEN) },
        bottom: unsafe { GetSystemMetrics(SM_CYSCREEN) },
    }
}

pub fn highlight_color() -> anyhow::Result<iced::Color> {
    // argb
    let packed = wincall_into_result!(GetSysColor(COLOR_HIGHLIGHT))?;

    let r = (packed & 0x0000_00FF) as u8;
    let g = ((packed & 0x0000_FF00) >> 8) as u8;
    let b = ((packed & 0x00FF_0000) >> 16) as u8;
    Ok(iced::Color::from_rgb8(r, g, b))
}

/// Restore all tiled windows to a cascading position for user convenience.
/// Typically called on application exit (nominal or error), so that windows are not lost off-screen.
pub fn restore_windows() {
    let mut windows = Window::enumerate().unwrap_or_else(|e| {
        warn!("Could not enumerate windows to restore them: {e}");
        vec![]
    });

    windows.retain(|w| window::filter::should_be_tiled(*w).unwrap_or(false));

    let mut pos = Position([100.0, 100.0]);
    for window in windows {
        if let Err(err) = window.move_to(pos, [800.0, 600.0].into()) {
            warn!("Failed to move window {window:?}: {err}");
        }
        pos += 100.0;
    }
}

pub fn get_desktop_window() -> anyhow::Result<Window> {
    Window::from_hwnd(wincall_into_result!(GetDesktopWindow())?)
}

#[must_use]
pub fn current_modifiers() -> keyboard_types::Modifiers {
    fn is_vkey_down(key: VIRTUAL_KEY) -> bool {
        let key_state = match wincall_into_result!(GetKeyState(key.0.into())) {
            Ok(res) => u16::from_ne_bytes(res.to_ne_bytes()),
            Err(e) => {
                log::error!(
                    "Error while checking if {key:?} modifier is pressed, defaulting to not pressed: {e}"
                );
                0
            }
        };

        key_state & 0xFF80 == 0xFF80
    }

    let mut modifiers = keyboard_types::Modifiers::empty();

    if is_vkey_down(VK_SHIFT) || is_vkey_down(VK_LSHIFT) {
        modifiers.insert(keyboard_types::Modifiers::SHIFT);
    }

    if is_vkey_down(VK_CONTROL) || is_vkey_down(VK_LCONTROL) {
        modifiers.insert(keyboard_types::Modifiers::CONTROL);
    }

    if is_vkey_down(VK_LWIN) || is_vkey_down(VK_RWIN) {
        modifiers.insert(keyboard_types::Modifiers::META);
    }

    if is_vkey_down(VK_MENU) {
        modifiers.insert(keyboard_types::Modifiers::ALT);
    }

    modifiers
}

#[allow(dead_code, reason = "Could be useful at some point")]
pub fn message_box_info(title: &str, message: &str) {
    winapi::message_box(title, message, MB_OK);
}

pub fn message_box_query(title: &str, message: &str) -> bool {
    winapi::message_box(title, message, MB_YESNO) == IDYES
}
