/// The root app module. It handle everything winri does.
pub mod action;
pub mod model;
mod service;
pub mod settings;
mod subscription;
pub mod task;
mod view;

use anyhow::Context;
use iced::{
    Color, Task,
    theme::Palette,
    window::{Settings, settings::PlatformSpecific},
};
use joy_error::ResultUtilityExt;

use crate::{
    app::{
        service::{
            overview::{self},
            tiler::{self},
        },
        subscription::global::GlobalMessage,
    },
    assert_log_fail, config,
    scroll_tiler::ScrollTiler,
    system,
    utils::math::Size,
    window::{self},
};

pub struct State {
    pub tiler: ScrollTiler,
    pub mode: Mode,
    pub configuration: model::Configuration,
    overlay_window_id: iced::window::Id,
    settings_window_id: Option<iced::window::Id>,
    settings_form: settings::SettingsForm,
    /// True from launch until the first successful tiler update completes.
    /// While set, the tiler snapshot ignores the monitor filter and pulls
    /// every visible app window onto the tiling monitor — so a fresh session
    /// always starts with a clean, consolidated tile strip.
    pub(crate) pending_initial_consolidation: bool,
    /// Set synchronously by `prepare_open_overview`, cleared by
    /// `finalize_open_overview` (success or error). Closes the window between
    /// "overview-window-creation task queued" and "Mode::Overview committed"
    /// during which a queued `Win+,` would otherwise slip past the settings
    /// guard — the May 2026 freeze-cascade incident.
    pub(crate) overview_opening: bool,
}

pub enum Mode {
    Tiler(tiler::State),
    Overview(overview::State),
    Exit,
}

impl Default for Mode {
    fn default() -> Self {
        Self::Tiler(tiler::State::default())
    }
}

#[derive(Debug, Clone)]
pub enum Message {
    Action(action::Action),

    Overview(overview::Message),

    Global(subscription::global::GlobalMessage),

    Settings(settings::SettingsMessage),

    /// Command from the local HTTP control API.
    Api(crate::api::ApiCommand),

    /// 60 fps tick that advances the smoothing animation when one is active.
    AnimationTick,

    /// Cursor moved within an iced window. We stash the position so the
    /// next press/release can hit-test against the overview thumbnails.
    WindowCursorMoved {
        window_id: iced::window::Id,
        position: iced::Point,
    },

    /// Left mouse button pressed inside an iced window. We record this so we
    /// can distinguish a click from a drag on the matching mouse-up.
    WindowMouseDown(iced::window::Id),

    /// Left mouse button released inside an iced window. If the cursor moved
    /// only a few pixels since mouse-down (i.e. a click, not a drag), the
    /// overview-mode handler will use this to jump to the clicked thumbnail.
    WindowMouseUp(iced::window::Id),

    /// Right mouse button pressed in an iced window. Opens the overview
    /// context menu when applicable.
    WindowMouseRightDown(iced::window::Id),

    /// An iced-managed window was closed (X-button, OS close, etc.). We
    /// must clear any cached id pointing at it (e.g. `settings_window_id`)
    /// or the next "open it again" path tries to gain_focus on a defunct
    /// handle and silently fails — the visible symptom is that Win+,
    /// stops opening the settings panel until you press it twice.
    WindowClosed(iced::window::Id),

    /// Context-menu actions fired by the overview popup buttons.
    OverviewIgnoreApp(String),
    /// Session-only ignore — just this window's HWND falls out of the
    /// tiler until restart. Other windows of the same app stay tiled.
    OverviewIgnoreWindow(u64),
    OverviewMoveToMonitor {
        target: window::Window,
        device_name: String,
    },
    /// Best-effort "wake up the renderer" for the targeted window. Same
    /// repaint sequence as `POST /windows/{id}/wake` — useful when a
    /// Chromium app's compositor has gone blank and the user wants a
    /// one-click unstick.
    OverviewForceRedraw(u64),

    CleanupAndExit,
}

fn create_overlay_window(screen_size: Size) -> (iced::window::Id, Task<Message>) {
    let (id, task) = iced::window::open(Settings {
        decorations: false,
        transparent: true,
        resizable: false,
        closeable: false,
        level: iced::window::Level::AlwaysOnTop,
        position: iced::window::Position::Specific(iced::Point::ORIGIN),
        size: screen_size.into(),
        platform_specific: PlatformSpecific {
            skip_taskbar: true,
            ..Default::default()
        },
        ..Default::default()
    });

    (id, task.then(iced::window::enable_mouse_passthrough))
}

fn create_settings_window() -> (iced::window::Id, Task<Message>) {
    let (id, task) = iced::window::open(Settings {
        decorations: true,
        transparent: false,
        resizable: true,
        closeable: true,
        // Modal-on-top: keep the settings panel above tiles so the user
        // doesn't lose it behind a focus-stealing app while editing.
        level: iced::window::Level::AlwaysOnTop,
        size: iced::Size::new(720.0, 560.0),
        min_size: Some(iced::Size::new(540.0, 420.0)),
        ..Default::default()
    });
    (id, task.discard())
}

impl State {
    pub fn new() -> (Self, Task<Message>) {
        let screen_size = system::screen_size().expect("Screen size retrieval");
        let (padding, resize_increment, smooth_enabled, smooth_factor, throttle_slow_apps) = {
            let cfg = config::current();
            (
                cfg.tiling.padding,
                cfg.tiling.resize_increment,
                cfg.tiling.smooth_scroll,
                cfg.tiling.smooth_scroll_factor,
                cfg.tiling.throttle_slow_apps,
            )
        };
        let mut tiler = ScrollTiler::new(padding, resize_increment, screen_size);
        tiler.set_smoothing(smooth_enabled, smooth_factor);
        tiler.set_throttle_slow_apps(throttle_slow_apps);
        let (overlay_window_id, overlay_window_creation_task) = create_overlay_window(screen_size);
        (
            Self {
                tiler,
                mode: Mode::default(),
                configuration: model::Configuration {
                    tiler_border_style: model::BorderStyle {
                        color: system::highlight_color().unwrap(),
                        radius: 8.0,
                    },
                },
                overlay_window_id,
                settings_window_id: None,
                settings_form: settings::SettingsForm::default(),
                pending_initial_consolidation: true,
                overview_opening: false,
            },
            overlay_window_creation_task,
        )
    }

    fn open_settings(&mut self) -> Task<Message> {
        if let Some(existing) = self.settings_window_id {
            return iced::window::gain_focus(existing);
        }
        // Refuse to open settings on top of an active overview. Same
        // freeze-cascade reasoning as the symmetric guard in
        // `prepare_open_overview`: the two transient UIs racing into
        // existence leaves no clear recovery path. Close overview
        // first (Win+Esc / Win+Down), then reopen settings.
        //
        // `overview_opening` covers the gap between the overview-window
        // creation tasks being queued and `Mode::Overview` being committed
        // in `finalize_open_overview`. Without it, queued `Win+Up`/`Win+,`
        // hotkeys could still race past this guard.
        if matches!(self.mode, Mode::Overview(_)) || self.overview_opening {
            log::warn!("Settings suppressed: overview is open. Close overview first.");
            return Task::none();
        }
        let current_windows = self.snapshot_tiled_windows_for_settings();
        self.settings_form = settings::SettingsForm::from_current_config(current_windows);
        let (id, task) = create_settings_window();
        self.settings_window_id = Some(id);
        task
    }

    /// Snapshot the tiled windows in a form the settings panel can render
    /// (display name, exe, class, title). Failures per-window are tolerated
    /// — we just skip windows we can't introspect.
    fn snapshot_tiled_windows_for_settings(&self) -> Vec<settings::WindowInfo> {
        const MAX_TITLE_CHARS: usize = 70;

        self.tiler
            .windows()
            .filter_map(|item| {
                let process = item.inner.process_name().ok()?;
                let class = item.inner.class().ok()?;
                let title = item.inner.title().ok().flatten().unwrap_or_default();
                let truncated_title = if title.chars().count() > MAX_TITLE_CHARS {
                    title.chars().take(MAX_TITLE_CHARS - 1).collect::<String>() + "…"
                } else {
                    title
                };
                let app_name = display_name_from_process(&process);
                Some(settings::WindowInfo {
                    process,
                    class,
                    app_name,
                    title: truncated_title,
                })
            })
            .collect()
    }
}

fn display_name_from_process(process_name: &str) -> String {
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

impl State {

    fn close_settings(&mut self) -> Task<Message> {
        if let Some(id) = self.settings_window_id.take() {
            iced::window::close(id)
        } else {
            Task::none()
        }
    }

    /// Persist the currently-focused window as a (process, class, title)
    /// triple in `filter.ignored_window_titles`. Used by Win+I to silence
    /// transient popups (e.g. TouchDesigner's Op Create Dialog) that
    /// auto-dismiss on mouse-click, so the overview right-click flow can't
    /// reach them. All three fields must match for filtering, so a popup
    /// sharing a class with its app's main window stays specific.
    pub fn ignore_focused_window_by_class(&mut self) -> anyhow::Result<()> {
        let focused = window::Window::focused().context("getting focused window")?;
        let class = focused.class().context("reading focused window class")?;
        let process = focused.process_name().unwrap_or_default();
        let title = focused.title().ok().flatten().unwrap_or_default();

        if process.is_empty() || class.is_empty() {
            log::warn!(
                "Ignore focused window: process or class empty (process={process:?}, class={class:?}); skipping"
            );
            return Ok(());
        }

        let mut cfg = config::current().clone();
        let already = cfg.filter.ignored_window_titles.iter().any(|e| {
            e.process == process
                && e.title.as_deref() == Some(title.as_str())
                && e.class.as_deref() == Some(class.as_str())
        });
        if !already {
            cfg.filter
                .ignored_window_titles
                .push(config::IgnoredWindowTitle {
                    process: process.clone(),
                    title: Some(title.clone()),
                    title_starts_with: None,
                    class: Some(class.clone()),
                });
            config::save(cfg).context("saving config after Ignore focused window")?;
            log::info!(
                "Ignore focused window: persisted process={process:?} class={class:?} title={title:?}"
            );
        } else {
            log::info!(
                "Ignore focused window: entry for process={process:?} class={class:?} title={title:?} already exists"
            );
        }
        Ok(())
    }

    fn handle_api_command(&mut self, command: crate::api::ApiCommand) -> Task<Message> {
        use crate::api::ApiCommand;

        match command {
            ApiCommand::Focus(hwnd_raw) => {
                if let Ok(window) = window::Window::from_safe_hwnd(hwnd_raw)
                    && let Err(e) = window.focus()
                {
                    log::warn!("API Focus({hwnd_raw}) failed: {e:#}");
                }
                Task::none()
            }
            ApiCommand::SetScrollOffset { offset, animate_ms } => {
                if matches!(self.mode, Mode::Tiler(_)) {
                    if animate_ms == 0 {
                        // Existing snap path: respects the user's
                        // smooth_scroll config (snap or exponential).
                        self.tiler.set_scroll_offset(offset);
                    } else {
                        // Time-based animation: ignores smooth_scroll
                        // config, always tweens over animate_ms with
                        // ease-out-cubic. Supersedes any in-flight
                        // animation (latest wins).
                        self.tiler.animate_scroll_to(offset, animate_ms);
                    }
                }
                Task::none()
            }
            ApiCommand::ScrollBy { delta, animate_ms } => {
                if matches!(self.mode, Mode::Tiler(_)) {
                    if animate_ms == 0 {
                        self.tiler.scroll_by(delta);
                    } else {
                        let target = self.tiler.scroll_offset() + delta;
                        self.tiler.animate_scroll_to(target, animate_ms);
                    }
                }
                Task::none()
            }
            ApiCommand::Action(named) => Task::done(Message::Action(named_action_to_action(named))),
            ApiCommand::WakeWindow { hwnd } => {
                match window::Window::from_safe_hwnd(hwnd) {
                    Ok(target) => {
                        if let Err(e) = target.force_repaint() {
                            log::warn!("API WakeWindow({hwnd}) failed: {e:#}");
                        } else {
                            log::info!("API WakeWindow({hwnd}) — repaint sequence sent");
                        }
                    }
                    Err(_) => log::warn!("API WakeWindow: invalid HWND {hwnd}"),
                }
                Task::none()
            }
            ApiCommand::MoveToMonitor { hwnd, device_name } => {
                if let Ok(target) = window::Window::from_safe_hwnd(hwnd) {
                    if let Err(e) = self.overview_action_move_to_monitor(target, device_name) {
                        log::warn!("API MoveToMonitor failed: {e:#}");
                    }
                    let _ = self.update_tiler();
                } else {
                    log::warn!("API MoveToMonitor: invalid HWND {hwnd}");
                }
                Task::none()
            }
            ApiCommand::ResizeWindow {
                hwnd,
                target_width,
                animate_ms,
                center,
            } => {
                if matches!(self.mode, Mode::Tiler(_)) {
                    let ok = self.tiler.animate_window_width(hwnd, target_width, animate_ms);
                    if !ok {
                        log::warn!(
                            "API ResizeWindow: HWND {hwnd} not tracked by the tiler"
                        );
                    } else if center {
                        // Compute centering against the FINAL width (not the
                        // live interpolated one) so the scroll animation
                        // converges on the position that'll be correct when
                        // the resize completes — both animations run in the
                        // same 16ms tick loop and finish together for
                        // typical animate_ms values.
                        self.tiler.center_window_at_width(hwnd, target_width);
                    }
                } else {
                    log::warn!(
                        "API ResizeWindow ignored: not in Tiler mode (current = {:?})",
                        std::mem::discriminant(&self.mode)
                    );
                }
                Task::none()
            }
        }
    }
}

fn named_action_to_action(named: crate::api::NamedAction) -> action::Action {
    use crate::api::NamedAction;
    use action::{Action, OverviewAction, TilerAction};
    match named {
        NamedAction::FocusPrev => Action::Tiler(TilerAction::MoveFocusPrevious),
        NamedAction::FocusNext => Action::Tiler(TilerAction::MoveFocusNext),
        NamedAction::SwapPrev => Action::Tiler(TilerAction::SwapWithPrevious),
        NamedAction::SwapNext => Action::Tiler(TilerAction::SwapWithNext),
        NamedAction::ResizeFullscreen => Action::Tiler(TilerAction::ResizeToFullscreen),
        NamedAction::ResizeHalfscreen => Action::Tiler(TilerAction::ResizeToHalfScreen),
        NamedAction::WidthIncrement => Action::Tiler(TilerAction::IncrementWidth),
        NamedAction::WidthDecrement => Action::Tiler(TilerAction::DecrementWidth),
        NamedAction::Refresh => Action::Tiler(TilerAction::ForceRefresh),
        NamedAction::CenterFocused => Action::Tiler(TilerAction::CenterFocused),
        NamedAction::OpenOverview => Action::Tiler(TilerAction::OpenOverview),
        NamedAction::CloseOverview => Action::Overview(OverviewAction::CloseOverview),
        NamedAction::OpenSettings => Action::OpenSettings,
        NamedAction::Exit => Action::Exit,
    }
}

impl State {
    pub fn title(_: &Self, _window_id: iced::window::Id) -> String {
        window::filter::WINRI_IGNORED_WINDOW_TITLE_SUBSTRING.into()
    }

    pub fn handle_app_message(&mut self, message: Message) -> Task<Message> {
        let mut task = Task::none();

        // HACK: the overlay window steals focus when iced creates / shows it,
        // and we want desktop focus restored after that. Used to run
        // unconditionally on every message — at 60Hz animation ticks plus
        // every snapshot publish, that was ~120 GetForegroundWindow +
        // GetDesktopWindow syscalls/sec for a no-op in almost every case.
        // Now gated on the messages that could plausibly steal focus:
        // anything else (AnimationTick, cursor tracking, SSE-driven API
        // commands, etc.) can't change focus state, so the check is wasted.
        let could_steal_focus = matches!(
            message,
            Message::Overview(_)
                | Message::Action(_)
                | Message::Settings(_)
                | Message::Global(_)
                | Message::WindowMouseDown(_)
                | Message::WindowMouseUp(_)
                | Message::WindowMouseRightDown(_)
                | Message::WindowClosed(_)
        );
        if could_steal_focus {
            task = task.chain(task::ensure_overlay_not_focused(self.overlay_window_id));
        }

        match message {
            Message::Global(global_message) => {
                task = task.chain(self.handle_global_message(global_message));
            }
            Message::CleanupAndExit => {
                system::restore_windows();
                return iced::exit();
            }
            Message::Overview(message) => {
                if let Ok(overview_task) = self
                    .handle_overview_message(message)
                    .context("overview message")
                    .handle_faillible_process()
                {
                    task = task.chain(overview_task);
                }
            }
            Message::Action(action) => {
                if let Ok(action_task) = self
                    .handle_action(action)
                    .context("action handling")
                    .handle_faillible_process()
                {
                    task = task.chain(action_task);
                }
            }
            Message::Settings(settings_message) => {
                let close = settings::update(&mut self.settings_form, settings_message);
                if close {
                    task = task.chain(self.close_settings());
                }
            }
            Message::Api(api_command) => {
                task = task.chain(self.handle_api_command(api_command));
            }
            Message::AnimationTick => {
                if self.tiler.is_animating() {
                    self.tiler.tick_animation();
                }
            }
            Message::WindowCursorMoved {
                window_id,
                position,
            } => {
                self.handle_overview_cursor_moved(window_id, position);
            }
            Message::WindowMouseDown(window_id) => {
                self.handle_overview_mouse_pressed(window_id);
            }
            Message::WindowMouseUp(window_id) => {
                // If a context menu is open, suppress the underlying
                // click-to-jump and just dismiss the menu — except if the
                // click was on a menu button (the button's on_press fired
                // its own message already; we only need to ensure the
                // menu closes, which the action handlers do).
                if self.overview_context_menu_open() {
                    self.dismiss_overview_context_menu();
                } else {
                    use crate::app::service::overview::ReleaseOutcome;
                    match self.overview_release_outcome(window_id) {
                        Some(ReleaseOutcome::Click(target)) => {
                            task = task.chain(Task::done(Message::Action(
                                action::Action::Overview(action::OverviewAction::JumpTo(target)),
                            )));
                        }
                        Some(ReleaseOutcome::Reorder { src, dst }) => {
                            if let Err(e) = self.reorder_overview(src, dst) {
                                log::warn!("Reorder failed: {e:#}");
                            }
                        }
                        None => {}
                    }
                }
            }
            Message::WindowMouseRightDown(window_id) => {
                self.open_overview_context_menu(window_id);
            }
            Message::OverviewIgnoreApp(process) => {
                if let Err(e) = self.overview_action_ignore_app(process) {
                    log::warn!("Ignore-app action failed: {e:#}");
                }
                // Re-tile so the now-ignored app falls out.
                let _ = self.update_tiler();
            }
            Message::OverviewIgnoreWindow(hwnd) => {
                if let Err(e) = self.overview_action_ignore_window(hwnd) {
                    log::warn!("Ignore-window action failed: {e:#}");
                }
                let _ = self.update_tiler();
            }
            Message::OverviewMoveToMonitor { target, device_name } => {
                if let Err(e) = self.overview_action_move_to_monitor(target, device_name) {
                    log::warn!("Move-to-monitor action failed: {e:#}");
                }
                let _ = self.update_tiler();
            }
            Message::OverviewForceRedraw(hwnd) => {
                if let Err(e) = self.overview_action_force_redraw(hwnd) {
                    log::warn!("Force-redraw action failed: {e:#}");
                }
            }
            Message::WindowClosed(window_id) => {
                // Clear cached ids that pointed at the closed window so a
                // subsequent open_x() doesn't try to gain_focus on a defunct
                // handle. Specifically fixes: close settings via X, then
                // Win+, no longer reopens (it called gain_focus on a stale
                // id which silently failed).
                if self.settings_window_id == Some(window_id) {
                    self.settings_window_id = None;
                }
            }
        }
        if matches!(self.mode, Mode::Exit) {
            task = task.chain(Task::done(Message::CleanupAndExit));
        }
        task
    }

    pub fn handle_global_message(&mut self, message: GlobalMessage) -> Task<Message> {
        match message {
            GlobalMessage::Key(modifiers, key) => {
                if let Some(action) = self.resolve_action(modifiers, key) {
                    return Task::done(Message::Action(action));
                }
            }
            GlobalMessage::Window => {
                self.update_tiler()
                    .context("global window event")
                    .handle_faillible_process()
                    .discard();
            }
            GlobalMessage::HorizontalScroll { delta_px } => {
                // Only meaningful in Tiler mode; the overview window doesn't
                // share the strip's scroll state.
                if matches!(self.mode, Mode::Tiler(_)) {
                    self.tiler.scroll_by(delta_px);
                }
            }
        }
        Task::none()
    }

    pub fn view(&self, window_id: iced::window::Id) -> iced::Element<'_, Message> {
        if window_id == self.overlay_window_id {
            view::overlay::view(self)
        } else if Some(window_id) == self.settings_window_id {
            settings::view(&self.settings_form)
        } else if let Some(monitor) = self.overview_monitor_for_window(window_id) {
            view::overview::view(monitor)
        } else {
            view::empty()
        }
    }

    pub fn theme(&self, window_id: iced::window::Id) -> iced::Theme {
        if window_id == self.overlay_window_id {
            iced::Theme::custom(
                "Overlay transparent theme",
                Palette {
                    background: Color::from_rgba(0.0, 0.0, 0.0, 0.0),
                    ..Palette::DARK
                },
            )
        } else if self.overview_monitor_for_window(window_id).is_some() {
            iced::Theme::custom(
                "Overview backdrop",
                Palette {
                    background: Color::from_rgba(0.0, 0.0, 0.0, 0.55),
                    ..Palette::DARK
                },
            )
        } else {
            iced::Theme::Dark
        }
    }

    pub fn subscription(state: &Self) -> iced::Subscription<Message> {
        let mut subs = vec![
            iced::Subscription::run(subscription::global::subscription),
            iced::Subscription::run(subscription::api::subscription),
            iced::event::listen_with(on_event),
        ];
        if state.tiler.wants_redraw() {
            subs.push(
                iced::time::every(std::time::Duration::from_millis(16))
                    .map(|_| Message::AnimationTick),
            );
        }
        iced::Subscription::batch(subs)
    }
}

fn on_event(
    event: iced::Event,
    _status: iced::event::Status,
    window_id: iced::window::Id,
) -> Option<Message> {
    use iced::mouse::{Button, Event as MouseEvent};

    match event {
        iced::Event::Mouse(MouseEvent::CursorMoved { position }) => {
            Some(Message::WindowCursorMoved {
                window_id,
                position,
            })
        }
        iced::Event::Mouse(MouseEvent::ButtonPressed(Button::Left)) => {
            Some(Message::WindowMouseDown(window_id))
        }
        iced::Event::Mouse(MouseEvent::ButtonReleased(Button::Left)) => {
            Some(Message::WindowMouseUp(window_id))
        }
        iced::Event::Mouse(MouseEvent::ButtonPressed(Button::Right)) => {
            Some(Message::WindowMouseRightDown(window_id))
        }
        iced::Event::Window(iced::window::Event::Closed) => Some(Message::WindowClosed(window_id)),
        _ => None,
    }
}

#[easy_ext::ext(HandleFaillibleProcessResultExt)]
impl<T, E: std::fmt::Debug> Result<T, E> {
    fn handle_faillible_process(self) -> Self {
        match &self {
            Ok(_) => {}
            Err(e) => {
                assert_log_fail!("{:?}", e);
            }
        }
        self
    }
}
