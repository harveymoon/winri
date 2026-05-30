//! In-app settings panel.
//!
//! Modal-on-top window (`Level::AlwaysOnTop`) with vertical side-tabs:
//!   General  – padding, resize step
//!   Apps     – currently-tiled apps with one-click "ignore exe / class"
//!   Excludes – the three persistent ignore lists, compact rows
//!
//! Form fields mirror [`crate::config::Config`]; numeric inputs are kept
//! as `String` while editing so partial input ("12.") doesn't reject.
//! Nothing persists until the user clicks Save.

use iced::{
    Alignment, Element, Length, Padding,
    widget::{Space, button, checkbox, column, container, row, scrollable, text, text_input},
};

use crate::{app, config};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsTab {
    General,
    Apps,
    Excludes,
}

impl Default for SettingsTab {
    fn default() -> Self {
        Self::General
    }
}

#[derive(Debug, Clone, Default)]
pub struct SettingsForm {
    pub active_tab: SettingsTab,
    pub padding: String,
    pub resize_increment: String,
    pub throttle_slow_apps: bool,
    pub ignored_processes: Vec<String>,
    pub ignored_classes: Vec<String>,
    /// Per-window persistent ignores. Editable here so users can review
    /// and remove rules they added via Win+I or the overview right-click.
    pub ignored_window_titles: Vec<config::IgnoredWindowTitle>,
    pub new_process: String,
    pub new_class: String,
    pub status: Option<String>,
    /// Snapshot of the currently-tiled windows when the panel was opened.
    pub current_windows: Vec<WindowInfo>,
}

#[derive(Debug, Clone)]
pub struct WindowInfo {
    pub process: String,
    pub class: String,
    pub app_name: String,
    pub title: String,
}

#[derive(Debug, Clone)]
pub enum SettingsMessage {
    TabSelected(SettingsTab),
    PaddingChanged(String),
    ResizeIncrementChanged(String),
    ThrottleSlowAppsChanged(bool),
    NewProcessChanged(String),
    AddProcess,
    AddProcessByName(String),
    RemoveProcess(usize),
    NewClassChanged(String),
    AddClass,
    AddClassByName(String),
    RemoveClass(usize),
    RemoveIgnoredWindow(usize),
    ReloadFromDisk,
    Save,
    Close,
}

impl SettingsForm {
    pub fn from_current_config(current_windows: Vec<WindowInfo>) -> Self {
        let cfg = config::current();
        Self {
            active_tab: SettingsTab::default(),
            padding: cfg.tiling.padding.to_string(),
            resize_increment: cfg.tiling.resize_increment.to_string(),
            throttle_slow_apps: cfg.tiling.throttle_slow_apps,
            ignored_processes: cfg.filter.ignored_processes.clone(),
            ignored_classes: cfg.filter.ignored_classes.clone(),
            ignored_window_titles: cfg.filter.ignored_window_titles.clone(),
            new_process: String::new(),
            new_class: String::new(),
            status: None,
            current_windows,
        }
    }

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
                throttle_slow_apps: self.throttle_slow_apps,
            },
            filter: config::FilterConfig {
                ignored_processes: self.ignored_processes.clone(),
                ignored_classes: self.ignored_classes.clone(),
                ignored_window_titles: self.ignored_window_titles.clone(),
            },
            api,
            monitors,
        })
    }
}

/// Returns `true` if the host should close the settings window.
pub fn update(form: &mut SettingsForm, message: SettingsMessage) -> bool {
    match message {
        SettingsMessage::TabSelected(tab) => form.active_tab = tab,
        SettingsMessage::PaddingChanged(v) => {
            form.padding = v;
            form.status = None;
        }
        SettingsMessage::ResizeIncrementChanged(v) => {
            form.resize_increment = v;
            form.status = None;
        }
        SettingsMessage::ThrottleSlowAppsChanged(v) => {
            form.throttle_slow_apps = v;
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
        SettingsMessage::RemoveIgnoredWindow(i) => {
            if i < form.ignored_window_titles.len() {
                form.ignored_window_titles.remove(i);
            }
        }
        SettingsMessage::ReloadFromDisk => {
            if let Err(e) = config::reload() {
                form.status = Some(format!("Reload failed: {e:#}"));
            } else {
                let cfg = config::current();
                form.padding = cfg.tiling.padding.to_string();
                form.resize_increment = cfg.tiling.resize_increment.to_string();
                form.throttle_slow_apps = cfg.tiling.throttle_slow_apps;
                form.ignored_processes = cfg.filter.ignored_processes.clone();
                form.ignored_classes = cfg.filter.ignored_classes.clone();
                form.ignored_window_titles = cfg.filter.ignored_window_titles.clone();
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
    let tabs = side_tabs(form.active_tab);
    let body = scrollable(
        container(match form.active_tab {
            SettingsTab::General => general_tab(form),
            SettingsTab::Apps => apps_tab(form),
            SettingsTab::Excludes => excludes_tab(form),
        })
        .padding(Padding {
            top: 18.0,
            right: 22.0,
            bottom: 18.0,
            left: 22.0,
        }),
    )
    .height(Length::Fill);

    let status_line: Element<'_, app::Message> = if let Some(msg_text) = &form.status {
        text(msg_text.as_str()).size(12).into()
    } else {
        text("Edits aren't saved until you press Save.").size(11).into()
    };

    let actions = row![
        button(text("Reload from disk").size(12))
            .on_press(msg(SettingsMessage::ReloadFromDisk)),
        Space::new().width(Length::Fill),
        button(text("Close").size(12)).on_press(msg(SettingsMessage::Close)),
        button(text("Save").size(12)).on_press(msg(SettingsMessage::Save)),
    ]
    .spacing(8)
    .align_y(Alignment::Center);

    let footer = column![status_line, actions]
        .spacing(8)
        .padding(Padding {
            top: 10.0,
            right: 14.0,
            bottom: 12.0,
            left: 14.0,
        });

    let main = row![tabs, body];
    container(column![main, footer])
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

fn side_tabs(active: SettingsTab) -> Element<'static, app::Message> {
    let item = |label: &'static str, tab: SettingsTab, active: bool| -> Element<'_, app::Message> {
        let style = if active {
            button::primary
        } else {
            button::secondary
        };
        button(text(label).size(13))
            .on_press(msg(SettingsMessage::TabSelected(tab)))
            .style(style)
            .width(Length::Fill)
            .into()
    };

    container(
        column![
            item("General", SettingsTab::General, active == SettingsTab::General),
            item("Apps", SettingsTab::Apps, active == SettingsTab::Apps),
            item(
                "Excludes",
                SettingsTab::Excludes,
                active == SettingsTab::Excludes,
            ),
        ]
        .spacing(6)
        .padding(Padding {
            top: 18.0,
            right: 10.0,
            bottom: 18.0,
            left: 14.0,
        }),
    )
    .width(Length::Fixed(150.0))
    .height(Length::Fill)
    .into()
}

fn general_tab(form: &SettingsForm) -> Element<'_, app::Message> {
    column![
        text("General").size(18),
        Space::new().height(Length::Fixed(4.0)),
        labeled_input(
            "Padding (px)",
            "10",
            &form.padding,
            |v| msg(SettingsMessage::PaddingChanged(v)),
        ),
        labeled_input(
            "Resize step (px)",
            "20",
            &form.resize_increment,
            |v| msg(SettingsMessage::ResizeIncrementChanged(v)),
        ),
        Space::new().height(Length::Fixed(6.0)),
        checkbox(form.throttle_slow_apps)
            .label("Throttle slow apps during scroll (File Explorer)")
            .on_toggle(|v| msg(SettingsMessage::ThrottleSlowAppsChanged(v)))
            .text_size(13),
        text(
            "Lowers SetWindowPos rate to slow apps (currently File Explorer) so they \
             stay in sync with the strip during fast scrolls. Off = every app gets every frame."
        )
        .size(11),
        Space::new().height(Length::Fixed(6.0)),
        text(
            "Padding and resize step take effect on winri restart. Filter changes apply on save."
        )
        .size(11),
    ]
    .spacing(10)
    .into()
}

fn labeled_input<'a>(
    label: &'a str,
    placeholder: &'a str,
    value: &'a str,
    on_input: impl Fn(String) -> app::Message + 'a,
) -> Element<'a, app::Message> {
    row![
        text(label).size(12).width(Length::Fixed(150.0)),
        text_input(placeholder, value)
            .on_input(on_input)
            .size(13)
            .width(Length::Fixed(120.0)),
    ]
    .spacing(10)
    .align_y(Alignment::Center)
    .into()
}

fn apps_tab(form: &SettingsForm) -> Element<'_, app::Message> {
    let header = text("Currently tiled apps").size(18);
    let hint =
        text("Quick-ignore by exe or window class without typing names.").size(11);

    if form.current_windows.is_empty() {
        return column![header, hint, text("(no tiled windows right now)").size(12)]
            .spacing(8)
            .into();
    }

    // Consolidate by process so 4 Chrome windows render as one "Chrome (4)" row.
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

    let mut list = column![].spacing(4);
    for g in &groups {
        let exe_ignored = form.ignored_processes.iter().any(|p| p == g.process);
        let class_ignored = form
            .ignored_classes
            .iter()
            .any(|c| c == g.representative_class);

        let subtitle = if g.count == 1 {
            g.sample_title.to_string()
        } else {
            format!("{} windows", g.count)
        };

        let info = column![
            text(g.app_name).size(13),
            text(subtitle).size(10),
            text(format!("{}  •  {}", g.process, g.representative_class)).size(10),
        ]
        .spacing(1)
        .width(Length::Fill);

        let exe_btn: Element<'_, app::Message> = if exe_ignored {
            text("✓ exe").size(11).into()
        } else {
            button(text("Ignore exe").size(11))
                .on_press(msg(SettingsMessage::AddProcessByName(g.process.to_string())))
                .into()
        };
        let class_btn: Element<'_, app::Message> = if class_ignored {
            text("✓ class").size(11).into()
        } else {
            button(text("Ignore class").size(11))
                .on_press(msg(SettingsMessage::AddClassByName(
                    g.representative_class.to_string(),
                )))
                .into()
        };

        list = list.push(
            container(
                row![info, exe_btn, class_btn]
                    .spacing(8)
                    .align_y(Alignment::Center),
            )
            .padding(Padding {
                top: 6.0,
                right: 8.0,
                bottom: 6.0,
                left: 8.0,
            }),
        );
    }

    column![header, hint, Space::new().height(Length::Fixed(4.0)), list]
        .spacing(6)
        .into()
}

fn excludes_tab(form: &SettingsForm) -> Element<'_, app::Message> {
    column![
        text("Excludes").size(18),
        text("Processes, window classes, and specific windows winri skips when tiling.")
            .size(11),
        Space::new().height(Length::Fixed(8.0)),
        compact_list_section(
            "Ignored processes (.exe)",
            &form.ignored_processes,
            &form.new_process,
            "MyOverlayApp.exe",
            |v| msg(SettingsMessage::NewProcessChanged(v)),
            msg(SettingsMessage::AddProcess),
            |i| msg(SettingsMessage::RemoveProcess(i)),
        ),
        compact_list_section(
            "Ignored window classes",
            &form.ignored_classes,
            &form.new_class,
            "CEF-OSC-WIDGET",
            |v| msg(SettingsMessage::NewClassChanged(v)),
            msg(SettingsMessage::AddClass),
            |i| msg(SettingsMessage::RemoveClass(i)),
        ),
        per_window_section(form),
    ]
    .spacing(14)
    .into()
}

fn compact_list_section<'a>(
    label: &'a str,
    items: &'a [String],
    new_input: &'a str,
    placeholder: &'a str,
    on_input_change: impl Fn(String) -> app::Message + 'a,
    add_message: app::Message,
    remove_message: impl Fn(usize) -> app::Message + 'a,
) -> Element<'a, app::Message> {
    let mut list = column![].spacing(2);
    if items.is_empty() {
        list = list.push(text("(none)").size(11));
    } else {
        for (i, item) in items.iter().enumerate() {
            list = list.push(
                row![
                    text(item.as_str()).size(12).width(Length::Fill),
                    button(text("×").size(12))
                        .on_press(remove_message(i))
                        .style(button::danger),
                ]
                .spacing(8)
                .align_y(Alignment::Center),
            );
        }
    }

    let add_row = row![
        text_input(placeholder, new_input)
            .on_input(on_input_change)
            .on_submit(add_message.clone())
            .size(12)
            .width(Length::Fill),
        button(text("Add").size(12)).on_press(add_message),
    ]
    .spacing(8)
    .align_y(Alignment::Center);

    column![text(label).size(13), list, add_row]
        .spacing(6)
        .into()
}

fn per_window_section(form: &SettingsForm) -> Element<'_, app::Message> {
    let header = text("Ignored windows (per-window)").size(13);
    let hint = text(
        "Specific windows matched by (process, title, class). Added via Win+I or the \
         overview right-click \u{2192} \u{201C}Ignore this window\u{201D}.",
    )
    .size(10);

    if form.ignored_window_titles.is_empty() {
        return column![header, hint, text("(none)").size(11)]
            .spacing(4)
            .into();
    }

    let mut list = column![].spacing(2);
    for (i, entry) in form.ignored_window_titles.iter().enumerate() {
        let class_str = entry
            .class
            .as_deref()
            .map_or_else(|| "(any class)".to_string(), |c| c.to_string());
        let title_str = if entry.title.is_empty() {
            "(empty title)".to_string()
        } else {
            entry.title.clone()
        };

        // Single-line compact: process · class · title · ×
        list = list.push(
            row![
                text(entry.process.as_str()).size(11).width(Length::Fixed(120.0)),
                text(class_str).size(11).width(Length::Fixed(200.0)),
                text(title_str).size(11).width(Length::Fill),
                button(text("×").size(11))
                    .on_press(msg(SettingsMessage::RemoveIgnoredWindow(i)))
                    .style(button::danger),
            ]
            .spacing(8)
            .align_y(Alignment::Center),
        );
    }

    column![header, hint, list].spacing(4).into()
}

fn msg(m: SettingsMessage) -> app::Message {
    app::Message::Settings(m)
}
