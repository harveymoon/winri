use std::collections::HashSet;

use anyhow::Context;

use crate::{
    app::{self, Mode},
    utils::math::Bounds,
    window::{Window, filter::opened_windows},
};

#[derive(Default)]
pub struct State {
    pub current_border_bounds: Option<Bounds>,
}

macro_rules! bind_tiler_mode_result {
    ($mode:expr => TilerState { $($bindings:tt),+ }) => {
        let Mode::Tiler(State { $($bindings),+ ,.. }) = &mut $mode else {
            log::warn!(
                "Tiler operation requested in {} while not in Tiler mode",
                crate::function!()
            );
            return Ok(());
        };
    };
}

macro_rules! ensure_tiler_mode_result {
    ($mode:expr) => {
        match &$mode {
            Mode::Tiler(_) => {}
            _ => {
                log::warn!(
                    "Tiler operation requested in {} while not in Tiler mode",
                    crate::function!()
                );
                return Ok(());
            }
        }
    };
}

fn get_process_names(windows: &HashSet<Window>) -> Vec<String> {
    windows
        .iter()
        .map(|w| {
            let is_focused = w.is_focused().unwrap_or(false);
            format!(
                "{}{}[class: {}][hwnd: {:?}][title: {}]",
                if is_focused { "[FOCUSED] " } else { "" },
                w.process_name()
                    .ok()
                    .unwrap_or_else(|| "[ERROR] Could not get process name".to_string()),
                w.class()
                    .unwrap_or_else(|_| "[ERROR] Could not get class name".to_string()),
                w.handle(),
                w.title()
                    .unwrap_or_else(|_| Some("[ERROR] Could not get window title".to_string()))
                    .unwrap_or_else(|| "[UNNAMED]".to_string()),
            )
        })
        .collect::<Vec<_>>()
}

impl app::State {
    pub fn update_tiler(&mut self) -> anyhow::Result<()> {
        ensure_tiler_mode_result!(self.mode);

        let tiling_hmonitor = self.tiling_hmonitor();

        let windows_snapshot =
            opened_windows().context("Window enumeration for tiler update")?;

        let initial_pass = self.pending_initial_consolidation;
        if initial_pass {
            log::info!(
                "Initial consolidation: tiling every managed window onto monitor {tiling_hmonitor}"
            );
            self.pending_initial_consolidation = false;
        }

        log::info!("Snapshot: {:?}", get_process_names(&windows_snapshot));

        // For the initial consolidation we pass a synthetic hmonitor of 0
        // for the add-gate; that means the gate compares each window's
        // monitor against 0, but we want EVERY window to be added. So for
        // the initial pass we feed the tiler a sentinel that matches all
        // windows by querying their own monitor in turn — easier: add a
        // dedicated initial-bulk path below.
        if initial_pass {
            // Force-add every window in the snapshot regardless of monitor.
            self.tiler.bulk_seed(&windows_snapshot);
        } else {
            self.tiler
                .handle_window_snapshot(&windows_snapshot, tiling_hmonitor);
        }

        self.update_tiler_border()?;

        self.publish_api_snapshot();

        Ok(())
    }

    /// Cached `HMONITOR` of the configured tiling monitor. Falls back to
    /// the primary monitor if the configured one isn't currently attached.
    /// Cheap (~tens of microseconds) — re-resolved on every tiler tick so
    /// hot-plug Just Works without needing display-change events.
    pub(crate) fn tiling_hmonitor(&self) -> isize {
        let cfg_value = {
            let cfg = crate::config::current();
            cfg.monitors.tiling_monitor.clone()
        };
        crate::monitor::resolve_tiling_monitor(&cfg_value)
            .map(|m| m.hmonitor)
            .unwrap_or(0)
    }

    /// Push the latest tiler view to the API state so `/state` and `/windows`
    /// reflect reality. Best-effort; bad introspection on any one window is
    /// just skipped.
    pub(crate) fn publish_api_snapshot(&self) {
        let screen = self.tiler.screen_size();
        let tiling_hmonitor = self.tiling_hmonitor();
        let all_monitors = crate::monitor::enumerate();
        let tiling_device_name = all_monitors
            .iter()
            .find(|m| m.hmonitor == tiling_hmonitor)
            .map(|m| m.device_name.clone())
            .unwrap_or_default();

        let monitor_lookup = |hmon: isize| -> String {
            all_monitors
                .iter()
                .find(|m| m.hmonitor == hmon)
                .map(|m| m.device_name.clone())
                .unwrap_or_default()
        };

        let mut windows = Vec::new();
        let mut current_x = self.tiler.padding();
        let mut focused_window_id: Option<u64> = None;
        for item in self.tiler.windows() {
            let hwnd_raw = item.inner.handle().0 as u64;
            let title = item.inner.title().ok().flatten().unwrap_or_default();
            let process = item.inner.process_name().unwrap_or_default();
            let class = item.inner.class().unwrap_or_default();
            let focused = item.inner.is_focused().unwrap_or(false);
            if focused {
                focused_window_id = Some(hwnd_raw);
            }
            windows.push(crate::api::WindowSnapshot {
                id: hwnd_raw,
                title,
                process,
                class,
                width: item.width,
                x: current_x,
                monitor: monitor_lookup(item.inner.monitor()),
                tiled: true,
            });
            current_x += item.width + self.tiler.padding();
        }
        // Floating (non-tiled) windows: include them so the API exposes the
        // full picture for external scripts.
        if let Ok(all) = crate::window::filter::all_managed_windows() {
            let tiled_ids: std::collections::HashSet<u64> = self
                .tiler
                .windows()
                .map(|i| i.inner.handle().0 as u64)
                .collect();
            for w in all {
                let hwnd_raw = w.handle().0 as u64;
                if tiled_ids.contains(&hwnd_raw) {
                    continue;
                }
                let title = w.title().ok().flatten().unwrap_or_default();
                let process = w.process_name().unwrap_or_default();
                let class = w.class().unwrap_or_default();
                let bounds = w.desktop_manager_bounds().ok();
                let width = bounds.map(|b| b.size().width()).unwrap_or(0.0);
                windows.push(crate::api::WindowSnapshot {
                    id: hwnd_raw,
                    title,
                    process,
                    class,
                    width,
                    x: 0.0,
                    monitor: monitor_lookup(w.monitor()),
                    tiled: false,
                });
            }
        }

        let monitor_snapshots: Vec<crate::api::MonitorSnapshot> = all_monitors
            .iter()
            .enumerate()
            .map(|(i, m)| crate::api::MonitorSnapshot {
                index: i,
                device_name: m.device_name.clone(),
                is_primary: m.is_primary,
                is_tiling: m.hmonitor == tiling_hmonitor,
                work_area_x: m.work_area.left,
                work_area_y: m.work_area.top,
                work_area_width: m.work_area_width(),
                work_area_height: m.work_area_height(),
            })
            .collect();

        let mode = match &self.mode {
            Mode::Tiler(_) => "tiler",
            Mode::Overview(_) => "overview",
            Mode::Exit => "exit",
        }
        .to_string();

        crate::api::publish_state(crate::api::ApiState {
            mode,
            windows,
            focused_window_id,
            scroll_offset: self.tiler.scroll_offset(),
            total_width: self.tiler.total_strip_width(),
            screen_width: screen.width(),
            screen_height: screen.height(),
            tiling_monitor_device_name: tiling_device_name,
            monitors: monitor_snapshots,
        });
    }

    pub fn update_tiler_border(&mut self) -> anyhow::Result<()> {
        bind_tiler_mode_result!(self.mode => TilerState { current_border_bounds });
        if let Some(focused_window) = self.tiler.focused_window() {
            let bounds = focused_window
                .desktop_manager_bounds()
                .context("Desktop manager bounds querying for tiler border update")?;
            if current_border_bounds != &Some(bounds) {
                *current_border_bounds = Some(bounds);
            }
        } else {
            *current_border_bounds = None;
        }

        Ok(())
    }

    pub fn switch_to_tiler_mode(&mut self) -> anyhow::Result<()> {
        if !matches!(self.mode, Mode::Tiler(_)) {
            log::info!("switching to Tiler mode");
            self.mode = Mode::Tiler(State::default());
            self.update_tiler()
                .context("initial tiler update on mode switch")?;
        }
        Ok(())
    }
}
