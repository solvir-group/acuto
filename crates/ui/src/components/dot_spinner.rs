use gpui::{Animation, AnimationExt, Hsla, px};
use std::time::Duration;

use crate::prelude::*;

/// How many dots make up one row or column of the grid.
const SIDE: usize = 3;

/// Positions on the ring, in the order the highlight travels around it.
///
/// The centre of the grid is deliberately absent: it never takes the highlight,
/// so the motion reads as a rotation rather than a scatter.
const RING: [(usize, usize); 8] = [
    (0, 0),
    (0, 1),
    (0, 2),
    (1, 2),
    (2, 2),
    (2, 1),
    (2, 0),
    (1, 0),
];

/// A square grid of dots with a highlight travelling around its edge.
///
/// Braille spinners are drawn from a font, which fixes them at the six or eight
/// dots a braille cell has. This is drawn from elements instead, so the grid can
/// be any size and every dot stays visible while it is dim rather than blinking
/// in and out.
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
            period: Duration::from_millis(900),
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

    /// How long the highlight takes to travel once around the ring.
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
        let dot_size = self.dot_size;
        let gap = self.gap;

        // The children are built inside the animator rather than up front: it is
        // handed a freshly constructed element every frame, so anything coloured
        // at construction time would stay frozen on the first frame.
        div()
            .flex()
            .flex_col()
            .gap(gap)
            .with_animation(
                self.id,
                Animation::new(self.period).repeat(),
                move |this, delta| {
                    let position = delta * RING.len() as f32;

                    this.children((0..SIDE).map(|row| {
                        div()
                            .flex()
                            .flex_row()
                            .gap(gap)
                            .children((0..SIDE).map(|column| {
                                let intensity = RING
                                    .iter()
                                    .position(|slot| *slot == (row, column))
                                    .map_or(0.0, |slot| ring_intensity(slot, position));

                                div()
                                    .size(dot_size)
                                    .rounded_full()
                                    .bg(color.opacity(0.2 + 0.8 * intensity))
                            }))
                    }))
                },
            )
    }
}

/// How brightly the dot at `slot` burns when the highlight is at `position`.
///
/// Distance is measured the short way round, so the trail behind the highlight
/// fades continuously across the seam between the last slot and the first rather
/// than snapping dark there.
fn ring_intensity(slot: usize, position: f32) -> f32 {
    /// How many slots the highlight bleeds into on either side.
    const SPREAD: f32 = 1.6;

    let count = RING.len() as f32;
    let raw = (slot as f32 - position).abs();
    let distance = raw.min(count - raw);
    (1.0 - distance / SPREAD).clamp(0.0, 1.0)
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
