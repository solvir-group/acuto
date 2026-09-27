//! Every agent working in this window, in one list.
//!
//! Agents run in their own git worktrees, and each worktree is its own
//! workspace in this window. That isolation is what makes running several at
//! once safe, and it is also what makes them invisible: a workspace you are not
//! looking at gives no sign that anything is happening in it. You end up
//! cycling through tabs to find out who finished.
//!
//! This reads across every workspace the window holds and answers the three
//! questions you actually have -- who is working, who is waiting on me, and how
//! much is waiting -- without leaving the one you are in.
//!
//! ```text
//!   CREW                                    3 agents
//!   -------------------------------------------------
//!   * agent/parser-crlf      working
//!     "handle CRLF in the parser"
//!
//!   . agent/flaky-tests      12 lines to review
//!     "quarantine the flaky terminal tests"
//!
//!   . main                   idle
//! ```

use std::time::Duration;

use acp_thread::{AcpThread, ThreadStatus};
use gpui::{
    Animation, AnimationExt, App, Context, Entity, EventEmitter, FocusHandle, Focusable,
    IntoElement, ParentElement, Render, Styled, Task, WeakEntity, Window, actions,
    pulsating_between,
};
use ui::{Divider, Tooltip, prelude::*};
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

use crate::AgentPanel;

actions!(
    agent,
    [
        /// Opens the crew: every agent working in this window.
        ToggleCrewFocus,
    ]
);

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ToggleCrewFocus, window, cx| {
            // Opens as a centre-pane tab: this panel is registered with the
            // workspace but not docked, so there is no dock focus to toggle.
            workspace.open_panel_as_tab::<CrewPanel>(window, cx);
        });
    })
    .detach();
}

/// One agent's worktree, as the crew list sees it.
struct CrewMember {
    workspace: Entity<Workspace>,
    /// The branch the agent is working on, or the worktree's folder name.
    name: SharedString,
    /// What the agent called the thread, if it has named it yet.
    task: Option<SharedString>,
    status: Option<ThreadStatus>,
    /// Files with changes the user has not answered, and the lines in them.
    waiting: (usize, usize),
    is_displayed: bool,
}

impl CrewMember {
    fn is_working(&self) -> bool {
        matches!(self.status, Some(ThreadStatus::Generating))
    }
}

pub struct CrewPanel {
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    /// Redraws while the panel is on screen.
    ///
    /// Deliberately a poll rather than a subscription to each agent: threads
    /// come and go as worktrees are created and archived, so the set to listen
    /// to changes underneath you, and rebuilding subscriptions from inside a
    /// render is how double-lease panics happen. A second's delay on a status
    /// dot is not worth that, and the work is one pass over a handful of
    /// workspaces.
    _tick: Option<Task<()>>,
}

impl CrewPanel {
    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: gpui::AsyncWindowContext,
    ) -> anyhow::Result<Entity<Self>> {
        workspace.update_in(&mut cx, |workspace, _window, cx| {
            let handle = workspace.weak_handle();
            cx.new(|cx| CrewPanel {
                workspace: handle,
                focus_handle: cx.focus_handle(),
                _tick: None,
            })
        })
    }

    /// Every workspace this window holds, newest-looking first: whoever is
    /// working, then whoever is waiting on you, then the rest.
    fn crew(&self, cx: &App) -> Vec<CrewMember> {
        let Some(workspace) = self.workspace.upgrade() else {
            return Vec::new();
        };
        let Some(multi) = workspace
            .read(cx)
            .multi_workspace()
            .and_then(|multi| multi.upgrade())
        else {
            return Vec::new();
        };

        let displayed = multi.read(cx).workspace().clone();
        let mut crew: Vec<CrewMember> = multi
            .read(cx)
            .workspaces()
            .map(|workspace| Self::member(workspace, &displayed, cx))
            .collect();

        // Working first, then anything waiting on the user, then by name so the
        // list does not reshuffle under the cursor between ticks.
        crew.sort_by(|left, right| {
            right
                .is_working()
                .cmp(&left.is_working())
                .then((right.waiting.0 > 0).cmp(&(left.waiting.0 > 0)))
                .then(left.name.cmp(&right.name))
        });
        crew
    }

    fn member(
        workspace: &Entity<Workspace>,
        displayed: &Entity<Workspace>,
        cx: &App,
    ) -> CrewMember {
        let project = workspace.read(cx).project();
        let name = project
            .read(cx)
            .active_repository(cx)
            .and_then(|repository| {
                repository
                    .read(cx)
                    .branch
                    .as_ref()
                    .map(|branch| branch.name().to_string())
            })
            .or_else(|| {
                project
                    .read(cx)
                    .worktree_root_names(cx)
                    .next()
                    .map(str::to_string)
            })
            .unwrap_or_else(|| "untitled".to_string());

        let thread = workspace
            .read(cx)
            .panel::<AgentPanel>(cx)
            .and_then(|panel| panel.read(cx).active_agent_thread(cx));

        let (task, status, waiting) = match thread {
            Some(thread) => (
                thread.read(cx).title(),
                Some(thread.read(cx).status()),
                Self::waiting_on_user(&thread, cx),
            ),
            None => (None, None, (0, 0)),
        };

        CrewMember {
            workspace: workspace.clone(),
            name: name.into(),
            task,
            status,
            waiting,
            is_displayed: workspace == displayed,
        }
    }

    /// Files and lines the agent changed that the user has not kept or rejected.
    fn waiting_on_user(thread: &Entity<AcpThread>, cx: &App) -> (usize, usize) {
        thread
            .read(cx)
            .action_log()
            .read(cx)
            .changed_buffers(cx)
            .fold((0, 0), |(files, lines), (buffer, diff)| {
                let snapshot = buffer.read(cx).snapshot();
                let hunks = diff.read(cx).snapshot(cx).hunks(&snapshot).count();
                (files + 1, lines + hunks)
            })
    }

    fn show(&mut self, member: &Entity<Workspace>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let Some(multi) = workspace
            .read(cx)
            .multi_workspace()
            .and_then(|multi| multi.upgrade())
        else {
            return;
        };
        let member = member.clone();
        let source = workspace.downgrade();
        // Deferred off this panel's lease: activating a workspace reaches back
        // into the dock this panel lives in.
        window.defer(cx, move |window, cx| {
            multi.update(cx, |multi, cx| {
                multi.activate(member, Some(source), window, cx);
            });
        });
    }

    fn render_member(&self, member: &CrewMember, cx: &mut Context<Self>) -> impl IntoElement {
        let working = member.is_working();
        let (files, lines) = member.waiting;
        let workspace = member.workspace.clone();

        let dot = div().size(px(6.)).rounded_full().bg(if working {
            cx.theme().status().info
        } else if files > 0 {
            cx.theme().status().modified
        } else {
            cx.theme().colors().text_disabled
        });
        // Breathing while it works, still while it waits. The only motion in
        // the panel, so it reads as activity rather than decoration.
        let dot = if working {
            dot.with_animation(
                "crew-working",
                Animation::new(Duration::from_secs(2))
                    .repeat()
                    .with_easing(pulsating_between(0.4, 1.0)),
                |dot, delta| dot.opacity(delta),
            )
            .into_any_element()
        } else {
            dot.into_any_element()
        };

        v_flex()
            .id(SharedString::from(format!("crew-{}", member.name)))
            .w_full()
            .px_2()
            .py_1p5()
            .gap_0p5()
            .rounded_md()
            .when(member.is_displayed, |row| {
                row.bg(cx.theme().colors().element_selected)
            })
            .hover(|row| row.bg(cx.theme().colors().element_hover))
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .child(dot)
                    .child(
                        Label::new(member.name.clone())
                            .size(LabelSize::Small)
                            .truncate(),
                    )
                    .child(div().flex_1())
                    .child(match (working, files) {
                        (true, _) => Label::new("working")
                            .size(LabelSize::XSmall)
                            .color(Color::Accent)
                            .into_any_element(),
                        (false, 0) => Label::new("idle")
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                            .into_any_element(),
                        // The number that matters: how much of your attention
                        // this agent is asking for.
                        (false, _) => Label::new(format!(
                            "{lines} line{} to review",
                            if lines == 1 { "" } else { "s" }
                        ))
                        .size(LabelSize::XSmall)
                        .color(Color::Modified)
                        .into_any_element(),
                    }),
            )
            .when_some(member.task.clone(), |row, task| {
                row.child(
                    Label::new(task)
                        .size(LabelSize::XSmall)
                        .color(Color::Muted)
                        .truncate(),
                )
            })
            .when(files > 1, |row| {
                row.child(
                    Label::new(format!("across {files} files"))
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
            })
            .tooltip(Tooltip::text("Show this agent's worktree"))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.show(&workspace, window, cx);
            }))
    }

    fn render_empty(&self, _cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .p_3()
            .gap_1()
            .child(Label::new("No agents yet").size(LabelSize::Small))
            .child(
                Label::new(
                    "Agents working in their own worktrees show up here, \
                     with what they are doing and what is waiting on you.",
                )
                .size(LabelSize::XSmall)
                .color(Color::Muted),
            )
    }
}

impl Render for CrewPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let crew = self.crew(cx);
        let working = crew.iter().filter(|member| member.is_working()).count();

        v_flex()
            .key_context("CrewPanel")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().panel_background)
            .child(
                h_flex()
                    .w_full()
                    .px_2()
                    .py_1p5()
                    .gap_2()
                    .child(Label::new("Crew").size(LabelSize::Small))
                    .child(div().flex_1())
                    .when(working > 0, |header| {
                        header.child(
                            Label::new(format!("{working} working"))
                                .size(LabelSize::XSmall)
                                .color(Color::Accent),
                        )
                    }),
            )
            .child(Divider::horizontal().color(ui::DividerColor::BorderVariant))
            .map(|panel| {
                if crew.is_empty() {
                    panel.child(self.render_empty(cx))
                } else {
                    panel.child(
                        v_flex()
                            .p_1()
                            .gap_0p5()
                            .children(
                                crew.iter()
                                    .map(|member| self.render_member(member, cx).into_any_element()),
                            ),
                    )
                }
            })
    }
}

impl Focusable for CrewPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for CrewPanel {}

impl Panel for CrewPanel {
    fn persistent_name() -> &'static str {
        "CrewPanel"
    }

    fn panel_key() -> &'static str {
        "crew_panel"
    }

    fn position(&self, _window: &Window, _cx: &App) -> DockPosition {
        DockPosition::Right
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

    fn default_size(&self, _window: &Window, _cx: &App) -> Pixels {
        px(280.)
    }

    fn icon(&self, _window: &Window, _cx: &App) -> Option<IconName> {
        Some(IconName::Person)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Crew")
    }

    /// The count of agents wanting your attention, on the dock button, so the
    /// panel is worth glancing at without opening it.
    fn icon_label(&self, _window: &Window, cx: &App) -> Option<String> {
        let waiting = self
            .crew(cx)
            .iter()
            .filter(|member| member.waiting.0 > 0)
            .count();
        (waiting > 0).then(|| waiting.to_string())
    }

    fn toggle_action(&self) -> Box<dyn gpui::Action> {
        Box::new(ToggleCrewFocus)
    }

    /// Keeps the panel live only while it is on screen. See `_tick`.
    fn set_active(&mut self, active: bool, _window: &mut Window, cx: &mut Context<Self>) {
        self._tick = active.then(|| {
            cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor()
                        .timer(Duration::from_secs(1))
                        .await;
                    if this.update(cx, |_, cx| cx.notify()).is_err() {
                        break;
                    }
                }
            })
        });
    }

    /// After the team panel, which is 8.
    fn activation_priority(&self) -> u32 {
        9
    }
}
