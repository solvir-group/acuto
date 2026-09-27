//! Diagnostics as a bottom-dock panel.
//!
//! Upstream exposes diagnostics only as a centre-pane item, so it competes with
//! your code for the editor area. Wrapping it as a `Panel` puts it in the bottom
//! dock beside Terminal and Debug, which is where a problems list is expected to
//! live and where the dock's own tab strip can reach it.
//!
//! This is a thin wrapper rather than a reimplementation: it owns a
//! `ProjectDiagnosticsEditor` and delegates rendering and focus to it, so the
//! list itself stays exactly the one upstream maintains.

use gpui::{
    Context, Entity, EventEmitter, FocusHandle, Focusable, IntoElement, ParentElement, Pixels,
    Render, Styled, Window, actions, px,
};
use project::Project;
use ui::prelude::*;
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

use crate::ProjectDiagnosticsEditor;

actions!(
    problems_panel,
    [
        /// Toggles focus on the problems panel.
        ToggleFocus
    ]
);

pub struct ProblemsPanel {
    diagnostics: Entity<ProjectDiagnosticsEditor>,
    focus_handle: FocusHandle,
}

impl ProblemsPanel {
    pub fn new(
        project: Entity<Project>,
        workspace: gpui::WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let diagnostics = cx.new(|cx| {
            ProjectDiagnosticsEditor::new(true, project, workspace, window, cx)
        });
        Self {
            diagnostics,
            focus_handle: cx.focus_handle(),
        }
    }
}

impl ProblemsPanel {
    /// Constructed fresh each session rather than restored from serialized state:
    /// a diagnostics list is derived entirely from the project's current
    /// diagnostics, so there is nothing meaningful to persist.
    pub async fn load(
        workspace: gpui::WeakEntity<Workspace>,
        mut cx: gpui::AsyncWindowContext,
    ) -> anyhow::Result<Entity<Self>> {
        workspace.update_in(&mut cx, |workspace, window, cx| {
            let project = workspace.project().clone();
            let handle = cx.entity().downgrade();
            cx.new(|cx| ProblemsPanel::new(project, handle, window, cx))
        })
    }
}

impl Render for ProblemsPanel {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .track_focus(&self.focus_handle)
            .size_full()
            .child(self.diagnostics.clone())
    }
}

impl Focusable for ProblemsPanel {
    fn focus_handle(&self, cx: &gpui::App) -> FocusHandle {
        // Focus belongs to the list itself, so keyboard navigation lands on the
        // diagnostics rather than on an empty wrapper.
        self.diagnostics.focus_handle(cx)
    }
}

impl EventEmitter<PanelEvent> for ProblemsPanel {}

impl Panel for ProblemsPanel {
    fn persistent_name() -> &'static str {
        "ProblemsPanel"
    }

    fn panel_key() -> &'static str {
        "ProblemsPanel"
    }

    fn position(&self, _window: &Window, _cx: &gpui::App) -> DockPosition {
        DockPosition::Bottom
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        // A problems list is a wide, short surface; the side docks are the wrong
        // shape for it.
        matches!(position, DockPosition::Bottom)
    }

    fn set_position(&mut self, _: DockPosition, _: &mut Window, _: &mut Context<Self>) {}

    fn default_size(&self, _window: &Window, _cx: &gpui::App) -> Pixels {
        px(280.)
    }

    /// No status bar button.
    ///
    /// The error and warning counts at the left of the status bar open this
    /// panel, and they say how many of each there are first. A second control
    /// at the other end of the bar, showing only a triangle, was the same
    /// action with less information attached.
    fn icon(&self, _window: &Window, _cx: &gpui::App) -> Option<IconName> {
        None
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &gpui::App) -> Option<&'static str> {
        Some("Problems")
    }

    fn toggle_action(&self) -> Box<dyn gpui::Action> {
        Box::new(ToggleFocus)
    }

    /// Ordering among dock panel buttons. Sits just after the terminal (2), since
    /// both share the bottom dock and the terminal is reached more often.
    fn activation_priority(&self) -> u32 {
        4
    }
}
