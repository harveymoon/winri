#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")] // hide console window on Windows in release builds
#![warn(clippy::pedantic, clippy::nursery, clippy::dbg_macro)]
#![allow(
    clippy::missing_errors_doc,
    clippy::cast_possible_truncation,
    clippy::missing_const_for_fn,
    clippy::needless_pass_by_value,
    clippy::option_if_let_else,
    clippy::default_trait_access
)]

use std::{panic, path::PathBuf};

use anyhow::{Context, anyhow};
use windows::{
    Win32::{
        Foundation::{ERROR_ALREADY_EXISTS, GetLastError},
        System::Threading::CreateMutexW,
        UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext},
    },
    core::w,
};

mod adapter;
mod api;
mod app;
mod bug_report;
mod config;
mod icon;
mod logger;
mod monitor;
mod scroll_tiler;
mod system;
mod tray;
mod utils;
mod winapi;
mod window;

pub const DEBUG_MODE: bool = cfg!(debug_assertions);
pub const WINRI_VERSION: &str = env!("CARGO_PKG_VERSION");

pub fn root_dir() -> anyhow::Result<PathBuf> {
    const PROJECT_DIR_NAME: &str = if DEBUG_MODE { "winri-dev" } else { "winri" };
    Ok(dirs::config_dir()
        .ok_or_else(|| anyhow!("Could not determine config directory"))?
        .join(PROJECT_DIR_NAME))
}

/// Acquire a named mutex so only one winri can run per user session.
/// Returns `true` if we are the first instance; `false` if another winri
/// already holds the mutex.
fn acquire_single_instance() -> bool {
    // `Local\` prefix scopes the mutex to the current Terminal Services
    // session, which is exactly what we want for a per-user tiler — running
    // winri across multiple logged-in user sessions shouldn't conflict.
    let handle = unsafe { CreateMutexW(None, true, w!("Local\\WinriSingleInstance")) };
    if handle.is_err() {
        // Couldn't even create the mutex — let winri continue rather than
        // false-positive-blocking.
        return true;
    }
    let already_exists = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
    // Deliberately leak the handle: we want it held for the lifetime of the
    // process so the kernel releases it when winri exits.
    if already_exists {
        // Drop the handle so we don't accidentally hold a reference that
        // confuses the existing instance.
        if let Ok(h) = handle {
            let _ = unsafe { windows::Win32::Foundation::CloseHandle(h) };
        }
        false
    } else {
        std::mem::forget(handle);
        true
    }
}

fn main() {
    // Per-monitor V2 DPI awareness so winri talks in physical pixels and
    // can place windows correctly on secondary monitors with different
    // scaling factors. Must be set before any window is created.
    let _ = unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };

    if !acquire_single_instance() {
        crate::bug_report::display_and_exit(anyhow!(
            "Another winri is already running. Exit it (Win+Esc) before launching again."
        ));
        std::process::exit(0);
    }

    let default_hook = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        log::error!("Winri panicked: {info}");
        bug_report::display_and_exit(info);
        system::restore_windows();
        default_hook(info);
        std::process::exit(1);
    }));

    if let Err(e) = logger::setup()
        .context("Could not initialize log system, no log will be written for this session")
    {
        bug_report::display_and_continue(e);
    }

    log::info!("Winri starting up");

    // Defensive cleanup: a previous winri may have been force-killed while
    // a window had a SetWindowRgn clip applied, leaving it rendering as
    // a blank rectangle. Clear any such regions before doing anything else.
    system::clear_all_window_clips();

    if let Err(e) = config::init() {
        log::warn!("Failed to load user config — falling back to defaults: {e:#}");
    }

    // Install the tray icon before iced takes over the main thread.
    tray::launch();

    if let Err(e) = iced::daemon(
        app::State::new,
        app::State::handle_app_message,
        app::State::view,
    )
    .subscription(app::State::subscription)
    .title(app::State::title)
    .theme(app::State::theme)
    .run()
    {
        log::error!("Winri exited with error: {e}");
        bug_report::display_and_exit(anyhow!(e));
    }

    log::info!("Winri exited successfully");
}
