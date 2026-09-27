//! Project search as a dock panel.
//!
//! Upstream deploys project search into the centre pane, so searching costs you
//! an editor tab and the results sit where code should be. Docked beside the
//! tree it behaves like the tree: same edge, same width, swapped in and out
//! without disturbing anything you have open.
//!
//! The search itself is unchanged -- this hosts the same `ProjectSearchView`
//! the centre-pane tab uses, so query syntax, filters, replace and the results
//! editor all behave exactly as they did.

use gpui::{
    App, AsyncWindowContext, Context, Entity, EventEmitter, FocusHandle, Focusable, Pixels,
    WeakEntity, Window, actions, px,
};
use ui::prelude::*;
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

use crate::project_search::{ProjectSearch, ProjectSearchView};

actions!(
    search_panel,
    [
        /// Opens the search panel and puts the cursor in the query field.
        ToggleFocus
    ]
);

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<SearchPanel>(window, cx);
        });
    })
    .detach();
}

pub struct SearchPanel {
    view: Entity<ProjectSearchView>,
    focus_handle: FocusHandle,
    width: Option<Pixels>,
}

impl SearchPanel {
    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> anyhow::Result<Entity<Self>> {
        workspace.update_in(&mut cx, |workspace, window, cx| {
            let project = workspace.project().clone();
            let handle = cx.entity().downgrade();
            cx.new(|cx| {
                let search = cx.new(|cx| ProjectSearch::new(project, cx));
                let view =
                    cx.new(|cx| ProjectSearchView::new(handle, search, window, cx, None));
                Self {
                    view,
                    focus_handle: cx.focus_handle(),
                    width: None,
                }
            })
        })
    }
}

impl Render for SearchPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The query field normally lives in the pane's toolbar, which a dock
        // does not have, so the panel carries it itself. Without this the panel
        // renders only the results view and there is nowhere to type.
        let query = self.view.read(cx).query_editor.clone();

        v_flex()
            .key_context("SearchPanel")
            .track_focus(&self.focus_handle)
            .size_full()
            .child(
                div()
                    .flex_none()
                    .w_full()
                    .p_2()
                    .child(
                        div()
                            .w_full()
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .border_1()
                            .border_color(cx.theme().colors().border)
                            .bg(cx.theme().colors().element_background)
                            .child(query),
                    ),
            )
            .child(div().flex_1().min_h_0().child(self.view.clone()))
            .on_action(cx.listener(Self::confirm))
    }
}

impl SearchPanel {
    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        cx.stop_propagation();
        self.view.update(cx, |view, cx| {
            view.prompt_to_save_if_dirty_then_search(window, cx)
                .detach_and_log_err(cx);
        });
    }
}

impl EventEmitter<PanelEvent> for SearchPanel {}

impl Focusable for SearchPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Panel for SearchPanel {
    fn persistent_name() -> &'static str {
        "SearchPanel"
    }

    fn panel_key() -> &'static str {
        "search_panel"
    }

    /// Focus lands in the query field, because the only reason to open this is
    /// to type a query.
    fn activation_focus_handle(&self, cx: &App) -> FocusHandle {
        self.view.focus_handle(cx)
    }

    /// Beside the tree. Searching and browsing are the same act -- finding a
    /// file -- so they belong on the same edge rather than on opposite sides of
    /// the window.
    fn position(&self, _window: &Window, _cx: &App) -> DockPosition {
        DockPosition::Left
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(position, DockPosition::Left | DockPosition::Right)
    }

    fn set_position(
        &mut self,
        _position: DockPosition,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }

    /// Whatever the dock is already at. Results are a list like the tree is a
    /// list, so swapping between them should not move the edge.
    fn default_size(&self, _window: &Window, _cx: &App) -> Pixels {
        self.width.unwrap_or(px(320.))
    }

    fn icon(&self, _window: &Window, _cx: &App) -> Option<IconName> {
        Some(IconName::MagnifyingGlass)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Search")
    }

    fn toggle_action(&self) -> Box<dyn gpui::Action> {
        Box::new(ToggleFocus)
    }

    /// The numbers have to be unique -- `Dock` panics on a collision rather than
    /// picking an order -- and 0 through 9 are taken by the panels above.
    fn activation_priority(&self) -> u32 {
        10
    }
}
