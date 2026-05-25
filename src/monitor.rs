//! Monitor enumeration and identification.
//!
//! `Monitor` is a lightweight snapshot of one display at a point in time —
//! its `hmonitor` handle is *not* persisted across runs (Windows reassigns
//! them on hotplug / DPI changes), so anything user-visible (config, API)
//! should reference monitors by `device_name` instead (e.g. `\\.\DISPLAY1`).


use anyhow::{Context, anyhow};
use windows::Win32::{
    Foundation::{HWND, LPARAM, RECT, TRUE},
    Graphics::Gdi::{
        EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITOR_DEFAULTTONEAREST,
        MONITORINFOEXW, MonitorFromWindow,
    },
};

/// Bit in `MONITORINFO.dwFlags` indicating the primary display. Win32 SDK
/// constant `MONITORINFOF_PRIMARY`; not re-exported by windows-rs 0.62.
const MONITORINFOF_PRIMARY: u32 = 0x0000_0001;

#[derive(Debug, Clone)]
pub struct Monitor {
    /// Raw `HMONITOR` handle as `isize`. Use only for in-session comparisons
    /// and Win32 calls; do not persist.
    pub hmonitor: isize,
    /// Stable device name like `\\.\DISPLAY1`. Use this as the cross-run
    /// identifier in config and the API.
    pub device_name: String,
    /// Work area (excludes the taskbar) in virtual-screen coordinates.
    pub work_area: RECT,
    pub is_primary: bool,
}

impl Monitor {
    pub fn work_area_width(&self) -> i32 {
        self.work_area.right - self.work_area.left
    }
    pub fn work_area_height(&self) -> i32 {
        self.work_area.bottom - self.work_area.top
    }
}

/// Enumerate all attached displays. Safe to call from any thread; cheap
/// enough (~tens of microseconds) to invoke per tiler snapshot.
pub fn enumerate() -> Vec<Monitor> {
    let mut out: Vec<Monitor> = Vec::new();

    unsafe extern "system" fn cb(
        hmonitor: HMONITOR,
        _hdc: HDC,
        _lprc: *mut RECT,
        lparam: LPARAM,
    ) -> windows::core::BOOL {
        let out = unsafe { &mut *(lparam.0 as *mut Vec<Monitor>) };

        let mut info = MONITORINFOEXW::default();
        info.monitorInfo.cbSize =
            u32::try_from(std::mem::size_of::<MONITORINFOEXW>()).unwrap_or(0);

        let ok = unsafe {
            GetMonitorInfoW(
                hmonitor,
                &raw mut info.monitorInfo,
            )
        };
        if !ok.as_bool() {
            // Skip this monitor on introspection failure; keep enumerating.
            return TRUE;
        }

        let device_name = read_wide_nul_terminated(&info.szDevice);
        let is_primary = info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0;

        out.push(Monitor {
            hmonitor: hmonitor.0 as isize,
            device_name,
            work_area: info.monitorInfo.rcWork,
            is_primary,
        });

        TRUE
    }

    unsafe {
        let _ = EnumDisplayMonitors(
            None,
            None,
            Some(cb),
            LPARAM(&raw mut out as isize),
        );
    }

    out
}

pub fn find_by_device_name(name: &str) -> Option<Monitor> {
    enumerate().into_iter().find(|m| m.device_name == name)
}

/// Resolves the user's `tiling_monitor` config string to a concrete
/// monitor. Accepts the literal `"primary"` or a device name. Falls back
/// to the primary if the named monitor isn't currently attached.
pub fn resolve_tiling_monitor(config_value: &str) -> anyhow::Result<Monitor> {
    let monitors = enumerate();
    if monitors.is_empty() {
        return Err(anyhow!("no monitors detected"));
    }

    if config_value.eq_ignore_ascii_case("primary") {
        return monitors
            .into_iter()
            .find(|m| m.is_primary)
            .context("no primary monitor flagged");
    }

    if let Some(m) = monitors.iter().find(|m| m.device_name == config_value) {
        return Ok(m.clone());
    }

    log::warn!(
        "tiling_monitor=\"{config_value}\" not attached; falling back to primary",
    );
    monitors
        .into_iter()
        .find(|m| m.is_primary)
        .context("no primary monitor and no match for configured device_name")
}

/// `MonitorFromWindow(MONITOR_DEFAULTTONEAREST)` — returns the `HMONITOR`
/// most closely containing the given window. Returns the raw handle as
/// `isize` for cross-thread storage.
pub fn monitor_of_hwnd(hwnd: HWND) -> isize {
    unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST).0 as isize }
}

fn read_wide_nul_terminated(buf: &[u16]) -> String {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}
