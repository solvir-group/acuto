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

/// The figure the highlight traces through the grid.
///
/// Each is a different, immediately nameable motion rather than a different
/// speed or colour of the same one: what you recognise is "the one that goes
/// round" or "the one that opens from the middle", which survives being small,
/// being glanced at, and being looked at by someone who does not separate one
/// hue from another.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DotPattern {
    /// A band travelling corner to corner.
    #[default]
    Diagonal,
    /// Opening from the middle and washing outwards.
    Bloom,
    /// One highlight running round the eight outer dots, centre held steady.
    Orbit,
    /// Columns lighting left to right.
    Sweep,
    /// The whole grid breathing together.
    Pulse,
}

impl DotPattern {
    /// The phase offset for one cell, in turns.
    ///
    /// Subtracted from the animation's progress, so a larger offset means that
    /// cell lights later.
    fn offset(self, row: usize, column: usize) -> f32 {
        match self {
            DotPattern::Diagonal => (row + column) as f32 * DIAGONAL_PHASE,
            // Chebyshev distance from the middle, so a 3x3 grid has exactly two
            // rings: the centre, then everything around it.
            DotPattern::Bloom => {
                let middle = (SIDE / 2) as isize;
                let distance = (row as isize - middle)
                    .abs()
                    .max((column as isize - middle).abs());
                distance as f32 * 0.3
            }
            // Around the ring in order. The centre has no place on a ring, so
            // it sits at a fixed dim value rather than pretending to travel.
            DotPattern::Orbit => match ring_position(row, column) {
                Some(position) => position as f32 / RING_LENGTH as f32,
                None => 0.5,
            },
            DotPattern::Sweep => column as f32 * 0.22,
            DotPattern::Pulse => 0.0,
        }
    }

    /// Whether this cell moves at all under this pattern.
    ///
    /// Only the orbit has a stationary cell. Holding the centre still is what
    /// makes the ring read as rotation rather than as nine dots flickering.
    fn is_static(self, row: usize, column: usize) -> bool {
        self == DotPattern::Orbit && ring_position(row, column).is_none()
    }
}

/// How many cells lie on the outer ring of the grid.
const RING_LENGTH: usize = SIDE * SIDE - 1;

/// Where a cell sits when walking the outer ring clockwise from the top left.
///
/// `None` for the centre, which is not on the ring.
fn ring_position(row: usize, column: usize) -> Option<usize> {
    const RING: [(usize, usize); RING_LENGTH] = [
        (0, 0),
        (0, 1),
        (0, 2),
        (1, 2),
        (2, 2),
        (2, 1),
        (2, 0),
        (1, 0),
    ];
    RING.iter()
        .position(|cell| *cell == (row, column))
}

#[derive(IntoElement, RegisterComponent)]
pub struct DotSpinner {
    id: ElementId,
    dot_size: Pixels,
    gap: Pixels,
    period: Duration,
    color: Option<Hsla>,
    pattern: DotPattern,
}

impl DotSpinner {
    pub fn new(id: impl Into<ElementId>) -> Self {
        Self {
            id: id.into(),
            dot_size: px(2.),
            gap: px(2.),
            period: Duration::from_millis(1100),
            color: None,
            pattern: DotPattern::default(),
        }
    }

    /// The figure the highlight traces through the grid.
    pub fn pattern(mut self, pattern: DotPattern) -> Self {
        self.pattern = pattern;
        self
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
        let pattern = self.pattern;

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
                                let delta = eased(delta);
                                let intensity = if pattern.is_static(row, column) {
                                    // Dim but present. Dropping it entirely
                                    // would leave a hole in the middle of the
                                    // grid, which reads as a missing dot rather
                                    // than as a still one.
                                    0.15
                                } else {
                                    wave(delta - pattern.offset(row, column))
                                };

                                // The cell is a fixed box and the dot inside it
                                // grows and shrinks. Sizing the cell itself
                                // would move every other dot each frame, and a
                                // grid that jitters reads as broken rather than
                                // as alive.
                                // Size and brightness together. Growing a
                                // dot without lightening it reads as the grid
                                // breathing; doing both reads as the light
                                // arriving at it.
                                let dot = cell * (0.42 + 0.58 * intensity);

                                div()
                                    .size(cell)
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .child(
                                        div()
                                            .size(dot)
                                            .rounded_full()
                                            .bg(color.opacity(0.16 + 0.84 * intensity)),
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

    // A comet rather than a blink: a bright head with a tail trailing behind it.
    //
    // The old curve was a squared sine, which is near zero for most of its
    // cycle -- so at any moment eight of the nine dots were dark and the grid
    // read as a single dot moving through an empty field. A cosine raised to a
    // low power stays lit much longer either side of the peak, which is what
    // makes the highlight look like it is moving *through* the dots rather than
    // jumping between them.
    const TAIL: f32 = 2.2;
    let head = (wrapped * std::f32::consts::TAU).cos() * 0.5 + 0.5;
    let comet = head.powf(TAIL);

    // A floor, so an unlit dot is still visibly a dot. Without it the grid
    // changes shape as the highlight passes, and a shape that changes reads as
    // a glitch rather than as an animation.
    0.12 + 0.88 * comet
}

/// Eases the highlight's travel around the ring.
///
/// Nothing physical moves at a constant rate, and a highlight that does looks
/// mechanical however pretty the dots are. This runs quick through the straight
/// and slows into the turn, which is the difference between a rotating light and
/// something with weight to it.
fn eased(delta: f32) -> f32 {
    let wrapped = delta.rem_euclid(1.0);
    // A gentle sine ease applied to the phase itself rather than to the
    // brightness: the dots keep their own curve, the *timing* is what bends.
    const SWING: f32 = 0.08;
    wrapped - SWING * (wrapped * std::f32::consts::TAU).sin()
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
