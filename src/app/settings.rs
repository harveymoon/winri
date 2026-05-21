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
    /// Snapshot of the currently-tiled windows when the settings panel was
    /// opened. Lets the user one-click "Ignore" instead of typing exe names.
    pub current_windows: Vec<WindowInfo>,
}

#[derive(Debug, Clone)]
pub struct WindowInfo {
    /// `chrome.exe`, etc.
    pub process: String,
    /// Win32 window class name.
    pub class: String,
    /// Display name derived from the process exe (e.g. "Chrome").
    pub app_name: String,
    /// Truncated window title for display.
    pub title: String,
}

#[derive(Debug, Clone)]
pub enum SettingsMessage {
    PaddingChanged(String),
    ResizeIncrementChanged(String),
    NewProcessChanged(String),
    AddProcess,
    AddProcessByName(String),
    RemoveProcess(usize),
    NewClassChanged(String),
    AddClass,
    AddClassByName(String),
    RemoveClass(usize),
    ReloadFromDisk,
    Save,
    Close,
}

impl SettingsForm {
    /// Populate the form from the live config and a snapshot of currently
    /// tiled windows.
    pub fn from_current_config(current_windows: Vec<WindowInfo>) -> Self {
        let cfg = config::current();
        Self {
            padding: cfg.tiling.padding.to_string(),
            resize_increment: cfg.tiling.resize_increment.to_string(),
            ignored_processes: cfg.filter.ignored_processes.clone(),
            ignored_classes: cfg.filter.ignored_classes.clone(),
            new_process: String::new(),
            new_class: String::new(),
            status: None,
            current_windows,
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
        // Preserve config fields the settings UI doesn't currently expose
        // (api section, smooth-scroll knobs, monitors section) by reading
        // them from the live config and passing them through unchanged.
        let live = config::current();
        let api = live.api.clone();
        let monitors = live.monitors.clone();
        let smooth_scroll = live.tiling.smooth_scroll;
        let smooth_scroll_factor = live.tiling.smooth_scroll_factor;
        drop(live);

        Ok(config::Config {
            tiling: config::TilingConfig {
                padding,
                resize_increment,
                smooth_scroll,
                smooth_scroll_factor,
            },
            filter: config::FilterConfig {
                ignored_processes: self.ignored_processes.clone(),
                ignored_classes: self.ignored_classes.clone(),
                // The settings UI doesn't yet expose per-window ignores;
                // preserve any that the overview right-click added so we
                // don't drop them on Save.
                ignored_window_titles: config::current()
                    .filter
                    .ignored_window_titles
                    .clone(),
            },
            api,
            monitors,
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
        SettingsMessage::AddProcessByName(name) => {
            if !name.is_empty() && !form.ignored_processes.iter().any(|p| p == &name) {
                form.ignored_processes.push(name);
            }
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
        SettingsMessage::AddClassByName(name) => {
            if !name.is_empty() && !form.ignored_classes.iter().any(|c| c == &name) {
                form.ignored_classes.push(name);
            }
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
                // Re-read config but keep the captured window snapshot — those
                // are the currently-tiled windows, not a config field.
                let cfg = config::current();
                form.padding = cfg.tiling.padding.to_string();
                form.resize_increment = cfg.tiling.resize_increment.to_string();
                form.ignored_processes = cfg.filter.ignored_processes.clone();
                form.ignored_classes = cfg.filter.ignored_classes.clone();
                form.new_process.clear();
                form.new_class.clear();
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

    let current_section = current_windows_section(form);

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
        current_section,
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

fn current_windows_section(form: &SettingsForm) -> Element<'_, app::Message> {
    let header = text("Currently tiled apps").size(16);
    let hint = text("Quick-add to the ignore lists below without typing exe names.").size(11);

    if form.current_windows.is_empty() {
        return column![header, hint, text("(no tiled windows right now)").size(12)]
            .spacing(8)
            .into();
    }

    // Consolidate windows by process so e.g. four Chrome windows show as one
    // row ("Chrome (4)") instead of cluttering the list. The first window we
    // see for each process is treated as the representative for the class
    // name displayed; that's fine because almost all multi-window apps use
    // the same class for their main windows.
    struct Group<'a> {
        app_name: &'a str,
        process: &'a str,
        representative_class: &'a str,
        count: usize,
        sample_title: &'a str,
    }
    let mut groups: Vec<Group<'_>> = Vec::new();
    for win in &form.current_windows {
        if let Some(g) = groups.iter_mut().find(|g| g.process == win.process) {
            g.count += 1;
        } else {
            groups.push(Group {
                app_name: &win.app_name,
                process: &win.process,
                representative_class: &win.class,
                count: 1,
                sample_title: &win.title,
            });
        }
    }

    let mut list = column![].spacing(6);
    for g in &groups {
        let exe_ignored = form.ignored_processes.iter().any(|p| p == g.process);
        let class_ignored = form
            .ignored_classes
            .iter()
            .any(|c| c == g.representative_class);

        let title_or_count = if g.count == 1 {
            g.sample_title.to_string()
        } else {
            format!("{} windows", g.count)
        };

        let info_column = column![
            text(g.app_name).size(14),
            text(title_or_count).size(11),
            text(format!("{}  •  {}", g.process, g.representative_class)).size(10),
        ]
        .spacing(2)
        .width(Length::Fill);

        let exe_button: Element<'_, app::Message> = if exe_ignored {
            text("✓ exe").size(11).into()
        } else {
            button(text("Ignore exe").size(11))
                .on_press(msg(SettingsMessage::AddProcessByName(g.process.to_string())))
                .into()
        };
        let class_button: Element<'_, app::Message> = if class_ignored {
            text("✓ class").size(11).into()
        } else {
            button(text("Ignore class").size(11))
                .on_press(msg(SettingsMessage::AddClassByName(
                    g.representative_class.to_string(),
                )))
                .into()
        };

        list = list.push(
            row![info_column, exe_button, class_button]
                .spacing(8)
                .align_y(iced::Alignment::Center),
        );
    }

    column![header, hint, list].spacing(8).into()
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
