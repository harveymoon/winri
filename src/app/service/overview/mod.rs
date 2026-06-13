//! Overview mode — one fullscreen iced window *per monitor* that shows
//! live DWM thumbnails of the windows located on that monitor.
//!
//! Architecture: on `Win+↑`, winri enumerates monitors and opens one
//! transparent topmost iced window over each. Every window emits its own
//! `OverviewWindowCreated` message; for each one we partition all
//! manageable top-level windows by `Window::monitor()` and register DWM
//! thumbnails into the matching overview window. Mouse events (move,
//! down, up) are delivered to whichever iced window the cursor is over —
//! we look up the matching `MonitorOverview` by `window_id` and operate
//! only on that subset.
//!
//! Cross-monitor drag-to-reorder is intentionally out of scope here;
//! moving windows between monitors goes through the right-click context
//! menu (Phase 4).

mod thumbnail;

use anyhow::Context;
use iced::Task;

use crate::{
    app::{self, Mode, service::overview::thumbnail::ThumbnailId},
    window::Window,
};

pub use thumbnail::ThumbnailRect;

pub struct State {
    /// One entry per monitor in this session.
    pub monitors: Vec<MonitorOverview>,
}

pub struct MonitorOverview {
    pub window_id: iced::window::Id,
    pub hmonitor: isize,
    /// Live DWM thumbnails in display order for this monitor.
    pub thumbnails: Vec<Thumbnail>,
    pub cursor_pos: Option<iced::Point>,
    pub press_pos: Option<iced::Point>,
    pub drag_source_idx: Option<usize>,
    /// Active right-click menu, if any. Cleared on left-click anywhere or
    /// after an action button is pressed.
    pub context_menu: Option<ContextMenu>,
}

/// State for an open right-click menu in an overview. We compute the
/// other-monitor list at open time so the view doesn't have to re-query
/// `crate::monitor::enumerate()` every frame.
#[derive(Debug, Clone)]
pub struct ContextMenu {
    /// Window the menu is acting on.
    pub target: Window,
    /// Anchor point in overview-window-local coords (where the right-click
    /// landed). The view positions the popup at this point.
    pub anchor: iced::Point,
    /// Display name of the source app (e.g. "Chrome"). Used for the
    /// "Ignore <app>" button label.
    pub app_name: String,
    /// `.exe` filename — what gets written to `ignored_processes` if the
    /// user picks "Ignore app".
    pub process: String,
    /// Monitors other than the one this menu is on, sorted by index.
    /// Each tuple is `(device_name, friendly_label)`.
    pub other_monitors: Vec<(String, String)>,
}

impl MonitorOverview {
    fn thumbnail_at(&self, pos: iced::Point) -> Option<usize> {
        self.thumbnails.iter().position(|t| {
            t.rect.contains(f64::from(pos.x), f64::from(pos.y))
        })
    }
}

/// Maximum pixel distance the cursor may move between mouse-down and mouse-up
/// to still count as a click rather than a drag.
pub const CLICK_DRAG_THRESHOLD_PX: f32 = 5.0;

fn process_name_to_app_name(process_name: &str) -> String {
    let stem = process_name
        .strip_suffix(".exe")
        .or_else(|| process_name.strip_suffix(".EXE"))
        .unwrap_or(process_name);
    let mut chars = stem.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect::<String>(),
        None => stem.to_owned(),
    }
}

/// Outcome of a left-mouse-up over an overview window.
#[derive(Debug, Clone, Copy)]
pub enum ReleaseOutcome {
    /// User clicked on this source window — jump to it.
    Click(Window),
    /// User dragged `src` onto `dst` within the same monitor — reorder so
    /// `src` takes `dst`'s slot.
    Reorder { src: Window, dst: Window },
}

pub struct Thumbnail {
    pub thumbnail_id: ThumbnailId,
    pub src: Window,
    pub rect: ThumbnailRect,
    pub app_name: String,
    pub title: String,
    /// App icon as a ready-to-draw iced image handle, if available.
    /// `None` when the window doesn't expose an icon (e.g. some UWP host
    /// windows). Built once at overview entry.
    pub icon: Option<(u32, u32, iced::widget::image::Handle)>,
}

#[derive(Debug, Clone)]
pub enum Message {
    /// Fired once a per-monitor overview window is created and we have its
    /// raw HWND. We register that monitor's thumbnails in response.
    OverviewWindowCreated {
        id: iced::window::Id,
        raw_handle: u64,
        hmonitor: isize,
    },
}

impl app::State {
    pub fn prepare_open_overview(&mut self) -> Task<app::Message> {
        if matches!(self.mode, Mode::Overview(_)) {
            log::warn!(
                "Overview operation requested in {} while already in Overview mode",
                crate::function!()
            );
            return Task::none();
        }

        // Refuse to open overview if the settings panel is already up.
        // This prevents the "queued hotkeys after a freeze" cascade:
        // a frozen app stalls the tiler, the OS buffers Win+, and
        // Win+Up, then both fire back-to-back when the loop unwedges
        // — overview parks every tile offscreen and the settings
        // window sits in front of the (now invisible) overview UI,
        // leaving the user with no on-screen tiles and no obvious
        // way to recover. (May 2026 incident.)
        if self.settings_window_id.is_some() {
            log::warn!(
                "Overview suppressed: settings panel is open. Close settings first."
            );
            return Task::none();
        }

        let monitors = crate::monitor::enumerate();
        if monitors.is_empty() {
            return Task::none();
        }

        // Mark "overview is opening" synchronously, before returning the
        // creation tasks. The async gap between this batch and
        // `finalize_open_overview` flipping `self.mode` is exactly when a
        // queued `Win+,` would otherwise slip past the settings guard.
        // Cleared in every `finalize_open_overview` exit path.
        self.overview_opening = true;

        // One window-creation task per monitor, batched together.
        Task::batch(monitors.into_iter().map(|m| {
            #[allow(clippy::cast_precision_loss)]
            let origin = (m.work_area.left as f32, m.work_area.top as f32);
            #[allow(clippy::cast_precision_loss)]
            let size = (m.work_area_width() as f32, m.work_area_height() as f32);
            thumbnail::open_overview_window(m.hmonitor, origin, size)
        }))
    }

    pub fn handle_overview_message(&mut self, message: Message) -> anyhow::Result<Task<app::Message>> {
        match message {
            Message::OverviewWindowCreated {
                id,
                raw_handle,
                hmonitor,
            } => match self.finalize_open_overview(id, raw_handle, hmonitor) {
                Ok(()) => Ok(Task::none()),
                Err(e) => {
                    // The overview window was opened before finalize ran, so
                    // failing here would orphan it. Close it explicitly so
                    // the user doesn't end up with an empty transparent
                    // window stuck on screen.
                    //
                    // Also drop `overview_opening` so the settings guard
                    // can't get permanently wedged shut by a failed
                    // overview open.
                    log::warn!("Overview finalize failed; closing orphan window: {e:#}");
                    self.overview_opening = false;
                    Ok(iced::window::close::<app::Message>(id))
                }
            },
        }
    }

    fn finalize_open_overview(
        &mut self,
        overview_window_id: iced::window::Id,
        raw_handle: u64,
        hmonitor: isize,
    ) -> anyhow::Result<()> {
        // First overview window creation also flips us into Overview mode
        // and moves the tiled source windows offscreen.
        if !matches!(self.mode, Mode::Overview(_)) {
            for tiled_window in self.tiler.windows() {
                if let Err(e) = tiled_window.inner.move_offscreen() {
                    log::warn!("Failed to move source window offscreen: {e:#}");
                }
            }
            // We just moved every tile to a non-tile position. Drop the
            // tiler's `last_layout` cache so when overview closes the
            // next layout pass actually moves them back — otherwise the
            // diff check sees no change vs the pre-overview rect and
            // short-circuits, leaving the windows parked offscreen
            // until the user scrolls by a pixel.
            self.tiler.invalidate_last_layouts();
            log::info!("switching to Overview mode (per-monitor)");
            self.mode = Mode::Overview(State {
                monitors: Vec::new(),
            });
            // `Mode::Overview` is now the active guard; clear the
            // synchronous opening flag set in `prepare_open_overview`.
            self.overview_opening = false;
        }

        // Find the matching `Monitor` so we can read its work area and
        // figure out which top-level windows live on it.
        let monitor_info = crate::monitor::enumerate()
            .into_iter()
            .find(|m| m.hmonitor == hmonitor)
            .with_context(|| format!("monitor {hmonitor} disappeared during overview open"))?;

        // Decide which windows belong in this monitor's overview.
        //
        // Tiled windows are placed by the tiler at strip-positions that can
        // physically straddle monitor edges (especially when the strip is
        // wider than the primary monitor). Their *logical* home is the
        // tiling monitor regardless of where their pixels currently land,
        // so we route ALL tiled windows into the tiling monitor's overview.
        //
        // Free-floating (non-tiled) windows are placed wherever the user
        // dragged them; we partition those by their actual current monitor.
        let tiling_hmonitor_now = self.tiling_hmonitor();
        // Skip iconic (minimized) and cloaked (other virtual desktop)
        // windows from the overview entirely — they have no on-screen
        // pixels right now and a DWM thumbnail would render as a blank
        // rectangle taking up a slot the user can't act on. Restore the
        // window from the taskbar / Win+Tab if you want to overview it.
        let is_overviewable = |w: &Window| -> bool {
            !w.is_iconic() && !w.is_cloaked().unwrap_or(false)
        };
        let mut on_this_monitor: Vec<Window> = Vec::new();
        if Some(hmonitor) == tiling_hmonitor_now {
            for item in self.tiler.windows() {
                if is_overviewable(&item.inner) {
                    on_this_monitor.push(item.inner);
                }
            }
        }
        if let Ok(all_filtered) = crate::window::filter::all_managed_windows() {
            let tiled: std::collections::HashSet<Window> =
                self.tiler.windows().map(|i| i.inner).collect();
            for w in all_filtered {
                if tiled.contains(&w) {
                    continue;
                }
                if w.monitor() == hmonitor && is_overviewable(&w) {
                    on_this_monitor.push(w);
                }
            }
        }

        // Layout uses tiler-known dimensions if the window is tiled,
        // otherwise the window's actual on-screen size. The grid layout
        // uses both width and height to preserve aspect ratio; the strip
        // layout only consults width and uses a fixed height.
        let tile_height = self
            .tiler
            .screen_size()
            .height()
            .mul_add(1.0, -2.0 * 10.0); // approximate strip thumb height
        let window_data: Vec<thumbnail::WindowData> = on_this_monitor
            .iter()
            .map(|w| {
                if let Some(item) = self.tiler.windows().find(|item| item.inner == *w) {
                    return thumbnail::WindowData {
                        inner: *w,
                        width: item.width,
                        height: tile_height,
                    };
                }
                if let Ok(bounds) = w.desktop_manager_bounds() {
                    return thumbnail::WindowData {
                        inner: *w,
                        width: bounds.size().width(),
                        height: bounds.size().height(),
                    };
                }
                thumbnail::WindowData {
                    inner: *w,
                    width: 800.0,
                    height: 600.0,
                }
            })
            .collect();

        #[allow(clippy::cast_precision_loss)]
        let monitor_size = crate::utils::math::Size([
            monitor_info.work_area_width() as f32,
            monitor_info.work_area_height() as f32,
        ]);
        // Tiling monitor keeps the horizontal-strip overview (preserves
        // drag-reorder semantics). Floating monitors get a wrapping grid
        // that adapts to the monitor's aspect ratio.
        let is_tiling_monitor = Some(hmonitor) == self.tiling_hmonitor();
        let layouts = if is_tiling_monitor {
            thumbnail::compute_thumbnail_layouts(&window_data, monitor_size, 10.0)
        } else {
            thumbnail::compute_grid_layout(&window_data, monitor_size, 16.0)
        };

        let dest_window = Window::from_safe_hwnd(raw_handle)
            .context(raw_handle)
            .context("invalid hwnd from overview window")?;

        let mut thumbnails = Vec::new();
        for (window, layout) in window_data.iter().zip(layouts.into_iter()) {
            match thumbnail::register_thumbnail(window.inner, dest_window, layout.rect) {
                Ok(thumbnail_id) => {
                    let title = window
                        .inner
                        .title()
                        .ok()
                        .flatten()
                        .unwrap_or_default();

                    // Config-driven app override resolved from the
                    // window's full exe path. Used so Electron-frame
                    // apps (which all share `process_name = "electron.exe"`
                    // and report the generic Electron HICON) can present
                    // their own name and bundled icon.
                    let exe_path = window.inner.exe_path().ok();
                    let cfg = crate::config::current();
                    let override_match = exe_path
                        .as_deref()
                        .and_then(|p| cfg.app_overrides.iter().find(|o| o.matches(p)).cloned());
                    drop(cfg);

                    let app_name = if let Some(o) = &override_match {
                        o.display_name.clone()
                    } else {
                        window
                            .inner
                            .process_name()
                            .ok()
                            .map_or_else(String::new, |p| process_name_to_app_name(&p))
                    };

                    // Icon: prefer override-supplied path, fall back to
                    // whatever the window exposes via WM_GETICON.
                    let icon_rgba = override_match
                        .as_ref()
                        .and_then(|o| o.icon_path.as_deref())
                        .and_then(crate::icon::load_icon_rgba_from_path)
                        .or_else(|| {
                            crate::icon::fetch_icon_rgba(window.inner.handle().0 as u64)
                        });
                    let icon = icon_rgba.map(|(w, h, rgba)| {
                        let handle = iced::widget::image::Handle::from_rgba(w, h, rgba);
                        (w, h, handle)
                    });

                    thumbnails.push(Thumbnail {
                        thumbnail_id,
                        src: window.inner,
                        rect: layout.rect,
                        app_name,
                        title,
                        icon,
                    });
                }
                Err(e) => {
                    log::warn!("Failed to register DWM thumbnail: {e:#}");
                }
            }
        }

        log::info!(
            "monitor {} ({}): bound {} thumbnails",
            monitor_info.device_name,
            if monitor_info.is_primary { "primary" } else { "secondary" },
            thumbnails.len(),
        );

        let Mode::Overview(state) = &mut self.mode else {
            unreachable!("set above");
        };
        state.monitors.push(MonitorOverview {
            window_id: overview_window_id,
            hmonitor,
            thumbnails,
            cursor_pos: None,
            press_pos: None,
            drag_source_idx: None,
            context_menu: None,
        });

        self.publish_api_snapshot();
        Ok(())
    }

    pub fn close_overview(&mut self) -> anyhow::Result<Task<app::Message>> {
        let Mode::Overview(state) = &self.mode else {
            log::warn!(
                "Close overview requested in {} while not in Overview mode",
                crate::function!()
            );
            return Ok(Task::none());
        };

        let mut close_tasks: Vec<Task<app::Message>> = Vec::new();
        for monitor in &state.monitors {
            for thumb in &monitor.thumbnails {
                if let Err(e) = thumbnail::unbind_thumbnail(thumb.thumbnail_id) {
                    log::warn!("Failed to unbind thumbnail: {e:#}");
                }
            }
            close_tasks.push(iced::window::close::<app::Message>(monitor.window_id));
        }

        self.switch_to_tiler_mode()?;
        Ok(Task::batch(close_tasks))
    }

    /// Whether the given iced window id belongs to one of our overview
    /// windows. Used by view/theme/event dispatch.
    pub fn overview_monitor_for_window(
        &self,
        window_id: iced::window::Id,
    ) -> Option<&MonitorOverview> {
        if let Mode::Overview(state) = &self.mode {
            state.monitors.iter().find(|m| m.window_id == window_id)
        } else {
            None
        }
    }

    fn overview_monitor_for_window_mut(
        &mut self,
        window_id: iced::window::Id,
    ) -> Option<&mut MonitorOverview> {
        if let Mode::Overview(state) = &mut self.mode {
            state.monitors.iter_mut().find(|m| m.window_id == window_id)
        } else {
            None
        }
    }

    pub fn handle_overview_cursor_moved(
        &mut self,
        window_id: iced::window::Id,
        pos: iced::Point,
    ) {
        if let Some(monitor) = self.overview_monitor_for_window_mut(window_id) {
            monitor.cursor_pos = Some(pos);
        }
    }

    pub fn handle_overview_mouse_pressed(&mut self, window_id: iced::window::Id) {
        if let Some(monitor) = self.overview_monitor_for_window_mut(window_id) {
            // If the right-click context menu is open, the left-click that
            // triggered this is either targeting a menu button (the
            // button's on_press fires the action and dismisses the menu)
            // or clicking outside to dismiss. Don't record press_pos —
            // by the time the matching mouse-up arrives the menu may
            // already be dismissed, and the release-outcome check would
            // otherwise misinterpret the gesture as click-to-jump or
            // drag-to-reorder.
            if monitor.context_menu.is_some() {
                monitor.press_pos = None;
                monitor.drag_source_idx = None;
                return;
            }
            monitor.press_pos = monitor.cursor_pos;
            monitor.drag_source_idx = monitor.cursor_pos.and_then(|p| monitor.thumbnail_at(p));
        }
    }

    pub fn overview_release_outcome(
        &mut self,
        window_id: iced::window::Id,
    ) -> Option<ReleaseOutcome> {
        let monitor = self.overview_monitor_for_window_mut(window_id)?;
        let press = monitor.press_pos.take();
        let drag_source_idx = monitor.drag_source_idx.take();
        let current = monitor.cursor_pos?;
        let press = press?;

        let dx = current.x - press.x;
        let dy = current.y - press.y;
        let distance = (dx * dx + dy * dy).sqrt();

        if distance <= CLICK_DRAG_THRESHOLD_PX {
            return monitor
                .thumbnail_at(current)
                .map(|i| ReleaseOutcome::Click(monitor.thumbnails[i].src));
        }

        // Drag: only intra-monitor reorder. Cross-monitor moves go through
        // the context menu (Phase 4).
        let src_idx = drag_source_idx?;
        let dst_idx = monitor.thumbnail_at(current)?;
        if src_idx == dst_idx {
            return None;
        }
        Some(ReleaseOutcome::Reorder {
            src: monitor.thumbnails[src_idx].src,
            dst: monitor.thumbnails[dst_idx].src,
        })
    }

    /// Right-click in an overview: hit-test the cursor against thumbnails;
    /// if there's a hit, open a context menu anchored at the cursor.
    pub fn open_overview_context_menu(&mut self, window_id: iced::window::Id) {
        let monitors = crate::monitor::enumerate();
        let Some(monitor_overview) = self.overview_monitor_for_window_mut(window_id) else {
            return;
        };
        let Some(cursor) = monitor_overview.cursor_pos else {
            return;
        };
        let Some(thumb_idx) = monitor_overview
            .thumbnails
            .iter()
            .position(|t| t.rect.contains(f64::from(cursor.x), f64::from(cursor.y)))
        else {
            // Right-click on empty space dismisses any open menu.
            monitor_overview.context_menu = None;
            return;
        };

        let thumb = &monitor_overview.thumbnails[thumb_idx];
        let target = thumb.src;
        let app_name = thumb.app_name.clone();
        let process = target.process_name().unwrap_or_default();
        let our_hmonitor = monitor_overview.hmonitor;

        // Anchor the menu just below the thumbnail's bottom edge. DWM
        // composites thumbnails on top of anything iced renders inside
        // their rect, so the popup has to live outside that rect to stay
        // visible.
        #[allow(clippy::cast_possible_truncation)]
        let anchor = iced::Point::new(
            thumb.rect.x as f32,
            (thumb.rect.y + thumb.rect.height) as f32 + 4.0,
        );

        let other_monitors: Vec<(String, String)> = monitors
            .iter()
            .filter(|m| m.hmonitor != our_hmonitor)
            .enumerate()
            .map(|(i, m)| {
                let label = if m.is_primary {
                    format!("Move to primary monitor")
                } else {
                    format!("Move to monitor {}", i + 2)
                };
                (m.device_name.clone(), label)
            })
            .collect();

        let _ = cursor; // Cursor used for hit-test only; menu anchors to thumbnail.
        monitor_overview.context_menu = Some(ContextMenu {
            target,
            anchor,
            app_name,
            process,
            other_monitors,
        });
    }

    /// Dismiss any open context menu. Called on left-click anywhere or after
    /// an action button is pressed.
    pub fn dismiss_overview_context_menu(&mut self) {
        if let Mode::Overview(state) = &mut self.mode {
            for monitor in &mut state.monitors {
                monitor.context_menu = None;
            }
        }
    }

    /// Whether any monitor's overview currently has a context menu open.
    pub fn overview_context_menu_open(&self) -> bool {
        if let Mode::Overview(state) = &self.mode {
            state.monitors.iter().any(|m| m.context_menu.is_some())
        } else {
            false
        }
    }

    /// "Ignore app" action from the context menu — adds the process name
    /// to `ignored_processes` in the persisted config and forces a tiler
    /// refresh so the app falls out immediately.
    pub fn overview_action_ignore_app(&mut self, process: String) -> anyhow::Result<()> {
        if process.is_empty() {
            return Ok(());
        }
        let mut cfg = crate::config::current().clone();
        if !cfg.filter.ignored_processes.iter().any(|p| p == &process) {
            cfg.filter.ignored_processes.push(process.clone());
            crate::config::save(cfg)
                .context("saving config after Ignore app from overview menu")?;
            log::info!("Overview menu: ignoring app {process}");
        }
        self.dismiss_overview_context_menu();
        Ok(())
    }

    /// "Ignore this window" action — adds a (process, title) tuple to the
    /// persisted `ignored_window_titles` so this specific window stays out
    /// of the tiler across restarts. Useful for popup/settings dialogs
    /// that an app reuses with a stable title.
    pub fn overview_action_ignore_window(&mut self, hwnd_raw: u64) -> anyhow::Result<()> {
        let target = crate::window::Window::from_safe_hwnd(hwnd_raw)
            .context("invalid HWND for Ignore window")?;
        let process = target.process_name().unwrap_or_default();
        let title = target.title().ok().flatten().unwrap_or_default();
        let class = target.class().unwrap_or_default();
        if process.is_empty() {
            log::warn!("Ignore window: missing process; skipping persist");
            self.dismiss_overview_context_menu();
            return Ok(());
        }

        let class_opt = if class.is_empty() { None } else { Some(class.clone()) };
        let mut cfg = crate::config::current().clone();
        let already = cfg.filter.ignored_window_titles.iter().any(|e| {
            e.process == process
                && e.title.as_deref() == Some(title.as_str())
                && e.class.as_deref() == class_opt.as_deref()
        });
        if !already {
            cfg.filter
                .ignored_window_titles
                .push(crate::config::IgnoredWindowTitle {
                    process: process.clone(),
                    title: Some(title.clone()),
                    title_starts_with: None,
                    class: class_opt.clone(),
                });
            crate::config::save(cfg)
                .context("saving config after Ignore window from overview menu")?;
            log::info!(
                "Overview menu: ignoring window process={process} class={class:?} title={title:?}"
            );
        }
        self.dismiss_overview_context_menu();
        Ok(())
    }

    /// "Move to monitor" action — centers the target window in the named
    /// monitor's work area. If the destination is the tiling monitor and the
    /// window isn't already tiled, the next snapshot picks it up; if the
    /// destination is a floating monitor and the window is tiled, the next
    /// drag-end / monitor-mismatch check untiles it.
    pub fn overview_action_move_to_monitor(
        &mut self,
        target: crate::window::Window,
        device_name: String,
    ) -> anyhow::Result<()> {
        let monitor = crate::monitor::find_by_device_name(&device_name)
            .with_context(|| format!("monitor `{device_name}` not attached"))?;
        target
            .move_to_monitor(&monitor)
            .context("moving window to selected monitor")?;
        log::info!(
            "Overview menu: moved {:?} to {}",
            target.handle(),
            monitor.device_name
        );
        self.dismiss_overview_context_menu();
        Ok(())
    }

    /// "Force redraw" action — runs the SetWindowRgn(NULL) +
    /// SWP_FRAMECHANGED + RedrawWindow sequence to unstick a window
    /// whose compositor went blank but is still receiving input.
    /// Mirrors `POST /windows/{id}/wake` so the right-click menu has
    /// the same affordance as the API.
    pub fn overview_action_force_redraw(&mut self, hwnd_raw: u64) -> anyhow::Result<()> {
        let target = crate::window::Window::from_safe_hwnd(hwnd_raw)
            .context("invalid HWND for Force redraw")?;
        target
            .force_repaint()
            .context("running repaint sequence on target window")?;
        log::info!(
            "Overview menu: force-redraw on {:?}",
            target.handle()
        );
        self.dismiss_overview_context_menu();
        Ok(())
    }

    pub fn reorder_overview(&mut self, src: Window, dst: Window) -> anyhow::Result<()> {
        self.tiler.reorder(src, dst);

        // Only the tiling monitor's thumbnails reflect the tiler's order;
        // recompute that monitor's layout in-place. Match the open-overview
        // filter: skip iconic/cloaked so the strip layout doesn't leave a
        // gap for a window that has no on-screen thumbnail to position.
        let strip_height = self.tiler.screen_size().height();
        let windows: Vec<thumbnail::WindowData> = self
            .tiler
            .windows()
            .filter(|item| {
                !item.inner.is_iconic() && !item.inner.is_cloaked().unwrap_or(false)
            })
            .map(|item| thumbnail::WindowData {
                inner: item.inner,
                width: item.width,
                height: strip_height,
            })
            .collect();
        let layouts =
            thumbnail::compute_thumbnail_layouts(&windows, self.tiler.screen_size(), 10.0);

        let Some(tiling_hmonitor) = self.tiling_hmonitor() else {
            return Ok(());
        };
        let Mode::Overview(state) = &mut self.mode else {
            return Ok(());
        };
        let Some(monitor) = state
            .monitors
            .iter_mut()
            .find(|m| m.hmonitor == tiling_hmonitor)
        else {
            return Ok(());
        };

        for thumb in &mut monitor.thumbnails {
            let Some(idx) = windows.iter().position(|w| w.inner == thumb.src) else {
                continue;
            };
            let new_rect = layouts[idx].rect;
            thumb.rect = new_rect;
            if let Err(e) = thumbnail::update_thumbnail_rect(thumb.thumbnail_id, new_rect) {
                log::warn!("Failed to update DWM thumbnail rect during reorder: {e:#}");
            }
        }

        Ok(())
    }
}
