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

        let windows_snapshot = opened_windows().context("Window enumeration for tiler update")?;

        log::info!("Opened windows: {:?}", get_process_names(&windows_snapshot));

        self.tiler.handle_window_snapshot(&windows_snapshot);

        self.update_tiler_border()?;

        self.publish_api_snapshot();

        Ok(())
    }

    /// Push the latest tiler view to the API state so `/state` and `/windows`
    /// reflect reality. Best-effort; bad introspection on any one window is
    /// just skipped.
    pub(crate) fn publish_api_snapshot(&self) {
        let screen = self.tiler.screen_size();
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
            });
            current_x += item.width + self.tiler.padding();
        }

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
