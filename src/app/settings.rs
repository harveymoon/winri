//! In-app settings window — view + state.
//!
//! The form is a mutable mirror of [`crate::config::Config`]: numeric fields
//! are kept as `String` while editing so partial input ("12.") doesn't reject,
//! and lists are mutated locally until the user clicks Save.

use iced::{
    Element, Length,
    widget::{button, column, container, row, scrollable, text, text_input},
};

use crate::{app, config};

#[derive(Debug, Clone, Default)]
pub struct SettingsForm {
    pub padding: String,
    pub resize_increment: String,
    pub ignored_processes: Vec<String>,
    pub ignored_classes: Vec<String>,
    pub new_process: String,
    pub new_class: String,
    pub status: Option<String>,
}

#[derive(Debug, Clone)]
pub enum SettingsMessage {
    PaddingChanged(String),
    ResizeIncrementChanged(String),
    NewProcessChanged(String),
    AddProcess,
    RemoveProcess(usize),
    NewClassChanged(String),
    AddClass,
    RemoveClass(usize),
    ReloadFromDisk,
    Save,
    Close,
}

impl SettingsForm {
    /// Populate the form from the live config.
    pub fn from_current_config() -> Self {
        let cfg = config::current();
        Self {
            padding: cfg.tiling.padding.to_string(),
            resize_increment: cfg.tiling.resize_increment.to_string(),
            ignored_processes: cfg.filter.ignored_processes.clone(),
            ignored_classes: cfg.filter.ignored_classes.clone(),
            new_process: String::new(),
            new_class: String::new(),
            status: None,
        }
    }

    /// Try to build a `Config` from the current form values. Returns an error
    /// string suitable for showing the user if numeric parsing fails.
    fn build_config(&self) -> Result<config::Config, String> {
        let padding: f32 = self
            .padding
            .trim()
            .parse()
            .map_err(|_| format!("Padding must be a number, got `{}`", self.padding))?;
        let resize_increment: f32 = self.resize_increment.trim().parse().map_err(|_| {
            format!(
                "Resize step must be a number, got `{}`",
                self.resize_increment
            )
        })?;
        Ok(config::Config {
            tiling: config::TilingConfig {
                padding,
                resize_increment,
            },
            filter: config::FilterConfig {
                ignored_processes: self.ignored_processes.clone(),
                ignored_classes: self.ignored_classes.clone(),
            },
        })
    }
}

/// Returns `Some(true)` if the host should close the settings window.
pub fn update(form: &mut SettingsForm, message: SettingsMessage) -> bool {
    match message {
        SettingsMessage::PaddingChanged(v) => {
            form.padding = v;
            form.status = None;
        }
        SettingsMessage::ResizeIncrementChanged(v) => {
            form.resize_increment = v;
            form.status = None;
        }
        SettingsMessage::NewProcessChanged(v) => form.new_process = v,
        SettingsMessage::AddProcess => {
            let trimmed = form.new_process.trim();
            if !trimmed.is_empty() && !form.ignored_processes.iter().any(|p| p == trimmed) {
                form.ignored_processes.push(trimmed.to_string());
            }
            form.new_process.clear();
        }
        SettingsMessage::RemoveProcess(i) => {
            if i < form.ignored_processes.len() {
                form.ignored_processes.remove(i);
            }
        }
        SettingsMessage::NewClassChanged(v) => form.new_class = v,
        SettingsMessage::AddClass => {
            let trimmed = form.new_class.trim();
            if !trimmed.is_empty() && !form.ignored_classes.iter().any(|c| c == trimmed) {
                form.ignored_classes.push(trimmed.to_string());
            }
            form.new_class.clear();
        }
        SettingsMessage::RemoveClass(i) => {
            if i < form.ignored_classes.len() {
                form.ignored_classes.remove(i);
            }
        }
        SettingsMessage::ReloadFromDisk => {
            if let Err(e) = config::reload() {
                form.status = Some(format!("Reload failed: {e:#}"));
            } else {
                *form = SettingsForm::from_current_config();
                form.status = Some("Reloaded from disk.".into());
            }
        }
        SettingsMessage::Save => match form.build_config() {
            Ok(cfg) => {
                if let Err(e) = config::save(cfg) {
                    form.status = Some(format!("Save failed: {e:#}"));
                } else {
                    form.status = Some(
                        "Saved. Filter changes apply now; padding/resize step take effect on \
                         winri restart."
                            .into(),
                    );
                }
            }
            Err(msg) => form.status = Some(msg),
        },
        SettingsMessage::Close => return true,
    }
    false
}

pub fn view(form: &SettingsForm) -> Element<'_, app::Message> {
    let title = text("Winri settings").size(22);

    let tiling_section = column![
        text("Tiling").size(16),
        row![
            text("Padding (px):").width(Length::Fixed(160.0)),
            text_input("10", &form.padding)
                .on_input(|v| msg(SettingsMessage::PaddingChanged(v)))
                .width(Length::Fixed(100.0)),
        ]
        .spacing(10)
        .align_y(iced::Alignment::Center),
        row![
            text("Resize step (px):").width(Length::Fixed(160.0)),
            text_input("20", &form.resize_increment)
                .on_input(|v| msg(SettingsMessage::ResizeIncrementChanged(v)))
                .width(Length::Fixed(100.0)),
        ]
        .spacing(10)
        .align_y(iced::Alignment::Center),
    ]
    .spacing(8);

    let processes_section = list_section(
        "Ignored processes (.exe filename)",
        &form.ignored_processes,
        &form.new_process,
        "e.g. MyOverlayApp.exe",
        |v| msg(SettingsMessage::NewProcessChanged(v)),
        msg(SettingsMessage::AddProcess),
        |i| msg(SettingsMessage::RemoveProcess(i)),
    );

    let classes_section = list_section(
        "Ignored window classes",
        &form.ignored_classes,
        &form.new_class,
        "e.g. CEF-OSC-WIDGET",
        |v| msg(SettingsMessage::NewClassChanged(v)),
        msg(SettingsMessage::AddClass),
        |i| msg(SettingsMessage::RemoveClass(i)),
    );

    let status_line: Element<'_, app::Message> = if let Some(msg_text) = &form.status {
        text(msg_text.as_str()).size(13).into()
    } else {
        text(
            "Edits aren't saved until you click Save. Filter changes apply immediately on save; \
             padding/resize step require a winri restart.",
        )
        .size(12)
        .into()
    };

    let actions = row![
        button(text("Reload from disk")).on_press(msg(SettingsMessage::ReloadFromDisk)),
        iced::widget::space::horizontal(),
        button(text("Close")).on_press(msg(SettingsMessage::Close)),
        button(text("Save")).on_press(msg(SettingsMessage::Save)),
    ]
    .spacing(8);

    let content = column![
        title,
        tiling_section,
        processes_section,
        classes_section,
        status_line,
        actions,
    ]
    .spacing(18)
    .padding(20);

    container(scrollable(content))
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

fn msg(m: SettingsMessage) -> app::Message {
    app::Message::Settings(m)
}

fn list_section<'a>(
    label: &'a str,
    items: &'a [String],
    new_input: &'a str,
    placeholder: &'a str,
    on_input_change: impl Fn(String) -> app::Message + 'a,
    add_message: app::Message,
    remove_message: impl Fn(usize) -> app::Message + 'a,
) -> Element<'a, app::Message> {
    let mut list = column![].spacing(4);
    if items.is_empty() {
        list = list.push(text("(none)").size(12));
    } else {
        for (i, item) in items.iter().enumerate() {
            list = list.push(
                row![
                    text(item.as_str()).width(Length::Fill),
                    button(text("×")).on_press(remove_message(i)),
                ]
                .spacing(10)
                .align_y(iced::Alignment::Center),
            );
        }
    }

    let add_row = row![
        text_input(placeholder, new_input)
            .on_input(on_input_change)
            .on_submit(add_message.clone())
            .width(Length::Fill),
        button(text("Add")).on_press(add_message),
    ]
    .spacing(8);

    column![text(label).size(16), list, add_row,]
        .spacing(8)
        .into()
}
