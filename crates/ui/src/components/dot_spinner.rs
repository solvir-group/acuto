use gpui::{Animation, AnimationExt, Hsla, px};
use std::time::Duration;

use crate::prelude::*;

/// How many dots make up one row or column of the grid.
const SIDE: usize = 3;

/// How far apart, in phase, two diagonally adjacent dots are.
///
/// The wave crosses the grid rather than lighting it at once: with a phase
/// offset per diagonal, the top-left corner leads and the bottom-right corner
/// trails, which is what makes it read as motion rather than as a flicker.
const DIAGONAL_PHASE: f32 = 0.11;

#[derive(IntoElement, RegisterComponent)]
pub struct DotSpinner {
    id: ElementId,
    dot_size: Pixels,
    gap: Pixels,
    period: Duration,
    color: Option<Hsla>,
}

impl DotSpinner {
    pub fn new(id: impl Into<ElementId>) -> Self {
        Self {
            id: id.into(),
            dot_size: px(2.),
            gap: px(2.),
            period: Duration::from_millis(1100),
            color: None,
        }
    }

    /// Diameter of a single dot.
    pub fn dot_size(mut self, dot_size: Pixels) -> Self {
        self.dot_size = dot_size;
        self
    }

    /// Space between adjacent dots.
    pub fn gap(mut self, gap: Pixels) -> Self {
        self.gap = gap;
        self
    }

    /// How long one wave takes to cross the grid.
    pub fn period(mut self, period: Duration) -> Self {
        self.period = period;
        self
    }

    pub fn color(mut self, color: Hsla) -> Self {
        self.color = Some(color);
        self
    }
}

impl RenderOnce for DotSpinner {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let color = self.color.unwrap_or_else(|| cx.theme().colors().text_muted);
        let cell = self.dot_size;
        let gap = self.gap;

        // The children are built inside the animator rather than up front: it
        // is handed a freshly constructed element every frame, so anything
        // coloured at construction time would stay frozen on the first frame.
        div()
            .flex()
            .flex_col()
            .gap(gap)
            .with_animation(
                self.id,
                Animation::new(self.period).repeat(),
                move |this, delta| {
                    this.children((0..SIDE).map(|row| {
                        div()
                            .flex()
                            .flex_row()
                            .gap(gap)
                            .children((0..SIDE).map(|column| {
                                let phase =
                                    delta - (row + column) as f32 * DIAGONAL_PHASE;
                                let intensity = wave(phase);

                                // The cell is a fixed box and the dot inside it
                                // grows and shrinks. Sizing the cell itself
                                // would move every other dot each frame, and a
                                // grid that jitters reads as broken rather than
                                // as alive.
                                let dot = cell * (0.45 + 0.55 * intensity);
                                div()
                                    .size(cell)
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .child(
                                        div()
                                            .size(dot)
                                            .rounded_full()
                                            .bg(color.opacity(0.18 + 0.82 * intensity)),
                                    )
                            }))
                    }))
                },
            )
    }
}

/// A single smooth pulse per cycle, wrapping cleanly.
///
/// `phase` is the animation's progress minus this dot's offset, so it can be
/// negative or above one; it is wrapped rather than clamped, which is what
/// makes the wave continue off one edge and arrive at the other instead of
/// stalling at the ends.
///
/// Raised to a power so the bright part is narrow: a sine on its own lights
/// most of the grid most of the time, which is a grid that glows rather than a
/// wave that travels.
fn wave(phase: f32) -> f32 {
    let wrapped = phase.rem_euclid(1.0);
    let pulse = (wrapped * std::f32::consts::TAU).sin().max(0.0);
    pulse * pulse
}

impl Component for DotSpinner {
    fn scope() -> ComponentScope {
        ComponentScope::Loading
    }

    fn name() -> &'static str {
        "DotSpinner"
    }

    fn description() -> &'static str {
        "A square grid of dots with a highlight travelling around its edge, for         indeterminate work where a text spinner would be fixed at the six or         eight dots a braille cell has."
    }

    fn preview(_window: &mut Window, _cx: &mut App) -> AnyElement {
        h_flex()
            .gap_4()
            .child(DotSpinner::new("dot-spinner-preview-small"))
            .child(
                DotSpinner::new("dot-spinner-preview-large")
                    .dot_size(px(4.))
                    .gap(px(3.)),
            )
            .into_any_element()
    }
}
