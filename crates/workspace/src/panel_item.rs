//! A dock panel shown as a centre-pane tab.
//!
//! A window has one sidebar per edge, so every panel docked to the right
//! competes with the agent for the same strip. Git, the outline, team notes and
//! the crew are all things you read alongside code rather than while writing a
//! prompt, so they open where code opens: as tabs you can split, reorder and
//! close.
//!
//! The panel entity is unchanged -- this only re-parents its view -- so a panel
//! keeps working exactly as it did in a dock.

use std::sync::Arc;

use gpui::{App, Context, EntityId, EventEmitter, FocusHandle, Focusable, Window};
use ui::prelude::*;

use crate::{dock::PanelHandle, item::Item};

pub struct PanelItem {
    panel: Arc<dyn PanelHandle>,
    /// Captured when the tab is opened: `Item::tab_content_text` is handed only
    /// an `App`, while a panel names itself given a `Window` as well.
    name: SharedString,
    icon: Option<IconName>,
}

impl PanelItem {
    pub fn new(panel: Arc<dyn PanelHandle>, window: &Window, cx: &App) -> Self {
        let name = panel
            .icon_tooltip(window, cx)
            .unwrap_or_else(|| panel.persistent_name());
        Self {
            name: name.into(),
            icon: panel.icon(window, cx),
            panel,
        }
    }

    /// Which panel this tab is showing, so a second request to open it can
    /// activate the tab that already exists rather than stacking another.
    pub fn panel_id(&self) -> EntityId {
        self.panel.panel_id()
    }
}

impl Render for PanelItem {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.panel.to_any())
    }
}

impl EventEmitter<()> for PanelItem {}

impl Focusable for PanelItem {
    /// The panel's own handle, so focus lands inside the panel rather than on
    /// the wrapper and the panel's key bindings stay live.
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.panel.panel_focus_handle(cx)
    }
}

impl Item for PanelItem {
    type Event = ();

    /// None of these are a location in the project, so they do not belong in
    /// back and forward.
    fn include_in_nav_history() -> bool {
        false
    }

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        self.name.clone()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<ui::Icon> {
        self.icon.map(ui::Icon::new)
    }
}
