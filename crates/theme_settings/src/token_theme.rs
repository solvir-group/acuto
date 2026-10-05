//! Syntax palettes that travel independently of the theme.
//!
//! A theme decides two largely unrelated things: what the editor's chrome looks
//! like, and what code looks like inside it. People have strong opinions about
//! the second that have nothing to do with the first -- someone arriving from VS
//! Code wants their keywords blue on whatever background they have chosen, and
//! someone who likes this fork's palette wants it on every theme they try.
//!
//! Binding the two together means the only way to keep your syntax colours is to
//! keep the whole theme, so this lifts the syntax layer out and lets it be
//! chosen separately.
//!
//! Each palette is written out in full for both appearances rather than derived
//! from the theme's own colours. Derivation is what produced the problem: a
//! palette computed from whatever accents a theme happened to ship gives a
//! different result per theme, which is the opposite of choosing your syntax
//! colours.

use gpui::{HighlightStyle, Hsla};
use settings::TokenTheme;

/// The syntax overrides for a token theme, or an empty list to leave the
/// theme's own syntax alone.
pub fn overrides(token_theme: TokenTheme, is_light: bool) -> Vec<(String, HighlightStyle)> {
    let palette = match token_theme {
        TokenTheme::Theme => return Vec::new(),
        TokenTheme::Acuto if is_light => ACUTO_LIGHT,
        TokenTheme::Acuto => ACUTO_DARK,
        TokenTheme::VsCode if is_light => VSCODE_LIGHT,
        TokenTheme::VsCode => VSCODE_DARK,
    };

    palette
        .iter()
        .filter_map(|(capture, hex)| {
            let color = parse_hex(hex)?;
            Some((
                (*capture).to_string(),
                HighlightStyle {
                    color: Some(color),
                    ..Default::default()
                },
            ))
        })
        .collect()
}

/// One entry per capture name a palette sets.
type Palette = &'static [(&'static str, &'static str)];

/// Acuto's own palette: violet leads.
///
/// Keywords, tags and markup structure all take the violet, so the shape of a
/// file -- the things that hold it together -- is one colour, and the things it
/// operates on are another. Deliberately no orange anywhere: numbers, constants
/// and escapes share a gold, which keeps literals warm without the whole file
/// looking like a build log.
const ACUTO_DARK: Palette = &[
    ("attribute", "#7fd3e8"),
    ("boolean", "#c792ea"),
    ("comment", "#5f7285"),
    ("comment.doc", "#7c8fa3"),
    ("constant", "#e8c877"),
    ("constructor", "#ffcb6b"),
    ("embedded", "#d6deeb"),
    ("emphasis", "#82aaff"),
    ("emphasis.strong", "#e8c877"),
    ("enum", "#ffcb6b"),
    ("function", "#82aaff"),
    ("function.method", "#82aaff"),
    ("hint", "#6f8296"),
    ("keyword", "#c792ea"),
    ("label", "#7fd3e8"),
    ("link_text", "#89ddff"),
    ("link_uri", "#89ddff"),
    ("number", "#e8c877"),
    ("operator", "#89ddff"),
    ("predictive", "#5f7285"),
    ("preproc", "#c792ea"),
    ("primary", "#d6deeb"),
    ("property", "#7fd3e8"),
    ("punctuation", "#8fa3b8"),
    ("punctuation.bracket", "#8fa3b8"),
    ("punctuation.delimiter", "#8fa3b8"),
    ("punctuation.list_marker", "#c792ea"),
    ("punctuation.special", "#c792ea"),
    ("string", "#c3e88d"),
    ("string.escape", "#e8c877"),
    ("string.regex", "#f07c98"),
    ("string.special", "#e8c877"),
    ("string.special.symbol", "#c3e88d"),
    ("tag", "#c792ea"),
    ("text.literal", "#c3e88d"),
    ("title", "#ffcb6b"),
    ("type", "#ffcb6b"),
    ("variable", "#d6deeb"),
    ("variable.special", "#c792ea"),
    ("variant", "#ffcb6b"),
];

/// The same palette, darkened for a light ground.
///
/// Not the dark values reused: a colour chosen to sit at 70% lightness against
/// near-black is unreadable at the same lightness against near-white, and the
/// usual shortcut of simply inverting the lightness throws the hues out of
/// relation with each other.
const ACUTO_LIGHT: Palette = &[
    ("attribute", "#0f7490"),
    ("boolean", "#7b3fa8"),
    ("comment", "#8a94a1"),
    ("comment.doc", "#6d7887"),
    ("constant", "#8a6a1f"),
    ("constructor", "#8a6a1f"),
    ("embedded", "#1b2531"),
    ("emphasis", "#2a5fa8"),
    ("emphasis.strong", "#8a6a1f"),
    ("enum", "#8a6a1f"),
    ("function", "#2a5fa8"),
    ("function.method", "#2a5fa8"),
    ("hint", "#8a94a1"),
    ("keyword", "#7b3fa8"),
    ("label", "#0f7490"),
    ("link_text", "#0f7490"),
    ("link_uri", "#0f7490"),
    ("number", "#8a6a1f"),
    ("operator", "#3b6b80"),
    ("predictive", "#949eab"),
    ("preproc", "#7b3fa8"),
    ("primary", "#1b2531"),
    ("property", "#0f7490"),
    ("punctuation", "#5a6775"),
    ("punctuation.bracket", "#5a6775"),
    ("punctuation.delimiter", "#5a6775"),
    ("punctuation.list_marker", "#7b3fa8"),
    ("punctuation.special", "#7b3fa8"),
    ("string", "#3f7f3f"),
    ("string.escape", "#8a6a1f"),
    ("string.regex", "#a5344f"),
    ("string.special", "#8a6a1f"),
    ("string.special.symbol", "#3f7f3f"),
    ("tag", "#7b3fa8"),
    ("text.literal", "#3f7f3f"),
    ("title", "#8a6a1f"),
    ("type", "#8a6a1f"),
    ("variable", "#1b2531"),
    ("variable.special", "#7b3fa8"),
    ("variant", "#8a6a1f"),
];

/// VS Code's Dark+ values.
///
/// Reproduced rather than approximated. The point of this option is that code
/// looks exactly as it did in the editor someone came from, and "nearly the
/// same blue" is worse than a different colour entirely -- it reads as a bug in
/// the theme rather than as a choice.
const VSCODE_DARK: Palette = &[
    ("attribute", "#9cdcfe"),
    ("boolean", "#569cd6"),
    ("comment", "#6a9955"),
    ("comment.doc", "#6a9955"),
    ("constant", "#4fc1ff"),
    ("constructor", "#4ec9b0"),
    ("embedded", "#d4d4d4"),
    ("emphasis", "#569cd6"),
    ("emphasis.strong", "#569cd6"),
    ("enum", "#4ec9b0"),
    ("function", "#dcdcaa"),
    ("function.method", "#dcdcaa"),
    ("hint", "#6a9955"),
    ("keyword", "#c586c0"),
    ("label", "#c8c8c8"),
    ("link_text", "#ce9178"),
    ("link_uri", "#ce9178"),
    ("number", "#b5cea8"),
    ("operator", "#d4d4d4"),
    ("predictive", "#6a9955"),
    ("preproc", "#c586c0"),
    ("primary", "#d4d4d4"),
    ("property", "#9cdcfe"),
    ("punctuation", "#d4d4d4"),
    ("punctuation.bracket", "#d4d4d4"),
    ("punctuation.delimiter", "#d4d4d4"),
    ("punctuation.list_marker", "#6796e6"),
    ("punctuation.special", "#d7ba7d"),
    ("string", "#ce9178"),
    ("string.escape", "#d7ba7d"),
    ("string.regex", "#d16969"),
    ("string.special", "#d7ba7d"),
    ("string.special.symbol", "#ce9178"),
    ("tag", "#569cd6"),
    ("text.literal", "#ce9178"),
    ("title", "#569cd6"),
    ("type", "#4ec9b0"),
    ("variable", "#9cdcfe"),
    ("variable.special", "#569cd6"),
    ("variant", "#4ec9b0"),
];

/// VS Code's Light+ values.
const VSCODE_LIGHT: Palette = &[
    ("attribute", "#e50000"),
    ("boolean", "#0000ff"),
    ("comment", "#008000"),
    ("comment.doc", "#008000"),
    ("constant", "#0070c1"),
    ("constructor", "#267f99"),
    ("embedded", "#000000"),
    ("emphasis", "#0000ff"),
    ("emphasis.strong", "#0000ff"),
    ("enum", "#267f99"),
    ("function", "#795e26"),
    ("function.method", "#795e26"),
    ("hint", "#008000"),
    ("keyword", "#af00db"),
    ("label", "#000000"),
    ("link_text", "#a31515"),
    ("link_uri", "#a31515"),
    ("number", "#098658"),
    ("operator", "#000000"),
    ("predictive", "#008000"),
    ("preproc", "#af00db"),
    ("primary", "#000000"),
    ("property", "#001080"),
    ("punctuation", "#000000"),
    ("punctuation.bracket", "#000000"),
    ("punctuation.delimiter", "#000000"),
    ("punctuation.list_marker", "#0000ff"),
    ("punctuation.special", "#ee0000"),
    ("string", "#a31515"),
    ("string.escape", "#ee0000"),
    ("string.regex", "#811f3f"),
    ("string.special", "#ee0000"),
    ("string.special.symbol", "#a31515"),
    ("tag", "#800000"),
    ("text.literal", "#a31515"),
    ("title", "#0000ff"),
    ("type", "#267f99"),
    ("variable", "#001080"),
    ("variable.special", "#0000ff"),
    ("variant", "#267f99"),
];

/// Parses `#rrggbb`.
///
/// Strict about the one shape these constants are written in, so a typo made
/// while editing a palette drops that capture rather than silently producing a
/// colour nobody chose. The alternative -- the theme crate's parser -- accepts
/// far more and would happily read a malformed constant as something plausible.
fn parse_hex(hex: &str) -> Option<Hsla> {
    let digits = hex.strip_prefix('#')?;
    if digits.len() != 6 || !digits.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let value = u32::from_str_radix(digits, 16).ok()?;
    Some(gpui::rgb(value).into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_theme_option_changes_nothing() {
        assert!(overrides(TokenTheme::Theme, false).is_empty());
        assert!(overrides(TokenTheme::Theme, true).is_empty());
    }

    #[test]
    fn every_palette_parses_completely() {
        // A typo in a hex constant would silently drop that one capture, which
        // shows up as a single token wearing the wrong colour -- the kind of
        // thing nobody notices for weeks.
        for (theme, is_light, expected) in [
            (TokenTheme::Acuto, false, ACUTO_DARK.len()),
            (TokenTheme::Acuto, true, ACUTO_LIGHT.len()),
            (TokenTheme::VsCode, false, VSCODE_DARK.len()),
            (TokenTheme::VsCode, true, VSCODE_LIGHT.len()),
        ] {
            assert_eq!(overrides(theme, is_light).len(), expected);
        }
    }

    #[test]
    fn light_and_dark_cover_the_same_captures() {
        // Otherwise switching appearance leaves some captures on the previous
        // palette and some on the theme's own.
        for (dark, light) in [(ACUTO_DARK, ACUTO_LIGHT), (VSCODE_DARK, VSCODE_LIGHT)] {
            let dark_captures: Vec<_> = dark.iter().map(|(capture, _)| *capture).collect();
            let light_captures: Vec<_> = light.iter().map(|(capture, _)| *capture).collect();
            assert_eq!(dark_captures, light_captures);
        }
    }

    #[test]
    fn acuto_gives_tags_and_keywords_the_same_violet() {
        // The whole idea of this palette: the things that hold a file together
        // are one colour.
        let dark: std::collections::HashMap<_, _> = ACUTO_DARK.iter().copied().collect();
        assert_eq!(dark["tag"], dark["keyword"]);
        assert_eq!(dark["tag"], "#c792ea");
    }

    #[test]
    fn parses_a_six_digit_hex_and_rejects_anything_else() {
        assert!(parse_hex("#c792ea").is_some());
        assert!(parse_hex("c792ea").is_none());
        assert!(parse_hex("#fff").is_none());
        assert!(parse_hex("#gggggg").is_none());
    }
}
