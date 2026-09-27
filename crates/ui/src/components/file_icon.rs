//! Drawing a file's icon, whichever kind of icon theme is active.
//!
//! Two kinds exist and they need different treatment. A monochrome glyph is
//! drawn as a silhouette and filled with a colour the editor chooses, which is
//! what keeps it legible on any background and lets the project panel colour it
//! by language. An icon with real artwork in it -- gradients, several colours,
//! the actual brand mark -- has to be drawn as it was authored, because
//! flattening it to a silhouette produces a solid block in the shape of a logo.
//!
//! `svg()` does the first, `img()` does the second, and the difference is not
//! something every call site should have to know about.

use gpui::{Hsla, SharedString, img};

use crate::{Icon, IconName, IconSize, prelude::*};

/// A file or directory icon, drawn the way its icon theme intends.
#[derive(IntoElement)]
pub struct FileIcon {
    path: Option<SharedString>,
    colored: bool,
    size: IconSize,
    /// Tint for monochrome icons. Ignored when the theme supplies its own
    /// colour, since there is nothing sensible to do with both.
    color: Option<Hsla>,
    /// Drawn when the theme has no icon for this file.
    fallback: IconName,
}

impl FileIcon {
    /// `path` is the icon the active icon theme resolved for the file, and
    /// `colored` is that theme's own declaration about what kind of icons it
    /// holds.
    pub fn new(path: Option<SharedString>, colored: bool) -> Self {
        Self {
            path,
            colored,
            size: IconSize::Small,
            color: None,
            fallback: IconName::File,
        }
    }

    pub fn size(mut self, size: IconSize) -> Self {
        self.size = size;
        self
    }

    /// Tint applied only to monochrome icons.
    pub fn color(mut self, color: Hsla) -> Self {
        self.color = Some(color);
        self
    }

    /// The icon drawn when the theme has none for this file.
    pub fn fallback(mut self, fallback: IconName) -> Self {
        self.fallback = fallback;
        self
    }
}

impl RenderOnce for FileIcon {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let Some(path) = self.path else {
            return Icon::new(self.fallback)
                .size(self.size)
                .color(self.color.map_or(Color::Muted, Color::Custom))
                .into_any_element();
        };

        if !self.colored {
            return Icon::from_path(path)
                .size(self.size)
                .color(self.color.map_or(Color::Muted, Color::Custom))
                .into_any_element();
        }

        // Sized in a box rather than scaled to fit: these are drawn on a 32-unit
        // grid with their own padding, and letting them fill the box makes them
        // noticeably larger than the monochrome set beside them in menus and
        // tabs.
        let size = self.size.rems();
        div()
            .size(size)
            .flex_none()
            .child(img(path).size(size))
            .into_any_element()
    }
}
