/// `Window` is a wrapper around a Windows HWND handle, providing methods to interact with and retrieve information about the window.
pub mod filter;

use std::{ffi::c_void, hash::Hash, thread, time::Duration};

use anyhow::{Context, ensure};
use windows::{
    Win32::{
        Foundation::{CloseHandle, HANDLE, HWND, LPARAM, RECT, WPARAM},
        Graphics::Dwm::{
            DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS, DWMWINDOWATTRIBUTE, DwmGetWindowAttribute,
        },
        System::{
            ProcessStatus::GetModuleFileNameExW,
            Threading::{OpenProcess, PROCESS_QUERY_INFORMATION, PROCESS_VM_READ},
        },
        UI::WindowsAndMessaging::{
            EnumWindows, GA_ROOT, GW_OWNER, GWL_EXSTYLE, GWL_STYLE, GetAncestor, GetClassNameW,
            GetClientRect, GetWindow, GetWindowLongW, GetWindowRect, GetWindowTextLengthW,
            GetWindowTextW, GetWindowThreadProcessId, IsIconic, IsWindow, IsWindowVisible,
            MoveWindow, PostMessageW, SW_RESTORE, SWP_NOSIZE, SetForegroundWindow, SetWindowPos,
            ShowWindow, WINDOW_LONG_PTR_INDEX, WINDOW_STYLE, WM_CLOSE, WS_DLGFRAME, WS_EX_TOOLWINDOW,
            WS_POPUP,
        },
    },
    core::BOOL,
};

use crate::{
    utils::math::{Bounds, Position, Size},
    wincall_into_result, wincall_result,
};

pub type SafeHWND = u64;

/// Per-window timeout for the `WM_NULL` probe that precedes
/// `SetWindowRgn`. A slow-but-not-ghosted app (TouchDesigner,
/// Photoshop, etc.) can keep its message pump alive at a few-hundred-
/// ms cadence — long enough to dodge `IsHungAppWindow` but long
/// enough to stall a synchronous `SetWindowRgn` for seconds. 50 ms
/// is loose enough not to false-positive on a normal app under
/// load, tight enough that a full strip of 14 stuck windows costs
/// at most ~700 ms instead of the 29-second freeze we observed.
const SETWINDOWRGN_PROBE_MS: u32 = 50;

/// Process-wide gate that suppresses `DwmGetWindowAttribute` calls
/// during the dwm.exe restart window.
///
/// When DWM restarts (graphics driver reset, dwm.exe crash, certain
/// theme/display changes), every `DwmGetWindowAttribute` call returns
/// `E_HANDLE` (0x80070006) for *every* HWND — the kernel handles are
/// still valid but DWM has dropped its per-window state. Our snapshot
/// tick fires `desktop_manager_bounds()` per window per tick, so each
/// dead second produces dozens of identical errors in the log and
/// keeps hammering the half-restarted DWM with cross-apartment RPC.
///
/// The gate is a single timestamp. On E_HANDLE we set "don't call
/// again until now+250 ms" and log once. Probes after the cooldown
/// fall through; success clears the gate and logs once. While the
/// gate is held we also drop the cached `IVirtualDesktopManager` so
/// the next caller pulls a fresh COM proxy against the new dwm.exe.
mod dwm_health {
    use std::{
        sync::{
            OnceLock,
            atomic::{AtomicU64, Ordering},
        },
        time::{Duration, Instant},
    };

    const DWM_BACKOFF: Duration = Duration::from_millis(250);

    static EPOCH: OnceLock<Instant> = OnceLock::new();
    /// Milliseconds since `EPOCH` after which calls may probe again.
    /// `0` means healthy; any non-zero value means we're in backoff
    /// and callers should skip until `now_ms() >= retry_at_ms`.
    static RETRY_AT_MS: AtomicU64 = AtomicU64::new(0);

    fn now_ms() -> u64 {
        let epoch = *EPOCH.get_or_init(Instant::now);
        #[allow(
            clippy::cast_possible_truncation,
            reason = "u128 → u64 truncation only matters after ~584 million years uptime"
        )]
        {
            Instant::now().saturating_duration_since(epoch).as_millis() as u64
        }
    }

    /// Returns `true` if a `DwmGetWindowAttribute` call should
    /// proceed. Returns `false` while the backoff window is active.
    pub fn allow_call() -> bool {
        let retry_at = RETRY_AT_MS.load(Ordering::Relaxed);
        retry_at == 0 || now_ms() >= retry_at
    }

    /// Note an `E_HANDLE` from `DwmGetWindowAttribute`. Arms the
    /// backoff window. Logs (and invalidates the cached virtual-
    /// desktop manager) only on the healthy→unhealthy transition.
    pub fn mark_unhealthy() {
        #[allow(
            clippy::cast_possible_truncation,
            reason = "DWM_BACKOFF is 250 ms; u128 → u64 is lossless"
        )]
        let next = now_ms() + DWM_BACKOFF.as_millis() as u64;
        let prev = RETRY_AT_MS.swap(next, Ordering::Relaxed);
        if prev == 0 {
            log::warn!(
                "DWM unavailable (E_HANDLE from DwmGetWindowAttribute) — backing off DwmGetWindowAttribute for {:?}; invalidating cached IVirtualDesktopManager",
                DWM_BACKOFF
            );
            crate::virtual_desktop::invalidate_cached_manager();
        }
    }

    /// Note a successful `DwmGetWindowAttribute`. Clears the gate
    /// and logs once on the unhealthy→healthy transition.
    pub fn mark_healthy() {
        let prev = RETRY_AT_MS.swap(0, Ordering::Relaxed);
        if prev != 0 {
            log::info!("DWM responsive again");
        }
    }
}

/// RAII closer for an `OpenProcess` HANDLE. Dropping it calls `CloseHandle`
/// so the handle is released on every path including early-returns.
struct ProcessHandleGuard(HANDLE);

impl Drop for ProcessHandleGuard {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            let _ = unsafe { CloseHandle(self.0) };
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    pub hwnd: SafeHWND,
}

/// One target rect for a batched window move (in screen pixels, already
/// padded for DWM frame).
#[derive(Debug, Clone, Copy)]
pub struct BatchMove {
    pub hwnd: HWND,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

/// Move many windows in one pass. Historically this tried
/// `BeginDeferWindowPos`/`DeferWindowPos`/`EndDeferWindowPos` for a
/// single DWM composition frame, but the very first deferred call
/// failed with `E_INVALIDARG` on **every** batch in this codebase
/// (root cause never identified across multiple sessions of
/// investigation — the deferred path was 100% dead). The per-window
/// `SetWindowPos` fallback was already doing 100% of the actual work,
/// so the batched scaffolding was paying for itself with nothing.
///
/// Now: pre-validate HWNDs (skip stale ones — they close between
/// enumeration and the move), then issue one async `SetWindowPos` per
/// window. Same flag set as before:
/// `SWP_NOACTIVATE | SWP_NOZORDER | SWP_NOSENDCHANGING | SWP_DEFERERASE
/// | SWP_ASYNCWINDOWPOS` — a hung target app can't block the loop.
pub fn batch_move_windows(moves: &[BatchMove]) -> anyhow::Result<()> {
    use windows::Win32::UI::WindowsAndMessaging::{
        IsWindow, SetWindowPos, SWP_ASYNCWINDOWPOS, SWP_DEFERERASE, SWP_NOACTIVATE,
        SWP_NOSENDCHANGING, SWP_NOZORDER,
    };

    let flags = SWP_NOACTIVATE
        | SWP_NOZORDER
        | SWP_NOSENDCHANGING
        | SWP_DEFERERASE
        | SWP_ASYNCWINDOWPOS;

    for mv in moves {
        if !unsafe { IsWindow(Some(mv.hwnd)) }.as_bool() {
            continue;
        }
        if let Err(e) = unsafe {
            SetWindowPos(mv.hwnd, None, mv.x, mv.y, mv.width, mv.height, flags)
        } {
            log::warn!(
                "SetWindowPos rejected hwnd={:?} pos=({}, {}) size=({}, {}) flags=0x{:08x} err={}",
                mv.hwnd, mv.x, mv.y, mv.width, mv.height, flags.0, e,
            );
        }
    }

    Ok(())
}

impl Hash for Window {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        format!("{:?}", self.hwnd).hash(state);
    }
}

impl From<RECT> for Bounds {
    fn from(rect: RECT) -> Self {
        #[allow(
            clippy::cast_precision_loss,
            reason = "The values will stay within screen size orders of magnitude"
        )]
        Self {
            left: rect.left as f32,
            top: rect.top as f32,
            right: rect.right as f32,
            bottom: rect.bottom as f32,
        }
    }
}

macro_rules! ensure_valid {
    ($s:expr) => {
        ensure!(
            $s.is_valid()?,
            "[{}] Invalid window handle: {:?}",
            crate::function!(),
            $s.handle()
        );
    };
}

impl Window {
    pub fn from_hwnd(hwnd: HWND) -> anyhow::Result<Self> {
        ensure!(!hwnd.is_invalid(), "Invalid window handle");
        Ok(Self {
            hwnd: hwnd.0 as SafeHWND,
        })
    }

    pub fn from_safe_hwnd(safe_hwnd: SafeHWND) -> anyhow::Result<Self> {
        let hwnd = HWND(safe_hwnd as *mut c_void);
        Self::from_hwnd(hwnd)
    }

    pub fn focused() -> anyhow::Result<Self> {
        let hwnd =
            wincall_into_result!(windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow())?;
        Self::from_hwnd(hwnd)
    }

    pub const fn handle(self) -> HWND {
        HWND(self.hwnd as *mut c_void)
    }

    pub fn is_valid(self) -> anyhow::Result<bool> {
        Ok(!self.handle().is_invalid()
            && wincall_into_result!(IsWindow(Some(self.handle())))?.as_bool())
    }

    /// Returns the `HMONITOR` of the monitor containing this window (or
    /// the nearest, if it straddles edges). Stored as `isize` so the value
    /// is comparable across threads. Cheap (~microseconds).
    pub fn monitor(self) -> isize {
        crate::monitor::monitor_of_hwnd(self.handle())
    }

    /// `true` if the window is currently minimized to the taskbar. Used
    /// by the tiler to skip layout for minimized windows so the user's
    /// minimize gesture sticks.
    pub fn is_iconic(self) -> bool {
        use windows::Win32::UI::WindowsAndMessaging::IsIconic;
        unsafe { IsIconic(self.handle()) }.as_bool()
    }

    /// `true` if Windows has flagged this window as "not responding" —
    /// it stopped pumping messages long enough for DWM to ghost it
    /// (~5 s). Cheap lookup of an internal flag; does **not** send a
    /// probe message itself. Used to skip synchronous Win32 calls
    /// (`SetWindowRgn`, etc.) that would otherwise block the tiler's
    /// main thread until the target app's UI thread unwedges.
    /// A 36-second `SetWindowRgn` stall on a frozen Chrome window
    /// caused the entire winri main loop to hang in May 2026 — this
    /// guard exists so one hung app can't take down the whole tiler.
    pub fn is_hung(self) -> bool {
        use windows::Win32::UI::WindowsAndMessaging::IsHungAppWindow;
        unsafe { IsHungAppWindow(self.handle()) }.as_bool()
    }

    /// Send a `WM_NULL` probe via `SendMessageTimeoutW` and report
    /// whether the target's UI thread acknowledged it within
    /// `timeout_ms`. `WM_NULL` does nothing — it just round-trips
    /// through the target's message queue, so the response timing
    /// tells us whether the thread is pumping messages right now.
    ///
    /// Complements `is_hung()`, which only catches DWM-ghosted apps
    /// (~5 s stall threshold). An app doing a heavy synchronous
    /// repaint or GPU drain (TouchDesigner, Photoshop, video editors)
    /// can stall message-pumping for seconds at a time without
    /// crossing the ghost threshold. Without this probe, the
    /// subsequent `SetWindowRgn` synchronously blocks on the slow
    /// target until it processes `WM_WINDOWPOSCHANGING` — which is
    /// the 29-second freeze we hit on May 31 2026 against
    /// `TouchDesigner.exe` (1 frame in 29.4 s).
    ///
    /// `SMTO_ABORTIFHUNG` short-circuits if the OS has already
    /// marked the app as hung. The function still blocks for up to
    /// `timeout_ms` for slow-but-not-hung targets — that's the cap
    /// on per-window stall in the scroll-start clip pass.
    pub fn is_responsive(self, timeout_ms: u32) -> bool {
        use windows::Win32::UI::WindowsAndMessaging::{
            SMTO_ABORTIFHUNG, SendMessageTimeoutW, WM_NULL,
        };
        let mut result: usize = 0;
        let ret = unsafe {
            SendMessageTimeoutW(
                self.handle(),
                WM_NULL,
                WPARAM::default(),
                LPARAM::default(),
                SMTO_ABORTIFHUNG,
                timeout_ms,
                Some(&raw mut result),
            )
        };
        ret.0 != 0
    }

    pub fn enumerate() -> anyhow::Result<Vec<Self>> {
        unsafe extern "system" fn enum_callback(window: HWND, out_list: LPARAM) -> BOOL {
            let list = unsafe { &mut *(out_list.0 as *mut Vec<Window>) };
            if let Ok(win) = Window::from_hwnd(window) {
                list.push(win);
            }
            true.into() // Continue enumeration
        }

        let mut result = Vec::new();

        wincall_result!(EnumWindows(
            Some(enum_callback),
            LPARAM(&raw mut result as isize)
        ))?;

        Ok(result)
    }

    fn get_dm_attribute<T>(
        self,
        attribute: DWMWINDOWATTRIBUTE,
        result: &mut T,
    ) -> anyhow::Result<()> {
        // Bail without an RPC round-trip while DWM is mid-restart;
        // see `dwm_health` for the rationale.
        if !dwm_health::allow_call() {
            return Err(anyhow::anyhow!(
                "DwmGetWindowAttribute({}) suppressed: DWM in backoff window",
                attribute.0
            ));
        }

        #[allow(
            clippy::cast_possible_truncation,
            reason = "size of small struct will never be large enough to be truncated"
        )]
        let res = unsafe {
            DwmGetWindowAttribute(
                self.handle(),
                attribute,
                std::ptr::from_mut::<T>(result).cast::<c_void>(),
                std::mem::size_of::<T>() as u32,
            )
        };

        match res {
            Ok(()) => {
                dwm_health::mark_healthy();
                Ok(())
            }
            Err(e) => {
                // Callers (`desktop_manager_bounds`, `is_cloaked`) call
                // `ensure_valid!(self)` before us, so E_HANDLE here is a
                // DWM-level failure, not a dead HWND. Race-window false
                // positives self-heal after the 250 ms cooldown.
                #[allow(
                    clippy::cast_sign_loss,
                    reason = "HRESULT is a signed bit pattern; compare as u32"
                )]
                if (e.code().0 as u32) == 0x8007_0006 {
                    dwm_health::mark_unhealthy();
                }
                Err(anyhow::Error::new(e))
                    .context("winri::window::Window::get_dm_attribute")
                    .context(attribute.0)
            }
        }
    }

    fn get_window_long(self, attribute: WINDOW_LONG_PTR_INDEX) -> anyhow::Result<i32> {
        ensure_valid!(self);
        wincall_into_result!(GetWindowLongW(self.handle(), attribute))
    }

    pub fn is_dialog(self) -> anyhow::Result<bool> {
        ensure_valid!(self);
        let style = self.get_window_long(GWL_STYLE)?;

        #[allow(clippy::cast_sign_loss, reason = "WINDOW_STYLE is u32")]
        let style = WINDOW_STYLE(style as u32);

        Ok(style.contains(WS_POPUP) && style.contains(WS_DLGFRAME))
    }

    /// `true` if the window has the `WS_EX_TOOLWINDOW` extended style.
    /// Apps set this on floating palettes, tool windows, devtools,
    /// color pickers, etc. — windows that intentionally don't appear in
    /// Alt-Tab or the taskbar. Electron uses it heavily for popup
    /// children of a main BrowserWindow. We should not tile these:
    /// they're auxiliaries that belong with their owner.
    pub fn is_tool_window(self) -> anyhow::Result<bool> {
        ensure_valid!(self);
        let exstyle = self.get_window_long(GWL_EXSTYLE)?;
        #[allow(clippy::cast_sign_loss, reason = "WINDOW_EX_STYLE is u32")]
        let exstyle = exstyle as u32;
        Ok(exstyle & WS_EX_TOOLWINDOW.0 != 0)
    }

    /// `true` if this window belongs to a Chromium-based process
    /// (Chrome, Edge, Brave, Opera, every Electron app — anything that
    /// composes through Chromium's `Chrome_WidgetWin_*` HWND).
    ///
    /// We skip `SetWindowRgn` for these: Chromium binds a DirectX swap
    /// chain to the HWND, and rapid region changes detach the surface.
    /// Once detached, the window goes "frosted transparent" (DWM
    /// background visible through where the WebView used to render) and
    /// only restarting the Chromium process recovers it. The secondary-
    /// monitor overflow clip is the visible cost; preserved
    /// compositors are the win.
    pub fn is_clip_unsafe(self) -> anyhow::Result<bool> {
        let class = self.class()?;
        Ok(class.starts_with("Chrome_WidgetWin_"))
    }

    /// `true` if the window has an owner window (per Win32 ownership,
    /// not parent-child). `GetWindow(hwnd, GW_OWNER)` returns the
    /// owning HWND for popups/dialogs that were created with an owner
    /// in `CreateWindowEx`; main app windows have no owner and return
    /// NULL. Filtering on this catches popup BrowserWindows, modal
    /// dialogs, color pickers, autocomplete dropdowns, etc. that
    /// belong to a parent we're already tiling.
    pub fn has_owner(self) -> anyhow::Result<bool> {
        ensure_valid!(self);
        // GetWindow doesn't set last-error on "no owner" — it just
        // returns NULL. Don't wrap in wincall_result.
        let owner = unsafe { GetWindow(self.handle(), GW_OWNER) };
        Ok(owner.as_ref().is_ok_and(|h| !h.is_invalid()))
    }

    pub fn title(self) -> anyhow::Result<Option<String>> {
        ensure_valid!(self);

        let title_len = wincall_into_result!(GetWindowTextLengthW(self.handle()))?;
        ensure!(
            title_len >= 0,
            "Unexpected error, window title length is negative: {title_len}"
        );
        if title_len == 0 {
            return Ok(None);
        }

        #[allow(clippy::cast_sign_loss)]
        let title_len = title_len as usize;

        let mut title = vec![0u16; title_len + 1];

        let title_len_read = wincall_into_result!(GetWindowTextW(self.handle(), &mut title))?;
        ensure!(
            title_len_read != 0,
            "Expected reading title of length {} but read 0",
            title.len()
        );

        let title = unsafe { windows_strings::PCWSTR::from_raw(title.as_ptr()).to_string() }?;

        Ok(Some(title))
    }

    pub fn process_id(self) -> anyhow::Result<u32> {
        ensure_valid!(self);
        let mut process_id = 0;

        let _ = wincall_into_result!(GetWindowThreadProcessId(
            self.handle(),
            Some(&raw mut process_id)
        ))?;

        Ok(process_id)
    }

    pub fn process_name(self) -> anyhow::Result<String> {
        ensure_valid!(self);
        let process_id = self.process_id()?;
        let process = wincall_result!(OpenProcess(
            PROCESS_QUERY_INFORMATION | PROCESS_VM_READ,
            false,
            process_id,
        ))?;
        // RAII guard: this fn is called from every snapshot's filter pass
        // (per-window per-tick) plus logging and API state publish. Without
        // a close on every path winri leaked ~100 process handles/sec.
        let _guard = ProcessHandleGuard(process);

        let mut process_name = vec![0u16; 256];

        let _ = wincall_into_result!(GetModuleFileNameExW(Some(process), None, &mut process_name))?;

        let process_file_path =
            unsafe { windows_strings::PCWSTR::from_raw(process_name.as_ptr()).to_string() }?;
        let process_name = process_file_path
            .split('\\')
            .next_back()
            .unwrap()
            .to_string();
        Ok(process_name)
    }

    /// Full path to the window's owning executable (e.g.
    /// `C:\Program Files\Foo\bar.exe`). Returned with forward slashes
    /// normalised so config-time path matching doesn't have to care
    /// about escaping. Used by `config.app_overrides` so Electron-based
    /// apps that all report `process_name = "electron.exe"` can be
    /// disambiguated by where their bundle lives on disk.
    pub fn exe_path(self) -> anyhow::Result<String> {
        ensure_valid!(self);
        let process_id = self.process_id()?;
        let process = wincall_result!(OpenProcess(
            PROCESS_QUERY_INFORMATION | PROCESS_VM_READ,
            false,
            process_id,
        ))?;
        let _guard = ProcessHandleGuard(process);

        let mut buf = vec![0u16; 1024];
        let _ = wincall_into_result!(GetModuleFileNameExW(Some(process), None, &mut buf))?;
        let path =
            unsafe { windows_strings::PCWSTR::from_raw(buf.as_ptr()).to_string() }?;
        Ok(path.replace('\\', "/"))
    }

    pub fn class(self) -> anyhow::Result<String> {
        ensure_valid!(self);
        let mut class = vec![0u16; 256];

        let _ = wincall_into_result!(GetClassNameW(self.handle(), &mut class))?;
        let class = unsafe { windows_strings::PCWSTR::from_raw(class.as_ptr()).to_string() }?;

        Ok(class)
    }

    pub fn is_visible(self) -> anyhow::Result<bool> {
        ensure_valid!(self);
        wincall_into_result!(IsWindowVisible(self.handle()).as_bool())
    }

    pub fn is_cloaked(self) -> anyhow::Result<bool> {
        ensure_valid!(self);
        let mut is_cloaked = BOOL::default();
        self.get_dm_attribute(DWMWA_CLOAKED, &mut is_cloaked)?;
        Ok(is_cloaked.as_bool())
    }

    pub fn ancestor(self) -> anyhow::Result<Self> {
        ensure_valid!(self);
        let ancestor = wincall_into_result!(GetAncestor(self.handle(), GA_ROOT))?;
        Self::from_hwnd(ancestor)
    }

    pub fn is_ancestor(self) -> anyhow::Result<bool> {
        Ok(self == self.ancestor()?)
    }

    pub fn move_to(self, pos: Position, size: Size) -> anyhow::Result<()> {
        ensure_valid!(self);
        let [left, top, right, bottom] = self.padding()?;

        let x = pos.x() - left;
        let y = pos.y() - top;
        let w = size.width() + right + left;
        let h = size.height() + bottom + top;

        let _ = wincall_into_result!(ShowWindow(self.handle(), SW_RESTORE))?;
        wincall_result!(MoveWindow(
            self.handle(),
            x as i32,
            y as i32,
            w as i32,
            h as i32,
            true
        ))?;
        Ok(())
    }

    /// DWM-frame-padded target rect for this window if it were placed at
    /// `(pos, size)`. Pulled out of `move_to` so the batched-move path can
    /// use the same math without re-doing it.
    pub fn padded_rect(self, pos: Position, size: Size) -> anyhow::Result<(i32, i32, i32, i32)> {
        ensure_valid!(self);
        let [left, top, right, bottom] = self.padding()?;
        Ok((
            (pos.x() - left) as i32,
            (pos.y() - top) as i32,
            (size.width() + right + left) as i32,
            (size.height() + bottom + top) as i32,
        ))
    }

    pub fn close(self) -> anyhow::Result<()> {
        ensure_valid!(self);
        wincall_result!(PostMessageW(
            Some(self.handle()),
            WM_CLOSE,
            WPARAM::default(),
            LPARAM::default()
        ))?;
        Ok(())
    }

    /// Apply a rectangular clipping region to the window in window-relative
    /// coordinates. Pixels outside this rect are not rendered, but the
    /// window keeps its full size from the app's perspective. Use to keep
    /// the tile strip visually contained within one monitor without
    /// resizing windows.
    ///
    /// Calls `SetWindowRgn`; ownership of the GDI region handle transfers
    /// to the OS, so we do not delete it ourselves.
    pub fn set_visible_region(
        self,
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
    ) -> anyhow::Result<()> {
        ensure_valid!(self);
        // `SetWindowRgn` synchronously dispatches `WM_WINDOWPOSCHANGING`
        // to the target and waits — a hung target app's stuck UI thread
        // would block the tiler's main thread until DWM ghosts it.
        // Skip the clip on hung windows; the next layout pass picks
        // them back up once they unwedge. See `Window::is_hung`.
        if self.is_hung() {
            return Err(anyhow::anyhow!("target window is hung"));
        }
        // Slow-but-not-ghosted defence (TouchDesigner et al.): probe
        // with a `WM_NULL` round-trip; if it doesn't return within
        // the budget, the target's UI thread is busy and the
        // `SetWindowRgn` call below would stall on it for seconds.
        // See `Window::is_responsive`.
        if !self.is_responsive(SETWINDOWRGN_PROBE_MS) {
            return Err(anyhow::anyhow!(
                "target window did not respond within {}ms",
                SETWINDOWRGN_PROBE_MS
            ));
        }
        use windows::Win32::Graphics::Gdi::{CreateRectRgn, DeleteObject, HGDIOBJ, SetWindowRgn};
        let hrgn = unsafe { CreateRectRgn(left, top, right, bottom) };
        if hrgn.is_invalid() {
            return Err(anyhow::anyhow!("CreateRectRgn returned NULL"));
        }
        // SetWindowRgn transfers ownership of `hrgn` to the OS only on
        // success (non-zero return). On failure (zero return) the OS does
        // NOT take it and we must DeleteObject ourselves or we leak a
        // GDI handle per failed call.
        let result = unsafe { SetWindowRgn(self.handle(), Some(hrgn), true) };
        if result == 0 {
            let _ = unsafe { DeleteObject(HGDIOBJ(hrgn.0)) };
            return Err(anyhow::anyhow!("SetWindowRgn returned 0"));
        }
        Ok(())
    }

    /// Clear any previously-applied clipping region — the whole window is
    /// rendered again. Also forces DWM to re-evaluate the non-client area:
    /// `SetWindowRgn` makes DWM drop the modern Aero/acrylic frame, and
    /// `SetWindowRgn(NULL)` alone doesn't always bring it back. The
    /// follow-up `SetWindowPos(SWP_FRAMECHANGED)` call signals "frame
    /// shape changed" which makes DWM rebuild the modern frame.
    pub fn clear_visible_region(self) -> anyhow::Result<()> {
        ensure_valid!(self);
        // Same blocking concern as `set_visible_region`: `SetWindowRgn`
        // is synchronous and a hung target stalls our caller. Skip
        // hung windows — a leftover clip on a hung app is preferable
        // to freezing the whole tiler, and the next non-hung pass
        // will clear it.
        if self.is_hung() {
            return Err(anyhow::anyhow!("target window is hung"));
        }
        if !self.is_responsive(SETWINDOWRGN_PROBE_MS) {
            return Err(anyhow::anyhow!(
                "target window did not respond within {}ms",
                SETWINDOWRGN_PROBE_MS
            ));
        }
        use windows::Win32::{
            Graphics::Gdi::SetWindowRgn,
            UI::WindowsAndMessaging::{
                SWP_ASYNCWINDOWPOS, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
                SWP_NOZORDER, SetWindowPos,
            },
        };
        let _ = unsafe { SetWindowRgn(self.handle(), None, true) };
        // `SWP_ASYNCWINDOWPOS` decouples the frame-changed notification
        // from the target's UI-thread cadence. Without it, clearing 14
        // window clips simultaneously at scroll-animation start
        // serializes 14 round-trips through each app's main thread
        // (`SetWindowPos` blocks until the target processes the message).
        // For Chrome with a busy GPU compositor, that backpressure can
        // wedge the swap chain and leave the window blank until
        // recreated.
        let _ = unsafe {
            SetWindowPos(
                self.handle(),
                None,
                0,
                0,
                0,
                0,
                SWP_NOMOVE
                    | SWP_NOSIZE
                    | SWP_NOZORDER
                    | SWP_NOACTIVATE
                    | SWP_FRAMECHANGED
                    | SWP_ASYNCWINDOWPOS,
            )
        };
        Ok(())
    }

    /// Best-effort "wake up the renderer" sequence for a window that's
    /// stuck blank — typically a Chromium app whose GPU compositor
    /// stopped producing frames after a flurry of SetWindowPos /
    /// SetWindowRgn churn during scroll or overview transitions. Does:
    ///
    /// 1. `SetWindowRgn(NULL)` — clears any lingering clip region.
    /// 2. `SetWindowPos(SWP_FRAMECHANGED)` — forces the target to
    ///    re-evaluate its non-client area; for Chromium that usually
    ///    triggers a swap-chain refresh.
    /// 3. `RedrawWindow(RDW_INVALIDATE | RDW_FRAME | RDW_UPDATENOW |
    ///    RDW_ALLCHILDREN)` — invalidates and synchronously repaints
    ///    the window and all children.
    ///
    /// Exposed via `POST /windows/<id>/wake` so external clients can
    /// offer a one-click "unstick this window" affordance.
    pub fn force_repaint(self) -> anyhow::Result<()> {
        ensure_valid!(self);
        use windows::Win32::{
            Graphics::Gdi::{
                InvalidateRect, RDW_ALLCHILDREN, RDW_FRAME, RDW_INVALIDATE, RDW_UPDATENOW,
                RedrawWindow, SetWindowRgn,
            },
            UI::WindowsAndMessaging::{
                SWP_ASYNCWINDOWPOS, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOZORDER,
                SetWindowPos,
            },
        };
        let _ = unsafe { SetWindowRgn(self.handle(), None, true) };
        let _ = unsafe {
            SetWindowPos(
                self.handle(),
                None,
                0,
                0,
                0,
                0,
                SWP_NOMOVE
                    | SWP_NOSIZE
                    | SWP_NOZORDER
                    | SWP_NOACTIVATE
                    | SWP_FRAMECHANGED
                    | SWP_ASYNCWINDOWPOS,
            )
        };
        let _ = unsafe { InvalidateRect(Some(self.handle()), None, true) };
        let _ = unsafe {
            RedrawWindow(
                Some(self.handle()),
                None,
                None,
                RDW_INVALIDATE | RDW_FRAME | RDW_UPDATENOW | RDW_ALLCHILDREN,
            )
        };
        Ok(())
    }

    /// Move the window so it's centered in the given monitor's work area
    /// at a comfortable default size (~80% of the work area). Used by the
    /// overview context menu's "Move to monitor N".
    pub fn move_to_monitor(self, monitor: &crate::monitor::Monitor) -> anyhow::Result<()> {
        ensure_valid!(self);
        let wa = monitor.work_area;
        #[allow(clippy::cast_precision_loss)]
        let w_w = (wa.right - wa.left) as f32;
        #[allow(clippy::cast_precision_loss)]
        let w_h = (wa.bottom - wa.top) as f32;

        // Default size: 80% of the work area, capped at a sane max.
        let target_w = (w_w * 0.8).min(1600.0);
        let target_h = (w_h * 0.8).min(1200.0);
        #[allow(clippy::cast_precision_loss)]
        let target_x = wa.left as f32 + (w_w - target_w) / 2.0;
        #[allow(clippy::cast_precision_loss)]
        let target_y = wa.top as f32 + (w_h - target_h) / 2.0;

        self.move_to(
            crate::utils::math::Position([target_x, target_y]),
            crate::utils::math::Size([target_w, target_h]),
        )
    }

    pub fn move_offscreen(self) -> anyhow::Result<()> {
        const ADDITIONAL_OFFSCREEN_OFFSET: f32 = 100.0;

        ensure_valid!(self);
        let width = self.desktop_manager_bounds()?.size().width();

        let offscreen_offset = width + ADDITIONAL_OFFSCREEN_OFFSET;

        wincall_result!(SetWindowPos(
            self.handle(),
            None,
            -offscreen_offset as i32,
            0,
            0,
            0,
            SWP_NOSIZE
        ))?;
        Ok(())
    }

    pub fn inner_bounds(self) -> anyhow::Result<Bounds> {
        ensure_valid!(self);
        let mut rect = RECT::default();
        wincall_result!(GetClientRect(self.handle(), &raw mut rect))?;
        Ok(rect.into())
    }

    pub fn desktop_manager_bounds(self) -> anyhow::Result<Bounds> {
        ensure_valid!(self);
        let mut rect = RECT::default();
        self.get_dm_attribute(DWMWA_EXTENDED_FRAME_BOUNDS, &mut rect)?;
        Ok(rect.into())
    }

    pub fn outer_bounds(self) -> anyhow::Result<Bounds> {
        ensure_valid!(self);
        let mut rect = RECT::default();
        wincall_result!(GetWindowRect(self.handle(), &raw mut rect))?;
        Ok(rect.into())
    }

    pub fn padding(self) -> anyhow::Result<[f32; 4]> {
        ensure_valid!(self);
        let dm_rect = self.desktop_manager_bounds()?;
        let rect = self.outer_bounds()?;
        Ok([
            (rect.left - dm_rect.left).abs(),
            (rect.top - dm_rect.top).abs(),
            (rect.right - dm_rect.right).abs(),
            (rect.bottom - dm_rect.bottom).abs(),
        ])
    }

    pub fn is_focused(self) -> anyhow::Result<bool> {
        ensure_valid!(self);
        Ok(Self::focused()? == self)
    }

    pub fn focus(self) -> anyhow::Result<()> {
        ensure_valid!(self);

        if self.is_focused().unwrap_or(false) {
            return Ok(());
        }

        if wincall_into_result!(IsIconic(self.handle()))?.as_bool() {
            let _ = wincall_into_result!(ShowWindow(self.handle(), SW_RESTORE))?;
            thread::sleep(Duration::from_millis(500));
        }

        // HACK: Simulate an alt key release to bypass focus stealing restrictions:
        // https://stackoverflow.com/questions/10740346/setforegroundwindow-only-working-while-visual-studio-is-open
        rdev::simulate(&rdev::EventType::KeyRelease(rdev::Key::Alt))?;
        let _ = wincall_into_result!(SetForegroundWindow(self.handle()))?;
        Ok(())
    }

    #[must_use]
    pub fn get_formatted_extensive_info(self) -> String {
        use std::fmt::Write as _;

        let handle = self.handle();
        let is_valid = self.is_valid();
        let title = self.title();
        let process_id = self.process_id();
        let process_name = self.process_name();
        let class = self.class();
        let is_visible = self.is_visible();
        let is_cloaked = self.is_cloaked();
        let ancestor = self.ancestor();
        let is_ancestor = self.is_ancestor();
        let outer_bounds = self.outer_bounds();
        let inner_bounds = self.inner_bounds();
        let desktop_manager_bounds = self.desktop_manager_bounds();
        let padding = self.padding();
        let is_focused = self.is_focused();
        let is_dialog = self.is_dialog();

        let mut res = String::new();

        let _ = write!(res, "Window {handle:?} info:");

        macro_rules! push {
            ($var:tt) => {
                let _ = write!(res, "\n\t{}: {:?}", stringify!($var), $var);
            };
        }

        push!(is_valid);
        push!(title);
        push!(process_id);
        push!(process_name);
        push!(class);
        push!(is_visible);
        push!(is_cloaked);
        push!(ancestor);
        push!(is_ancestor);
        push!(outer_bounds);
        push!(inner_bounds);
        push!(desktop_manager_bounds);
        push!(padding);
        push!(is_focused);
        push!(is_dialog);

        res
    }
}
