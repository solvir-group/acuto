use crate::prelude::*;
use gpui::{Animation, AnimationExt, FontWeight};
use std::time::Duration;

/// Different types of spinner animations
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub enum SpinnerVariant {
    #[default]
    Dots,
    DotsVariant,
    Sand,
    /// Claude Code's mascot: a sparkle that swells and settles.
    ///
    /// The frames are the ones the Claude Code CLI itself cycles, because the
    /// point of this variant is recognition. Someone who has used Claude in a
    /// terminal should see the same thing thinking at them here, and know
    /// without reading a label which agent is running.
    Claude,
}

/// A spinner indication, based on the label component, that loops through
/// frames of the specified animation. It implements `LabelCommon` as well.
///
/// # Default Example
///
/// ```
/// use ui::{SpinnerLabel};
///
/// SpinnerLabel::new();
/// ```
///
/// # Variant Example
///
/// ```
/// use ui::{SpinnerLabel};
///
/// SpinnerLabel::dots_variant();
/// ```
#[derive(IntoElement, RegisterComponent)]
pub struct SpinnerLabel {
    base: Label,
    variant: SpinnerVariant,
    frames: Vec<&'static str>,
    duration: Duration,
}

impl SpinnerVariant {
    fn frames(&self) -> Vec<&'static str> {
        match self {
            SpinnerVariant::Dots => vec!["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"],
            SpinnerVariant::DotsVariant => vec!["⣼", "⣹", "⢻", "⠿", "⡟", "⣏", "⣧", "⣶"],
            SpinnerVariant::Sand => vec![
                "⠁", "⠂", "⠄", "⡀", "⡈", "⡐", "⡠", "⣀", "⣁", "⣂", "⣄", "⣌", "⣔", "⣤", "⣥", "⣦",
                "⣮", "⣶", "⣷", "⣿", "⡿", "⠿", "⢟", "⠟", "⡛", "⠛", "⠫", "⢋", "⠋", "⠍", "⡉", "⠉",
                "⠑", "⠡", "⢁",
            ],
            // Grows to the full sparkle and comes back down, rather than
            // looping in one direction: the breathing motion is what reads as
            // "thinking" instead of "loading".
            // U+2733 is deliberately absent, and U+2732 stands in for it.
            //
            // It is the only frame in the CLI's sequence with an emoji
            // presentation, and the UI font has no glyph for it, so Windows
            // font fallback resolved that one frame to Segoe UI Emoji and drew
            // the colour emoji: a green flash, once per cycle, in a mark that
            // is one colour throughout. U+2732 is the same weight at the same
            // point in the swell and has no emoji form.
            //
            // U+FE0E on every frame is the belt to that pair of braces: it
            // pins each one to its text presentation, so a future font cannot
            // decide some other frame deserves a colour either.
            SpinnerVariant::Claude => {
                vec![
                    "·\u{fe0e}",
                    "✢\u{fe0e}",
                    "✲\u{fe0e}",
                    "∗\u{fe0e}",
                    "✻\u{fe0e}",
                    "✽\u{fe0e}",
                    "✻\u{fe0e}",
                    "∗\u{fe0e}",
                    "✲\u{fe0e}",
                    "✢\u{fe0e}",
                ]
            }
        }
    }

    fn duration(&self) -> Duration {
        match self {
            SpinnerVariant::Dots => Duration::from_millis(1000),
            SpinnerVariant::DotsVariant => Duration::from_millis(1000),
            SpinnerVariant::Sand => Duration::from_millis(2000),
            // Slower than the loaders. A fast pulse reads as urgency; this one
            // is meant to feel like something considering the question.
            SpinnerVariant::Claude => Duration::from_millis(1400),
        }
    }

    fn animation_id(&self) -> &'static str {
        match self {
            SpinnerVariant::Dots => "spinner_label_dots",
            SpinnerVariant::DotsVariant => "spinner_label_dots_variant",
            SpinnerVariant::Sand => "spinner_label_dots_variant_2",
            SpinnerVariant::Claude => "spinner_label_claude",
        }
    }
}

impl SpinnerLabel {
    pub fn new() -> Self {
        Self::with_variant(SpinnerVariant::default())
    }

    pub fn with_variant(variant: SpinnerVariant) -> Self {
        let frames = variant.frames();
        let duration = variant.duration();

        SpinnerLabel {
            base: Label::new(frames[0]).color(Color::Muted),
            variant,
            frames,
            duration,
        }
    }

    pub fn dots() -> Self {
        Self::with_variant(SpinnerVariant::Dots)
    }

    pub fn dots_variant() -> Self {
        Self::with_variant(SpinnerVariant::DotsVariant)
    }

    pub fn sand() -> Self {
        Self::with_variant(SpinnerVariant::Sand)
    }

    pub fn claude() -> Self {
        Self::with_variant(SpinnerVariant::Claude)
    }
}

impl LabelCommon for SpinnerLabel {
    fn size(mut self, size: LabelSize) -> Self {
        self.base = self.base.size(size);
        self
    }

    fn weight(mut self, weight: FontWeight) -> Self {
        self.base = self.base.weight(weight);
        self
    }

    fn line_height_style(mut self, line_height_style: LineHeightStyle) -> Self {
        self.base = self.base.line_height_style(line_height_style);
        self
    }

    fn color(mut self, color: Color) -> Self {
        self.base = self.base.color(color);
        self
    }

    fn strikethrough(mut self) -> Self {
        self.base = self.base.strikethrough();
        self
    }

    fn italic(mut self) -> Self {
        self.base = self.base.italic();
        self
    }

    fn alpha(mut self, alpha: f32) -> Self {
        self.base = self.base.alpha(alpha);
        self
    }

    fn underline(mut self) -> Self {
        self.base = self.base.underline();
        self
    }

    fn truncate(mut self) -> Self {
        self.base = self.base.truncate();
        self
    }

    fn single_line(mut self) -> Self {
        self.base = self.base.single_line();
        self
    }

    fn buffer_font(mut self, cx: &App) -> Self {
        self.base = self.base.buffer_font(cx);
        self
    }

    fn inline_code(mut self, cx: &App) -> Self {
        self.base = self.base.inline_code(cx);
        self
    }
}

impl RenderOnce for SpinnerLabel {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let frames = self.frames.clone();
        let duration = self.duration;

        self.base.with_animation(
            self.variant.animation_id(),
            Animation::new(duration).repeat(),
            move |mut label, delta| {
                let frame_index = (delta * frames.len() as f32) as usize % frames.len();

                label.set_text(frames[frame_index]);
                label
            },
        )
    }
}

impl Component for SpinnerLabel {
    fn scope() -> ComponentScope {
        ComponentScope::Loading
    }

    fn name() -> &'static str {
        "Spinner Label"
    }

    fn sort_name() -> &'static str {
        "Spinner Label"
    }

    fn description() -> &'static str {
        "A text-based loading indicator that animates through a sequence of \
        unicode glyphs to show ongoing or indeterminate work."
    }

    fn preview(_window: &mut Window, _cx: &mut App) -> AnyElement {
        let examples = vec![
            single_example("Default", SpinnerLabel::new().into_any_element()),
            single_example(
                "Dots Variant",
                SpinnerLabel::dots_variant().into_any_element(),
            ),
            single_example("Sand Variant", SpinnerLabel::sand().into_any_element()),
        ];

        example_group(examples).vertical().into_any_element()
    }
}
