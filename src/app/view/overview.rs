//! Overview window's iced view. DWM paints the thumbnails on top of whatever
//! iced renders here, so we only draw decoration in the gaps and around
//! thumbnails — currently window titles + drag visual feedback.

use iced::{
    Alignment, Color, Length, Padding, Rectangle, Renderer, Theme, mouse,
    widget::{
        self, button, column, container,
        canvas::{self, Image as CanvasImage, Path, Stroke},
        stack, text,
    },
};

use crate::app::{
    self,
    service::overview::{CLICK_DRAG_THRESHOLD_PX, ContextMenu, MonitorOverview, Thumbnail, ThumbnailRect},
};

const APP_NAME_FONT_SIZE: f32 = 18.0;
const TITLE_FONT_SIZE: f32 = 12.0;
const LABEL_PAD_TOP_FROM_THUMBNAIL: f32 = 8.0;
const APP_NAME_LINE_HEIGHT: f32 = 22.0;
const TITLE_MAX_CHARS: usize = 90;
/// Icon edge size next to the app-name label.
const ICON_SIZE: f32 = 24.0;
/// Gap between the icon and the start of the app-name text.
const ICON_GAP: f32 = 6.0;

const DROP_LINE_COLOR: Color = Color::from_rgb(0.30, 0.78, 1.0);
const DROP_LINE_WIDTH_PX: f32 = 3.0;
/// How far above/below each thumbnail the insertion line extends, so it
/// reads as a continuous bar that obviously belongs to the row.
const DROP_LINE_OVERHANG_PX: f32 = 12.0;
const GHOST_FILL: Color = Color {
    r: 1.0,
    g: 1.0,
    b: 1.0,
    a: 0.12,
};
const GHOST_BORDER: Color = Color {
    r: 1.0,
    g: 1.0,
    b: 1.0,
    a: 0.60,
};

pub fn view(monitor: &MonitorOverview) -> iced::Element<'_, app::Message> {
    let drag_info = compute_drag_info(monitor);

    let canvas_layer: iced::Element<'_, app::Message> = widget::canvas(OverviewCanvas {
        thumbnails: &monitor.thumbnails,
        drag: drag_info,
    })
    .width(Length::Fill)
    .height(Length::Fill)
    .into();

    if let Some(menu) = &monitor.context_menu {
        // Stack the menu on top of the canvas, positioned via padding so
        // its top-left lands at the right-click anchor. The menu is a
        // small column of buttons; clicks on the buttons emit action
        // messages and the rest is left to bubble through to the
        // window-level event handler which dismisses on any other click.
        let menu_widget = build_context_menu(menu);
        stack![canvas_layer, menu_widget].into()
    } else {
        canvas_layer
    }
}

fn build_context_menu<'a>(menu: &'a ContextMenu) -> iced::Element<'a, app::Message> {
    let mut items = column![].spacing(2);

    // Persistent "just this window" ignore — matched by (process, title)
    // so the same popup stays ignored across restarts.
    items = items.push(
        button(text("Ignore this window").size(13))
            .on_press(app::Message::OverviewIgnoreWindow(
                menu.target.handle().0 as u64,
            ))
            .width(Length::Fixed(220.0)),
    );

    // Persistent "ignore the whole exe" — writes to config.
    if !menu.process.is_empty() {
        items = items.push(
            button(text(format!("Ignore all {} windows", menu.app_name)).size(13))
                .on_press(app::Message::OverviewIgnoreApp(menu.process.clone()))
                .width(Length::Fixed(220.0)),
        );
    }

    for (device_name, label) in &menu.other_monitors {
        items = items.push(
            button(text(label.as_str()).size(13))
                .on_press(app::Message::OverviewMoveToMonitor {
                    target: menu.target,
                    device_name: device_name.clone(),
                })
                .width(Length::Fixed(220.0)),
        );
    }

    let menu_block = container(items)
        .padding(8)
        .style(|theme: &Theme| {
            let palette = theme.extended_palette();
            container::Style {
                background: Some(palette.background.base.color.into()),
                border: iced::Border {
                    color: palette.background.strong.color,
                    width: 1.0,
                    radius: 6.0.into(),
                },
                ..container::Style::default()
            }
        });

    // Offset by the anchor via padding on an outer container so the menu
    // appears at the cursor.
    container(menu_block)
        .padding(Padding {
            top: menu.anchor.y,
            right: 0.0,
            bottom: 0.0,
            left: menu.anchor.x,
        })
        .align_x(Alignment::Start)
        .align_y(Alignment::Start)
        .into()
}

#[derive(Debug, Clone, Copy)]
struct DragInfo {
    /// Source thumbnail index being dragged.
    source_idx: usize,
    /// Current cursor position inside the overview window.
    cursor: iced::Point,
    /// Index of the thumbnail under the cursor — the drop target — if any.
    dest_idx: Option<usize>,
}

fn compute_drag_info(monitor: &MonitorOverview) -> Option<DragInfo> {
    let source_idx = monitor.drag_source_idx?;
    let cursor = monitor.cursor_pos?;
    let press = monitor.press_pos?;

    let dx = cursor.x - press.x;
    let dy = cursor.y - press.y;
    if (dx * dx + dy * dy).sqrt() <= CLICK_DRAG_THRESHOLD_PX {
        return None;
    }

    let dest_idx = monitor.thumbnails.iter().position(|t| {
        t.rect.contains(f64::from(cursor.x), f64::from(cursor.y))
    });

    Some(DragInfo {
        source_idx,
        cursor,
        dest_idx,
    })
}

struct OverviewCanvas<'a> {
    thumbnails: &'a [Thumbnail],
    drag: Option<DragInfo>,
}

impl canvas::Program<app::Message> for OverviewCanvas<'_> {
    type State = ();

    fn draw(
        &self,
        _state: &Self::State,
        renderer: &Renderer,
        _theme: &Theme,
        bounds: iced::Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<canvas::Geometry<Renderer>> {
        let mut frame = canvas::Frame::new(renderer, bounds.size());

        for thumb in self.thumbnails {
            draw_label(&mut frame, thumb);
        }

        // Drop indicator: a single vertical line at the leading edge of the
        // target thumbnail (matches our reorder semantics — src lands on
        // dst's slot, shifting dst to the right).
        if let Some(drag) = self.drag
            && let Some(dest_idx) = drag.dest_idx
            && drag.source_idx != dest_idx
            && let Some(dest) = self.thumbnails.get(dest_idx)
        {
            draw_drop_line(&mut frame, &dest.rect);
        }

        // Ghost: a translucent outline of the source thumbnail anchored to the
        // cursor so the user can see what they're dragging.
        if let Some(drag) = self.drag
            && let Some(source) = self.thumbnails.get(drag.source_idx)
        {
            draw_ghost(&mut frame, &source.rect, drag.cursor);
        }

        vec![frame.into_geometry()]
    }
}

fn draw_label(frame: &mut canvas::Frame, thumb: &Thumbnail) {
    #[allow(clippy::cast_possible_truncation)]
    let left_x = thumb.rect.x as f32;
    #[allow(clippy::cast_possible_truncation)]
    let below_y = (thumb.rect.y + thumb.rect.height) as f32 + LABEL_PAD_TOP_FROM_THUMBNAIL;
    #[allow(clippy::cast_possible_truncation)]
    let max_width = thumb.rect.width as f32;

    // App icon (if available) to the left of the app name.
    let (text_x_offset, app_name_max_width) = if let Some((_, _, handle)) = &thumb.icon {
        frame.draw_image(
            Rectangle {
                x: left_x,
                y: below_y,
                width: ICON_SIZE,
                height: ICON_SIZE,
            },
            CanvasImage::new(handle.clone()),
        );
        (ICON_SIZE + ICON_GAP, (max_width - ICON_SIZE - ICON_GAP).max(0.0))
    } else {
        (0.0, max_width)
    };

    // App name (large, top line).
    frame.fill_text(canvas::Text {
        content: thumb.app_name.clone(),
        position: iced::Point::new(left_x + text_x_offset, below_y + 2.0),
        max_width: app_name_max_width,
        color: Color::WHITE,
        size: iced::Pixels(APP_NAME_FONT_SIZE),
        ..canvas::Text::default()
    });

    // Window title (smaller, secondary line) — full width below the icon.
    let mut title = thumb.title.clone();
    if title.chars().count() > TITLE_MAX_CHARS {
        title = title.chars().take(TITLE_MAX_CHARS - 1).collect::<String>() + "…";
    }
    frame.fill_text(canvas::Text {
        content: title,
        position: iced::Point::new(left_x, below_y + APP_NAME_LINE_HEIGHT + 6.0),
        max_width,
        color: Color::from_rgba(1.0, 1.0, 1.0, 0.70),
        size: iced::Pixels(TITLE_FONT_SIZE),
        ..canvas::Text::default()
    });
}

fn draw_drop_line(frame: &mut canvas::Frame, dest_rect: &ThumbnailRect) {
    // Vertical line on the *left* edge of the destination thumbnail —
    // indicates "src will be inserted here, dst slides right".
    #[allow(clippy::cast_possible_truncation)]
    let x = dest_rect.x as f32;
    #[allow(clippy::cast_possible_truncation)]
    let top = dest_rect.y as f32 - DROP_LINE_OVERHANG_PX;
    #[allow(clippy::cast_possible_truncation)]
    let bottom = (dest_rect.y + dest_rect.height) as f32 + DROP_LINE_OVERHANG_PX;

    let path = Path::line(iced::Point::new(x, top), iced::Point::new(x, bottom));
    frame.stroke(
        &path,
        Stroke::default()
            .with_color(DROP_LINE_COLOR)
            .with_width(DROP_LINE_WIDTH_PX),
    );
}

fn draw_ghost(frame: &mut canvas::Frame, source_rect: &ThumbnailRect, cursor: iced::Point) {
    // Position the ghost rect centered on the cursor for predictability —
    // we don't know the click offset within the source thumbnail and
    // recovering it after the fact isn't worth the bookkeeping.
    #[allow(clippy::cast_possible_truncation)]
    let w = source_rect.width as f32;
    #[allow(clippy::cast_possible_truncation)]
    let h = source_rect.height as f32;
    let pos = iced::Point::new(cursor.x - w / 2.0, cursor.y - h / 2.0);

    let path = Path::rounded_rectangle(pos, iced::Size::new(w, h), 8.0.into());
    frame.fill(&path, GHOST_FILL);
    frame.stroke(
        &path,
        Stroke::default().with_color(GHOST_BORDER).with_width(2.0),
    );
}
