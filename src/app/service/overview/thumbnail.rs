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
    /// Source-window height — used by the grid layout to preserve aspect
    /// ratio. Strip layout ignores it (all strip thumbnails use the same
    /// height derived from the monitor).
    pub height: f32,
}

pub struct ThumbnailLayout {
    pub rect: ThumbnailRect,
}

/// Lay out thumbnails as a wrapping grid — used on monitors that aren't
/// the tile strip's host. Cells are sized so that thumbnails roughly
/// preserve the source-window aspect ratios, distributed across rows
/// chosen to suit the monitor's own aspect (so a portrait secondary
/// monitor packs them vertically). A blank strip below each thumbnail is
/// reserved so the existing label renderer has somewhere to draw the
/// title + app name.
pub fn compute_grid_layout(
    windows: &[WindowData],
    monitor_size: Size,
    padding: f32,
) -> Vec<ThumbnailLayout> {
    /// Space reserved below each thumbnail for the two-line label.
    const LABEL_RESERVE: f32 = 44.0;

    let n = windows.len();
    if n == 0 {
        return Vec::new();
    }

    let mw = monitor_size.width();
    let mh = monitor_size.height();
    #[allow(clippy::cast_precision_loss)]
    let n_f = n as f32;

    // Pick a column count that roughly matches the monitor aspect ratio.
    // For a 1.78 landscape with 12 windows: cols ≈ ceil(sqrt(12 * 1.78)) ≈ 5.
    // For a 0.56 portrait with 12 windows: cols ≈ ceil(sqrt(12 * 0.56)) ≈ 3.
    let aspect = (mw / mh).max(0.01);
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    let cols = ((n_f * aspect).sqrt().ceil() as usize).max(1).min(n);
    let rows = n.div_ceil(cols);

    #[allow(clippy::cast_precision_loss)]
    let cell_w = ((mw - padding) / (cols as f32)) - padding;
    #[allow(clippy::cast_precision_loss)]
    let cell_h = ((mh - padding) / (rows as f32)) - padding;
    let thumb_w = cell_w.max(10.0);
    let thumb_h = (cell_h - LABEL_RESERVE).max(10.0);

    let mut layouts = Vec::with_capacity(n);
    for (i, window) in windows.iter().enumerate() {
        #[allow(clippy::cast_precision_loss)]
        let col = (i % cols) as f32;
        #[allow(clippy::cast_precision_loss)]
        let row = (i / cols) as f32;
        let cell_x = padding + col * (cell_w + padding);
        let cell_y = padding + row * (cell_h + padding);

        // Preserve the source-window aspect ratio: fit the thumbnail inside
        // (thumb_w × thumb_h) and centre it within the cell. Without this,
        // DWM stretches the source to whatever rect we hand it, producing
        // squished thumbnails on portrait/landscape mismatch.
        let src_w = window.width.max(1.0);
        let src_h = window.height.max(1.0);
        let cell_aspect = thumb_w / thumb_h.max(0.01);
        let src_aspect = src_w / src_h;
        let (fit_w, fit_h) = if src_aspect > cell_aspect {
            // Source is wider than the cell — fit by width.
            (thumb_w, thumb_w / src_aspect)
        } else {
            (thumb_h * src_aspect, thumb_h)
        };
        let offset_x = (thumb_w - fit_w) / 2.0;
        let offset_y = (thumb_h - fit_h) / 2.0;

        layouts.push(ThumbnailLayout {
            rect: ThumbnailRect {
                x: f64::from(cell_x + offset_x),
                y: f64::from(cell_y + offset_y),
                width: f64::from(fit_w),
                height: f64::from(fit_h),
            },
        });
    }
    layouts
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

/// Open one fullscreen overview window at a specific monitor's work area.
/// On creation, emits an `OverviewWindowCreated` message tagged with the
/// `hmonitor` so the caller can register the right windows' thumbnails.
pub fn open_overview_window(
    hmonitor: isize,
    origin: (f32, f32),
    size: (f32, f32),
) -> Task<app::Message> {
    let (id, window_creation) = iced::window::open(Settings {
        decorations: false,
        transparent: true,
        size: iced::Size::new(size.0, size.1),
        position: iced::window::Position::Specific(iced::Point::new(origin.0, origin.1)),
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
                overview::Message::OverviewWindowCreated {
                    id,
                    raw_handle,
                    hmonitor,
                },
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
