//! Global window-event hook. `SetWinEventHook` fires per
//! `EVENT_OBJECT_LOCATIONCHANGE` from every top-level window on the
//! system; we coalesce that firehose down to one `GlobalMessage::Window`
//! per [`WINDOW_HOOK_COOLDOWN`] and forward it to the main loop.
//!
//! ## Threading model
//!
//! Two threads, both lifetime-of-process:
//! - **`win-event-hook-loop`**: runs `GetMessageW` in a tight loop so
//!   `SetWinEventHook`'s out-of-context callback fires on this thread.
//!   The callback is a hot path (it runs from every other process'
//!   move/resize), so we keep its body branchless and lock-free where
//!   we can.
//! - **`window-hook-coalescer`**: single dedicated thread that owns
//!   the cooldown timing. Wakes on a `Condvar` when a trailing-edge
//!   tick is pending and sleeps for the remaining cooldown before
//!   firing. **One thread, total, for the whole process.**
//!
//! Earlier versions spawned a fresh OS thread per debounced fire (see
//! commit log for the crash this caused: 32-hour session exhausted the
//! Windows commit limit with ~1 MB stack reservations and panicked on
//! `thread::Builder::spawn().unwrap()`). The coalescer below replaces
//! that with bounded resource use.

use std::{
    ptr::null_mut,
    sync::{Condvar, Mutex, OnceLock},
    thread,
    time::{Duration, Instant},
};

use iced::futures::channel::mpsc::Sender;
use windows::Win32::UI::{
    Accessibility::{SetWinEventHook, UnhookWinEvent},
    WindowsAndMessaging::{
        EVENT_OBJECT_CREATE, EVENT_OBJECT_LOCATIONCHANGE, GetMessageW, WINEVENT_OUTOFCONTEXT,
        WINEVENT_SKIPOWNPROCESS,
    },
};

use crate::app::subscription::global::GlobalMessage;

/// Minimum interval between consecutive `GlobalMessage::Window` deliveries.
/// We always fire the leading edge of a burst immediately; subsequent
/// events that arrive within the cooldown coalesce into a single
/// trailing fire at the end of the cooldown window.
///
/// Raised from 200ms to 500ms: the leading edge still catches anything
/// the user *initiated* with no delay (new window, focus change, drag
/// start), so user-visible latency is unchanged. The trailing edge
/// gates only the post-event re-sync — which is bounded by app-side
/// chatter from things like Chromium's per-frame `LOCATIONCHANGE`
/// storms. Cutting that rate 2.5x means ~60% less ambient snapshot
/// work without anything feeling laggier in practice.
const WINDOW_HOOK_COOLDOWN: Duration = Duration::from_millis(500);

struct CoalescerState {
    /// `Some(Sender)` once `launch()` has run; `None` before that. The
    /// hook callback can fire before `launch()` completes on some
    /// machines (rare) so we tolerate it.
    tx: Option<Sender<GlobalMessage>>,
    /// `Instant` of the most recent successful fire. Used to compute
    /// the remaining cooldown.
    last_fire: Instant,
    /// `true` when a hook event arrived during cooldown and we owe the
    /// main loop a trailing tick once cooldown elapses. Set by the hook
    /// callback (via `request_tick`), cleared when the timer thread
    /// actually fires.
    trailing_pending: bool,
}

static STATE: OnceLock<Mutex<CoalescerState>> = OnceLock::new();
static COND: OnceLock<Condvar> = OnceLock::new();

fn state() -> &'static Mutex<CoalescerState> {
    STATE.get_or_init(|| {
        Mutex::new(CoalescerState {
            tx: None,
            last_fire: Instant::now() - WINDOW_HOOK_COOLDOWN,
            trailing_pending: false,
        })
    })
}

fn cond() -> &'static Condvar {
    COND.get_or_init(Condvar::new)
}

/// Called from the WinEvent callback. Either fires the leading edge of
/// a burst immediately, or marks a trailing fire pending and wakes the
/// coalescer thread. Lock-only; no syscalls (other than the channel
/// `try_send` which is non-blocking).
fn request_tick() {
    let Some(mutex) = STATE.get() else {
        return;
    };
    let Ok(mut s) = mutex.lock() else {
        return;
    };
    if s.last_fire.elapsed() >= WINDOW_HOOK_COOLDOWN {
        // Leading edge: fire now. Clear any pending trailing-fire flag
        // since the work that would deliver is now redundant.
        s.last_fire = Instant::now();
        s.trailing_pending = false;
        if let Some(tx) = s.tx.as_mut() {
            let _ = tx.try_send(GlobalMessage::Window);
        }
    } else {
        // Trailing edge: coalesce. Multiple hook events during the
        // cooldown set the same flag and wake the same condvar — the
        // timer thread fires once when the cooldown ends.
        s.trailing_pending = true;
        cond().notify_one();
    }
}

/// Coalescer thread main loop. One per process. Waits on the condvar
/// for a trailing-edge request, then sleeps the remaining cooldown
/// before firing. No spawning per request.
fn run_coalescer() {
    let mutex = state();
    loop {
        let mut s = match mutex.lock() {
            Ok(s) => s,
            Err(_) => return, // mutex permanently poisoned — give up
        };

        // Wait until something is pending.
        while !s.trailing_pending {
            s = match cond().wait(s) {
                Ok(s) => s,
                Err(_) => return,
            };
        }

        // Honour the remaining cooldown before firing.
        let wait = WINDOW_HOOK_COOLDOWN.saturating_sub(s.last_fire.elapsed());
        if wait.is_zero() {
            s.trailing_pending = false;
            s.last_fire = Instant::now();
            if let Some(tx) = s.tx.as_mut() {
                let _ = tx.try_send(GlobalMessage::Window);
            }
        } else {
            // Drop the lock, sleep, loop to re-check. We don't fire
            // here — we go back to the top so any *new* trailing
            // events that arrive during the sleep are picked up by the
            // single fire after we wake.
            drop(s);
            thread::sleep(wait);
        }
    }
}

unsafe extern "system" fn win_event_hook_callback(
    _hwineventhook: windows::Win32::UI::Accessibility::HWINEVENTHOOK,
    _event: u32,
    _hwnd: windows::Win32::Foundation::HWND,
    _idobject: i32,
    _idchild: i32,
    _ideventthread: u32,
    _dwmseventtime: u32,
) {
    request_tick();
}

pub fn launch(tx: Sender<GlobalMessage>) {
    // Install the sender so the (already-initialised) state can deliver.
    if let Ok(mut s) = state().lock() {
        s.tx = Some(tx);
    }

    // Single persistent coalescer thread. Detached — lives for the
    // process. Any error logging the spawn failure but continue: the
    // hook itself still works in leading-edge-only mode if the
    // coalescer didn't start.
    if let Err(e) = thread::Builder::new()
        .name("window-hook-coalescer".into())
        .spawn(run_coalescer)
    {
        log::error!("Failed to launch window-hook coalescer thread: {e:?}");
    }

    // The WinEvent loop thread runs forever in `GetMessageW` so the
    // callback can fire. If it ever returns we unhook and bail.
    if let Err(e) = thread::Builder::new()
        .name("win-event-hook-loop".into())
        .spawn(|| unsafe {
            let hook = SetWinEventHook(
                EVENT_OBJECT_CREATE,
                EVENT_OBJECT_LOCATIONCHANGE,
                None,
                Some(win_event_hook_callback),
                0,
                0,
                WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
            );
            // `GetMessageW` with a null hwnd parks the thread waiting for
            // posted messages so the hook callback can fire.
            if !GetMessageW(null_mut(), None, 0, 0).as_bool() {
                let _ = UnhookWinEvent(hook);
            }
        })
    {
        log::error!("Failed to launch win-event-hook-loop thread: {e:?}");
    }
}
