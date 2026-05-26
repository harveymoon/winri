//! Public-only virtual-desktop introspection.
//!
//! Wraps Windows' stable `IVirtualDesktopManager` COM interface. We
//! deliberately do *not* touch `IVirtualDesktopManagerInternal` —
//! that one is undocumented, its ABI shifts between Windows builds,
//! and depending on it is exactly the kind of fragility callers want
//! to drop (pyvda, etc.).
//!
//! ## Indexing semantics
//!
//! Windows' public API exposes a window's desktop **GUID** but NOT its
//! position in the user's Task View ordering. To turn the GUID into a
//! small integer we keep a session-local `Vec<GUID>` of desktops we've
//! seen, first-sighting first; the number returned is `index + 1`.
//!
//! Guarantees:
//! - Two windows on the same desktop always share the same number.
//! - Numbers are stable for the lifetime of the winri process.
//!
//! Non-guarantees:
//! - Order does **not** necessarily match Win+Tab numbering. Matching it
//!   would require `IVirtualDesktopManagerInternal`. Practically, when
//!   winri starts on desktop 1 with most windows there, desktop 1 will
//!   be `desktop_id = 1`; other desktops get 2, 3, ... in the order they
//!   first appear in a `/state` poll.
//!
//! ## Threading
//!
//! `IVirtualDesktopManager` isn't `Sync` so we cache it in a `thread_local`.
//! All call sites (`publish_api_snapshot`) run on the iced main thread,
//! so a single cached pointer is enough; cross-thread callers would
//! initialise a fresh instance per thread (still cheap, no COM init
//! needed because the apartment is already set up).

use std::{
    cell::RefCell,
    collections::HashMap,
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

use windows::Win32::{
    Foundation::HWND,
    System::Com::{CLSCTX_ALL, CoCreateInstance},
    UI::Shell::{IVirtualDesktopManager, VirtualDesktopManager},
};
use windows::core::GUID;

thread_local! {
    /// Outer `Option`: "have we tried to create the manager yet?".
    /// Inner `Option`: "did the creation succeed?".
    static MANAGER: RefCell<Option<Option<IVirtualDesktopManager>>> = const { RefCell::new(None) };
    /// Per-HWND cache of `GetWindowDesktopId` results to skip the
    /// COM-marshalled round-trip to dwm.exe on hot snapshot paths.
    /// Entries are valid for [`HWND_GUID_TTL`].
    static HWND_GUID_CACHE: RefCell<HashMap<u64, (GUID, Instant)>> =
        RefCell::new(HashMap::new());
}

/// How long a cached `GetWindowDesktopId` result is trusted. Chosen to
/// stay below the typical snapshot interval (~200ms) so almost every
/// publish hits cache, while still picking up a real desktop change
/// within a beat. A window genuinely moves between desktops only on
/// explicit user action (Task View drag etc.).
const HWND_GUID_TTL: Duration = Duration::from_millis(800);

fn with_manager<F, R>(f: F) -> Option<R>
where
    F: FnOnce(&IVirtualDesktopManager) -> Option<R>,
{
    MANAGER.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            *slot = Some(unsafe {
                CoCreateInstance::<_, IVirtualDesktopManager>(
                    &VirtualDesktopManager,
                    None,
                    CLSCTX_ALL,
                )
                .ok()
            });
        }
        let mgr = slot.as_ref()?.as_ref()?;
        f(mgr)
    })
}

fn registry() -> &'static Mutex<Vec<GUID>> {
    static REG: OnceLock<Mutex<Vec<GUID>>> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(Vec::new()))
}

/// 1-indexed desktop number for the given HWND.
///
/// Returns `None` when the OS doesn't have the window in any virtual
/// desktop (some shell / system windows). Indexing is by first-sighting
/// within the session — see the module docs.
///
/// Performance: result is cached per HWND with [`HWND_GUID_TTL`].
/// `GetWindowDesktopId` is a cross-apartment COM call into dwm.exe;
/// caching collapses ~14 calls/snapshot to ~14 calls/second.
pub fn desktop_id_for(hwnd_raw: u64) -> Option<u32> {
    let now = Instant::now();

    let cached = HWND_GUID_CACHE.with(|cache| {
        cache
            .borrow()
            .get(&hwnd_raw)
            .filter(|(_, ts)| now.duration_since(*ts) < HWND_GUID_TTL)
            .map(|(g, _)| *g)
    });

    let guid = match cached {
        Some(g) => g,
        None => {
            let hwnd = HWND(hwnd_raw as *mut std::ffi::c_void);
            let fresh = with_manager(|mgr| unsafe { mgr.GetWindowDesktopId(hwnd) }.ok())?;
            HWND_GUID_CACHE.with(|cache| {
                cache.borrow_mut().insert(hwnd_raw, (fresh, now));
            });
            fresh
        }
    };

    if guid == GUID::zeroed() {
        return None;
    }

    let mut list = registry().lock().ok()?;
    if let Some(idx) = list.iter().position(|g| *g == guid) {
        u32::try_from(idx + 1).ok()
    } else {
        list.push(guid);
        u32::try_from(list.len()).ok()
    }
}
