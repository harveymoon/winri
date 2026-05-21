use iced::{
    Renderer, Theme, mouse,
    widget::{
        self,
        canvas::{self, Path, Stroke},
    },
};

use crate::{
    app::{self, model::BorderStyle, service::tiler::State, view},
    scroll_tiler::{BORDER_FADE_IN, BORDER_SETTLE_GRACE},
    utils::math::Bounds,
};

/// Visual thickness once settled — intentionally thinner than the rest-state
/// stroke since the user wanted a "calmer" indicator.
const SETTLED_BORDER_THICKNESS: f32 = 1.5;

pub fn view(app: &app::State) -> iced::Element<'_, app::Message> {
    match &app.mode {
        app::Mode::Tiler(tiler_state) => tiler_view(app, tiler_state),
        _ => view::empty(),
    }
}

fn tiler_view<'a>(app: &'a app::State, tiler_state: &'a State) -> iced::Element<'a, app::Message> {
    let Some(border_bounds) = tiler_state.current_border_bounds else {
        return view::empty();
    };

    // Border is hidden while motion is fresh; fades in over BORDER_FADE_IN
    // after BORDER_SETTLE_GRACE of quiet. If we've never moved, treat it as
    // fully settled.
    let alpha = match app.tiler.time_since_motion() {
        None => 1.0,
        Some(elapsed) if elapsed < BORDER_SETTLE_GRACE => 0.0,
        Some(elapsed) => {
            let fade_progress = (elapsed - BORDER_SETTLE_GRACE).as_secs_f32()
                / BORDER_FADE_IN.as_secs_f32();
            fade_progress.clamp(0.0, 1.0)
        }
    };

    if alpha <= 0.001 {
        return view::empty();
    }

    widget::canvas(TilerBorder {
        border_bounds,
        border_style: app.configuration.tiler_border_style,
        alpha,
    })
    .width(iced::Length::Fill)
    .height(iced::Length::Fill)
    .into()
}

struct TilerBorder {
    border_bounds: Bounds,
    border_style: BorderStyle,
    alpha: f32,
}

impl canvas::Program<app::Message> for TilerBorder {
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

        let path = Path::rounded_rectangle(
            self.border_bounds.position().into(),
            self.border_bounds.size().into(),
            self.border_style.radius.into(),
        );

        let mut color = self.border_style.color;
        color.a *= self.alpha;

        frame.stroke(
            &path,
            Stroke::default()
                .with_color(color)
                .with_width(SETTLED_BORDER_THICKNESS),
        );

        vec![frame.into_geometry()]
    }
}
