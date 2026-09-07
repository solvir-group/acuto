//! Text with a highlight travelling through it.
//!
//! The house loading label types its text out and then cycles trailing dots.
//! That reads as "a thing is loading". A shimmer reads as "a thing is
//! thinking" -- the text is already there and whole, and something is moving
//! through it. It is the difference between a progress bar and a pulse, and it
//! is what Claude Code uses while it works.

use std::time::Duration;

use gpui::{Animation, AnimationExt, Hsla};

use crate::prelude::*;

/// How long the highlight takes to cross the text.
const SWEEP: Duration = Duration::from_millis(1_800);

/// How wide the bright band is, as a fraction of the text's length.
///
/// Narrow enough to read as a gleam rather than the whole phrase brightening,
/// wide enough to cover two or three characters in a short word.
const BAND: f32 = 0.35;

/// Alpha of text the highlight is not currently over.
///
/// Not fully dim: the word has to stay readable at every moment of the sweep,
/// because its job is to say what the agent is doing.
const RESTING_ALPHA: f32 = 0.45;

#[derive(IntoElement)]
pub struct ShimmerLabel {
    id: ElementId,
    text: SharedString,
    size: LabelSize,
    color: Hsla,
}

impl ShimmerLabel {
    pub fn new(id: impl Into<ElementId>, text: impl Into<SharedString>, color: Hsla) -> Self {
        Self {
            id: id.into(),
            text: text.into(),
            size: LabelSize::Default,
            color,
        }
    }

    pub fn size(mut self, size: LabelSize) -> Self {
        self.size = size;
        self
    }
}

impl RenderOnce for ShimmerLabel {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        // One label per character, because a single label is one colour. The
        // strings are one character each, so this is cheap for the handful of
        // words it is ever asked to draw.
        let characters: Vec<SharedString> = self
            .text
            .chars()
            // A plain space between labels collapses to nothing in a flex row,
            // which would close the gaps in a two-word phrase.
            .map(|character| {
                SharedString::from(if character == ' ' {
                    "\u{00a0}".to_string()
                } else {
                    character.to_string()
                })
            })
            .collect();

        let count = characters.len().max(1) as f32;
        let color = self.color;
        let size = self.size;

        h_flex().with_animation(
            self.id,
            Animation::new(SWEEP).repeat(),
            move |this, delta| {
                // The band starts fully off the left edge and finishes fully
                // past the right, so the highlight enters and leaves rather
                // than appearing in the middle of the first character.
                let head = delta * (1.0 + 2.0 * BAND) - BAND;

                this.children(characters.iter().enumerate().map(|(index, character)| {
                    let position = index as f32 / count;
                    let distance = (position - head).abs() / BAND;
                    let closeness = (1.0 - distance).max(0.0);
                    // Squared: a linear ramp reads as a moving block, and the
                    // square gives a tight peak with a soft falloff.
                    let alpha = RESTING_ALPHA + (1.0 - RESTING_ALPHA) * closeness * closeness;

                    Label::new(character.clone())
                        .size(size)
                        .color(Color::Custom(Hsla { a: alpha, ..color }))
                }))
            },
        )
    }
}
