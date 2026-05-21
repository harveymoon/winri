pub type ThumbnailId = isize;

use iced::{
    Task,
    window::{
        Settings,
        settings::{PlatformSpecific, platform::CornerPreference},
    },
};
use log::debug;
use windows::Win32::{
    Foundation::RECT,
    Graphics::Dwm::{
        DWM_THUMBNAIL_PROPERTIES, DWM_TNP_RECTDESTINATION, DWM_TNP_VISIBLE, DwmRegisterThumbnail,
        DwmUnregisterThumbnail, DwmUpdateThumbnailProperties,
    },
};

use crate::{
    app::{self, service::overview},
    utils::math::Size,
    wincall_result,
    window::Window,
};

#[derive(Debug, Clone, Copy)]
pub struct ThumbnailRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl ThumbnailRect {
    pub fn contains(&self, x: f64, y: f64) -> bool {
        x >= self.x && x < self.x + self.width && y >= self.y && y < self.y + self.height
    }
}

pub struct WindowData {
    pub inner: Window,
    pub width: f32,
}

pub struct ThumbnailLayout {
    pub rect: ThumbnailRect,
}

/// Lay out thumbnails as a centered horizontal strip, scaled down from the
/// tiler's full width.
pub fn compute_thumbnail_layouts(
    windows: &[WindowData],
    screen_size: Size,
    padding: f32,
) -> Vec<ThumbnailLayout> {
    let total_tiler_width = windows.iter().map(|w| w.width + padding).sum::<f32>() - padding;

    let reduction_ratio = screen_size.width() / total_tiler_width;

    debug!("total tiler width including padding: {total_tiler_width}");
    debug!("Thumbnail reduction ratio for packing windows: {reduction_ratio}");

    let reduction_ratio = if reduction_ratio > 1.0 {
        reduction_ratio * 0.6
    } else if reduction_ratio < 1.0 {
        reduction_ratio * 0.9
    } else {
        reduction_ratio
    }
    .clamp(0.0, 0.6);

    debug!("Thumbnail reduction ratio after size adaptation: {reduction_ratio}");

    let mut current_x = 0.0;
    let mut layouts = Vec::new();

    let thumbnail_height = reduction_ratio * screen_size.height();
    let thumbnail_y = (screen_size.height() - thumbnail_height).abs() / 2.0;
    let thumbnail_x_center_offset =
        (screen_size.width() - reduction_ratio * total_tiler_width).abs() / 2.0;

    for window in windows {
        let width = reduction_ratio * window.width;
        layouts.push(ThumbnailLayout {
            rect: ThumbnailRect {
                x: f64::from(current_x + thumbnail_x_center_offset),
                y: f64::from(thumbnail_y),
                width: f64::from(width),
                height: f64::from(thumbnail_height),
            },
        });
        current_x += width + padding;
    }

    layouts
}

/// Open the single fullscreen overview window. On creation, emits an
/// `OverviewWindowCreated` message carrying the iced id and raw HWND so the
/// caller can bind DWM thumbnails into it.
///
/// We rely on iced's own visibility/level handling rather than calling Win32
/// `ShowWindow` ourselves — that flow has been flaky on iced-managed windows
/// in this fork (intermittent "system cannot find the file" errors).
pub fn open_overview_window(screen_size: Size) -> Task<app::Message> {
    let (id, window_creation) = iced::window::open(Settings {
        decorations: false,
        transparent: true,
        size: screen_size.into(),
        position: iced::window::Position::Specific(iced::Point::ORIGIN),
        resizable: false,
        visible: true,
        level: iced::window::Level::AlwaysOnTop,
        platform_specific: PlatformSpecific {
            skip_taskbar: true,
            corner_preference: CornerPreference::Default,
            ..Default::default()
        },
        ..Default::default()
    });

    window_creation
        .then(move |_| iced::window::raw_id::<app::Message>(id))
        .then(move |raw_handle| {
            Task::done(app::Message::Overview(
                overview::Message::OverviewWindowCreated { id, raw_handle },
            ))
        })
}

pub fn register_thumbnail(
    src: Window,
    dest: Window,
    rect: ThumbnailRect,
) -> anyhow::Result<ThumbnailId> {
    let thumbnail_id = wincall_result!(DwmRegisterThumbnail(dest.handle(), src.handle()))?;

    let props = DWM_THUMBNAIL_PROPERTIES {
        dwFlags: DWM_TNP_RECTDESTINATION | DWM_TNP_VISIBLE,
        rcDestination: RECT {
            left: rect.x as i32,
            top: rect.y as i32,
            right: (rect.x + rect.width) as i32,
            bottom: (rect.y + rect.height) as i32,
        },
        rcSource: RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        },
        opacity: 0,
        fVisible: true.into(),
        fSourceClientAreaOnly: true.into(),
    };

    wincall_result!(DwmUpdateThumbnailProperties(thumbnail_id, &raw const props))?;

    Ok(thumbnail_id)
}

pub fn unbind_thumbnail(thumbnail_id: ThumbnailId) -> anyhow::Result<()> {
    wincall_result!(DwmUnregisterThumbnail(thumbnail_id))?;
    Ok(())
}

/// Update an already-registered thumbnail's destination rect. Used to
/// re-layout after drag-reorder without unregistering and re-registering.
pub fn update_thumbnail_rect(thumbnail_id: ThumbnailId, rect: ThumbnailRect) -> anyhow::Result<()> {
    let props = DWM_THUMBNAIL_PROPERTIES {
        dwFlags: DWM_TNP_RECTDESTINATION | DWM_TNP_VISIBLE,
        rcDestination: RECT {
            left: rect.x as i32,
            top: rect.y as i32,
            right: (rect.x + rect.width) as i32,
            bottom: (rect.y + rect.height) as i32,
        },
        rcSource: RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        },
        opacity: 0,
        fVisible: true.into(),
        fSourceClientAreaOnly: true.into(),
    };
    wincall_result!(DwmUpdateThumbnailProperties(thumbnail_id, &raw const props))?;
    Ok(())
}
