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

    // Text only. The pane draws `tab_icon` beside this, so an icon here as
    // well put two sparkles on every agent tab.
    fn tab_content(&self, params: TabContentParams, _window: &Window, cx: &App) -> AnyElement {
        Label::new(self.tab_content_text(params.detail.unwrap_or(0), cx))
            .size(LabelSize::Small)
            .color(params.text_color())
            .single_line()
            .into_any_element()
    }

    /// The mark of whichever agent is in the thread.
    ///
    /// A Claude thread carries Claude's logo, a Gemini thread Gemini's. With
    /// several agent tabs open, one shared assistant glyph makes them
    /// indistinguishable until you read every title -- which is the moment the
    /// icon was supposed to save.
    fn tab_icon(&self, _window: &Window, cx: &App) -> Option<Icon> {
        let conversation_view = self.conversation_view.read(cx);
        let agent = conversation_view.agent_server();

        // A known agent always wears its own mark in its brand colour (ink on a
        // light theme), from the first frame: while it is still connecting as
        // much as once it has loaded, so the icon never swaps under you.
        if let Some(brand) = crate::conversation_view::claude_brand::AgentBrand::for_agent_in(
            agent.agent_id().0.as_ref(),
            cx,
        ) {
            return Some(Icon::new(brand.icon).color(Color::Custom(brand.accent)));
        }

        let Some(thread_view) = conversation_view.root_thread_view() else {
            return Some(Icon::new(agent.logo()));
        };
        let thread_view = thread_view.read(cx);

        // An agent installed with its own artwork ships an SVG; the built-in
        // ones have an icon in the enum. Preferring the SVG means a custom
        // agent shows its own mark rather than the generic assistant glyph.
        if let Some(path) = thread_view.agent_icon_from_external_svg.clone() {
            return Some(Icon::from_external_svg(path));
        }
        Some(Icon::new(thread_view.agent_icon))
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        Some("Agent Thread Item")
    }
}
