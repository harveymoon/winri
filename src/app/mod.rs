/// The root app module. It handle everything winri does.
mod action;
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

    /// Left mouse button pressed on an iced window. Used in Overview mode
    /// to jump to the clicked thumbnail's source window.
    WindowClicked(iced::window::Id),

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
        size: iced::Size::new(560.0, 640.0),
        min_size: Some(iced::Size::new(420.0, 420.0)),
        ..Default::default()
    });
    (id, task.discard())
}

impl State {
    pub fn new() -> (Self, Task<Message>) {
        let screen_size = system::screen_size().expect("Screen size retrieval");
        let (padding, resize_increment) = {
            let cfg = config::current();
            (cfg.tiling.padding, cfg.tiling.resize_increment)
        };
        let tiler = ScrollTiler::new(padding, resize_increment, screen_size);
        let (overlay_window_id, overlay_window_creation_task) = create_overlay_window(screen_size);
        (
            Self {
                tiler,
                mode: Mode::default(),
                configuration: model::Configuration {
                    tiler_border_style: model::BorderStyle {
                        color: system::highlight_color().unwrap(),
                        thickness: 4.0,
                        radius: 8.0,
                    },
                },
                overlay_window_id,
                settings_window_id: None,
                settings_form: settings::SettingsForm::default(),
            },
            overlay_window_creation_task,
        )
    }

    fn open_settings(&mut self) -> Task<Message> {
        if let Some(existing) = self.settings_window_id {
            return iced::window::gain_focus(existing);
        }
        self.settings_form = settings::SettingsForm::from_current_config();
        let (id, task) = create_settings_window();
        self.settings_window_id = Some(id);
        task
    }

    fn close_settings(&mut self) -> Task<Message> {
        if let Some(id) = self.settings_window_id.take() {
            iced::window::close(id)
        } else {
            Task::none()
        }
    }

    pub fn title(_: &Self, _window_id: iced::window::Id) -> String {
        window::filter::WINRI_IGNORED_WINDOW_TITLE_SUBSTRING.into()
    }

    pub fn handle_app_message(&mut self, message: Message) -> Task<Message> {
        let mut task = Task::none();

        // HACK: By default the overlay window steals focus when created, but should not be able to be focused.
        // It causes weird behavior like keystroke not recorded until another window is focused.
        // So we refocus the desktop window after creation.
        task = task.chain(task::ensure_overlay_not_focused(self.overlay_window_id));

        match message {
            Message::Global(global_message) => {
                task = task.chain(self.handle_global_message(global_message));
            }
            Message::CleanupAndExit => {
                system::restore_windows();
                return iced::exit();
            }
            Message::Overview(message) => self
                .handle_overview_message(message)
                .handle_faillible_process()
                .discard(),
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
            Message::WindowClicked(window_id) => {
                if let Some(target) = self.window_at_thumbnail_id(window_id) {
                    task = task.chain(Task::done(Message::Action(action::Action::Overview(
                        action::OverviewAction::JumpTo(target),
                    ))));
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
        }
        Task::none()
    }

    pub fn view(&self, window_id: iced::window::Id) -> iced::Element<'_, Message> {
        if window_id == self.overlay_window_id {
            view::overlay::view(self)
        } else if Some(window_id) == self.settings_window_id {
            settings::view(&self.settings_form)
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
        } else {
            iced::Theme::Dark // TODO: Adapt to system theme
        }
    }

    pub fn subscription(_: &Self) -> iced::Subscription<Message> {
        iced::Subscription::batch([
            iced::Subscription::run(subscription::global::subscription),
            iced::event::listen_with(on_event),
        ])
    }
}

fn on_event(
    event: iced::Event,
    _status: iced::event::Status,
    window_id: iced::window::Id,
) -> Option<Message> {
    use iced::mouse::{Button, Event as MouseEvent};

    if matches!(event, iced::Event::Mouse(MouseEvent::ButtonPressed(Button::Left))) {
        Some(Message::WindowClicked(window_id))
    } else {
        None
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
