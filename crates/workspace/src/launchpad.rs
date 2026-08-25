// ui::prelude re-exports gpui's prelude plus the builder traits these buttons
// need - `full_width` lives on FixedWidth, which must be in scope for the method
// to resolve at all - and Button, Label, LabelSize, Color along with them.
// Only the items it does not cover are imported explicitly.
use gpui::{EventEmitter, FocusHandle, Focusable};
use ui::prelude::*;
use util::ResultExt as _;

use crate::item::Item;

/// A tab offering the things you might want to start, rather than an empty
/// buffer.
///
/// Upstream's `+` creates an untitled file, which assumes the next thing you
/// want is always to type code. This gives the same click a short list of
/// entry points instead.
pub struct Launchpad {
    focus_handle: FocusHandle,
}

impl Launchpad {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
        }
    }
}

impl Launchpad {
    /// Actions are dispatched by name through the global registry: `workspace`
    /// does not depend on the crates that own them, and taking those
    /// dependencies for a handful of buttons would pull large subtrees into its
    /// rebuild graph. `build_action` returns a `Result`, so an action renamed
    /// upstream degrades to a logged warning and an inert button, not a panic.
    fn entry(id: &'static str, label: &'static str, action_name: &'static str) -> Button {
        Button::new(id, label)
            .full_width()
            .on_click(move |_, window, cx: &mut App| {
                if let Some(action) = cx.build_action(action_name, None).log_err() {
                    window.dispatch_action(action, cx);
                }
            })
    }
}

impl Render for Launchpad {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .child(
                v_flex()
                    .w_64()
                    .gap_1()
                    .child(
                        Label::new("Start something")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(Self::entry(
                        "launchpad-file",
                        "Open a File",
                        "file_finder::Toggle",
                    ))
                    .child(Self::entry(
                        "launchpad-terminal",
                        "New Terminal",
                        "terminal_panel::Toggle",
                    ))
                    .child(Self::entry(
                        "launchpad-agent",
                        "New Agent Thread",
                        "agent::NewThread",
                    ))
                    .child(Self::entry(
                        "launchpad-git",
                        "Git Status",
                        "git_panel::ToggleFocus",
                    )),
            )
    }
}

impl EventEmitter<()> for Launchpad {}

impl Focusable for Launchpad {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Item for Launchpad {
    type Event = ();

    /// Nothing here is a location, so it should not appear in back/forward.
    fn include_in_nav_history() -> bool {
        false
    }

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        "New Tab".into()
    }
}
