//! An agent thread living in the editor area rather than the side panel.
//!
//! The panel is the right home for a thread you are watching while you work,
//! and the wrong one for a thread you are reading closely: it is narrow, it
//! cannot be split, and only one thread is visible at a time. Wrapping a
//! [`ConversationView`] as a workspace [`Item`] lets a thread be dragged into a
//! pane, where it becomes an ordinary tab — splittable, movable between panes,
//! and side by side with the code it is talking about.
//!
//! This is a wrapper rather than an `Item` implementation on `ConversationView`
//! itself: the view is also rendered by the panel, and an `Item` carries tab
//! semantics (titles, close behaviour, nav history) that only make sense for
//! the pane copy.

use gpui::{
    AnyElement, App, Entity, EventEmitter, FocusHandle, Focusable, IntoElement, ParentElement,
    Render, Styled, Window,
};
use ui::prelude::*;
use workspace::item::{Item, TabContentParams};

use crate::conversation_view::ConversationView;

pub struct AgentThreadItem {
    conversation_view: Entity<ConversationView>,
}

impl AgentThreadItem {
    pub fn new(conversation_view: Entity<ConversationView>) -> Self {
        Self { conversation_view }
    }
}

impl Render for AgentThreadItem {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        // Focus is delegated to the conversation view rather than tracked here,
        // so the message editor keeps receiving keystrokes exactly as it does
        // in the panel. A wrapper that grabbed focus would swallow them.
        div().size_full().child(self.conversation_view.clone())
    }
}

impl Focusable for AgentThreadItem {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.conversation_view.focus_handle(cx)
    }
}

impl EventEmitter<()> for AgentThreadItem {}

impl Item for AgentThreadItem {
    type Event = ();

    /// A thread is not a location in the project, so it should not appear in
    /// back/forward navigation.
    fn include_in_nav_history() -> bool {
        false
    }

    fn tab_content_text(&self, _detail: usize, cx: &App) -> SharedString {
        self.conversation_view.read(cx).title(cx)
    }

    fn tab_content(&self, params: TabContentParams, _window: &Window, cx: &App) -> AnyElement {
        h_flex()
            .gap_1()
            .child(
                Icon::new(IconName::ZedAssistant)
                    .size(IconSize::XSmall)
                    .color(if params.selected {
                        Color::Default
                    } else {
                        Color::Muted
                    }),
            )
            .child(
                Label::new(self.tab_content_text(params.detail.unwrap_or(0), cx))
                    .size(LabelSize::Small)
                    .color(params.text_color())
                    .single_line(),
            )
            .into_any_element()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::ZedAssistant))
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        Some("Agent Thread Item")
    }
}
