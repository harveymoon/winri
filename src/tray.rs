//! Tray icon for winri.
//!
//! Provides a small notification-area icon with a right-click menu so users
//! can open settings or quit winri without remembering the keybindings. The
//! `tray-icon` crate handles the Win32 plumbing (`Shell_NotifyIconW`, popup
//! menu, message pump). We bridge its event receivers to the same message
//! channel the HTTP API uses, so menu clicks dispatch into the iced app
//! identically to any other external command.

use std::{thread, time::Duration};

use tray_icon::{
    Icon, TrayIconBuilder,
    menu::{Menu, MenuEvent, MenuItem},
};

use crate::{
    api,
    app::{Message, action},
};

/// 16×16 RGBA icon generated procedurally: a thin horizontal bar suggesting
/// the tile strip. Avoids shipping a binary asset.
fn build_icon() -> Icon {
    const W: u32 = 16;
    const H: u32 = 16;
    let mut rgba = vec![0u8; (W * H * 4) as usize];
    for y in 0..H {
        for x in 0..W {
            let idx = ((y * W + x) * 4) as usize;
            // Background = transparent.
            rgba[idx] = 0;
            rgba[idx + 1] = 0;
            rgba[idx + 2] = 0;
            rgba[idx + 3] = 0;
            // Strip: horizontal band in the middle, accent-blue.
            if y >= 6 && y <= 9 && x >= 2 && x <= 13 {
                rgba[idx] = 76;
                rgba[idx + 1] = 198;
                rgba[idx + 2] = 255;
                rgba[idx + 3] = 255;
            }
        }
    }
    Icon::from_rgba(rgba, W, H).expect("static rgba should be valid")
}

/// Install the tray icon and pump its menu events on a background thread.
/// Menu actions are converted to `Message` and pushed into the shared API
/// command channel (so they queue cleanly until the iced subscription
/// drains them).
pub fn launch() {
    // tray-icon must be created on the main thread on Windows for proper
    // window-message integration. iced::daemon runs on the main thread,
    // so we create the tray right before `iced::daemon::run()` in main.rs.
    // This function is therefore called from main, not a worker thread.

    let menu = Menu::new();
    let item_settings = MenuItem::new("Open settings (Win+,)", true, None);
    let item_overview = MenuItem::new("Open overview (Win+↑)", true, None);
    let item_center = MenuItem::new("Center focused (Win+H)", true, None);
    let sep = tray_icon::menu::PredefinedMenuItem::separator();
    let item_quit = MenuItem::new("Quit winri (Win+Esc)", true, None);
    menu.append_items(&[
        &item_settings,
        &item_overview,
        &item_center,
        &sep,
        &item_quit,
    ])
    .ok();

    let id_settings = item_settings.id().clone();
    let id_overview = item_overview.id().clone();
    let id_center = item_center.id().clone();
    let id_quit = item_quit.id().clone();

    let _tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("winri")
        .with_icon(build_icon())
        .build();

    // Pump menu events in a worker thread. Retry-send into the API channel
    // so events queued before the iced subscription drains start cleanly
    // arrive at the same place HTTP commands do.
    let receiver = MenuEvent::receiver();
    thread::Builder::new()
        .name("winri-tray".into())
        .spawn(move || {
            while let Ok(event) = receiver.recv() {
                let msg = if event.id == id_settings {
                    Some(Message::Action(action::Action::OpenSettings))
                } else if event.id == id_overview {
                    Some(Message::Action(action::Action::Tiler(
                        action::TilerAction::OpenOverview,
                    )))
                } else if event.id == id_center {
                    Some(Message::Action(action::Action::Tiler(
                        action::TilerAction::CenterFocused,
                    )))
                } else if event.id == id_quit {
                    Some(Message::Action(action::Action::Exit))
                } else {
                    None
                };
                if let Some(m) = msg {
                    // The channel might not be wired up yet at startup;
                    // retry briefly so we don't drop the very first click.
                    for _ in 0..10 {
                        if api::send_message(m.clone()).is_ok() {
                            break;
                        }
                        thread::sleep(Duration::from_millis(50));
                    }
                }
            }
        })
        .ok();

    // Intentionally leak the tray icon: tray-icon's API requires the
    // TrayIcon to outlive its menu; the easiest way to keep both alive
    // for the lifetime of the process is to forget the handle here.
    std::mem::forget(_tray);
}
