//! Overview window's iced view. DWM paints the thumbnails on top of whatever
//! iced renders here, so we only draw decoration in the gaps and around
//! thumbnails — currently window titles + drag visual feedback.

use iced::{
    Color, Length, Renderer, Theme, mouse,
    widget::{
        self,
        canvas::{self, Path, Stroke},
    },
};

use crate::app::{
    self,
    service::overview::{CLICK_DRAG_THRESHOLD_PX, State, Thumbnail, ThumbnailRect},
};

const APP_NAME_FONT_SIZE: f32 = 18.0;
const TITLE_FONT_SIZE: f32 = 12.0;
const LABEL_PAD_TOP_FROM_THUMBNAIL: f32 = 8.0;
const APP_NAME_LINE_HEIGHT: f32 = 22.0;
const TITLE_MAX_CHARS: usize = 90;

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

pub fn view(state: &State) -> iced::Element<'_, app::Message> {
    let drag_info = compute_drag_info(state);

    widget::canvas(OverviewCanvas {
        thumbnails: &state.thumbnails,
        drag: drag_info,
    })
    .width(Length::Fill)
    .height(Length::Fill)
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

fn compute_drag_info(state: &State) -> Option<DragInfo> {
    let source_idx = state.drag_source_idx?;
    let cursor = state.cursor_pos?;
    let press = state.press_pos?;

    // Only treat it as a drag after the user actually moves past the
    // click/drag threshold — otherwise a steady click would render a ghost.
    let dx = cursor.x - press.x;
    let dy = cursor.y - press.y;
    if (dx * dx + dy * dy).sqrt() <= CLICK_DRAG_THRESHOLD_PX {
        return None;
    }

    let dest_idx = state.thumbnails.iter().position(|t| {
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

    // App name (large, top line).
    frame.fill_text(canvas::Text {
        content: thumb.app_name.clone(),
        position: iced::Point::new(left_x, below_y),
        max_width,
        color: Color::WHITE,
        size: iced::Pixels(APP_NAME_FONT_SIZE),
        ..canvas::Text::default()
    });

    // Window title (smaller, secondary line).
    let mut title = thumb.title.clone();
    if title.chars().count() > TITLE_MAX_CHARS {
        title = title.chars().take(TITLE_MAX_CHARS - 1).collect::<String>() + "…";
    }
    frame.fill_text(canvas::Text {
        content: title,
        position: iced::Point::new(left_x, below_y + APP_NAME_LINE_HEIGHT),
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
