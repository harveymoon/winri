/// Filters for windows that should be tiled.
use std::collections::HashSet;

use crate::{config, window::Window};

pub const WINRI_IGNORED_CLASS_NAME: &str = "Winri_IgnoreWindowClass";
pub const WINRI_IGNORED_WINDOW_TITLE_SUBSTRING: &str = "[Winri Ignore Window]";

const IGNORED_CLASSES: &[&str] = &[
    "Progman",
    "TopLevelWindowForOverflowXamlIsland",
    "XamlExplorerHostIslandWindow",
    "Xaml_WindowedPopupClass",
    "Shell_TrayWnd",
    "FindMyMouse",
    // Win11 modern context menus (Explorer right-click and similar WinUI 3 popups)
    // are hosted in a top-level window of this class.
    "Microsoft.UI.Content.PopupWindowSiteBridge",
    WINRI_IGNORED_CLASS_NAME,
];

const IGNORED_PROCESS_NAMES: &[&str] = &[
    "Microsoft.CmdPal.UI.exe",
    "PowerToys.MeasureToolUI.exe",
    "ShareX.exe",
    "SnippingTool.exe",
    "PowerToys.PowerLauncher.exe",
    "Ditto.exe",
];

macro_rules! filter_out_if {
    ($bool:expr) => {
        if $bool {
            return Ok(false);
        }
    };
}

pub fn should_be_tiled(window: Window) -> anyhow::Result<bool> {
    filter_out_if!(!window.is_visible()?);
    filter_out_if!(window.is_cloaked()?);
    filter_out_if!(!window.is_ancestor()?);
    filter_out_if!(window.is_dialog()?);
    let title = window.title()?;
    filter_out_if!(title.is_none());
    filter_out_if!(title.is_some_and(|title| title.contains(WINRI_IGNORED_WINDOW_TITLE_SUBSTRING)));
    let class = window.class()?;
    filter_out_if!(IGNORED_CLASSES.contains(&class.as_str()));
    let process = window.process_name()?;
    filter_out_if!(IGNORED_PROCESS_NAMES.contains(&process.as_str()));

    let user_cfg = config::current();
    filter_out_if!(user_cfg.filter.ignored_classes.iter().any(|c| c == &class));
    filter_out_if!(user_cfg.filter.ignored_processes.iter().any(|p| p == &process));
    // Per-window persistent ignore: (process, title, optional class) match.
    let title_str = window
        .title()
        .ok()
        .flatten()
        .unwrap_or_default();
    filter_out_if!(
        user_cfg
            .filter
            .ignored_window_titles
            .iter()
            .any(|e| e.process == process
                && e.title == title_str
                && e.class.as_deref().map_or(true, |c| c == class.as_str()))
    );
    drop(user_cfg);

    filter_out_if!(!window.is_valid()?);

    Ok(true)
}

/// Enumerate all top-level windows that pass the standard "should be
/// managed" filter. The result is *not* gated by monitor — that decision
/// is made by [`crate::scroll_tiler::ScrollTiler`] when deciding whether
/// to add a new window or to untile an existing one (see the drag-end
/// detection there).
pub fn opened_windows() -> anyhow::Result<HashSet<Window>> {
    let windows = Window::enumerate()?
        .into_iter()
        .filter(|window| should_be_tiled(*window).unwrap_or(false))
        .collect::<HashSet<_>>();

    Ok(windows)
}

/// All visible top-level windows that pass the standard filter, regardless
/// of which monitor they're on. Used by the overview so floating windows on
/// secondary monitors also get thumbnails.
pub fn all_managed_windows() -> anyhow::Result<Vec<Window>> {
    Ok(Window::enumerate()?
        .into_iter()
        .filter(|window| should_be_tiled(*window).unwrap_or(false))
        .collect())
}
