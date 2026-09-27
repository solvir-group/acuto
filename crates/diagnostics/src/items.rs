use std::time::Duration;

use editor::{Editor, MultiBufferOffset};
use gpui::{
    App, Context, Entity, EventEmitter, IntoElement, ParentElement, Render, Styled, Subscription,
    Task, WeakEntity, Window,
};
use language::Diagnostic;
use project::project_settings::{GoToDiagnosticSeverityFilter, ProjectSettings};
use settings::Settings;
use ui::{Button, ButtonLike, Color, Icon, IconName, Label, Tooltip, h_flex, prelude::*};
use util::ResultExt;
use workspace::{HideStatusItem, StatusItemView, ToolbarItemEvent, Workspace, item::ItemHandle};

use crate::{Deploy, IncludeWarnings, problems_panel::ProblemsPanel};

/// The status bar item that displays diagnostic counts.
pub struct DiagnosticIndicator {
    summary: project::DiagnosticSummary,
    workspace: WeakEntity<Workspace>,
    current_diagnostic: Option<Diagnostic>,
    active_editor: Option<WeakEntity<Editor>>,
    _observe_active_editor: Option<Subscription>,

    diagnostics_update: Task<()>,
    diagnostic_summary_update: Task<()>,
}

impl Render for DiagnosticIndicator {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let indicator = h_flex().gap_2().min_w_0().overflow_x_hidden();
        if !ProjectSettings::get_global(cx).diagnostics.button {
            return indicator.hidden();
        }

        // Both counts, always, the way every other editor's status bar does it.
        //
        // Upstream collapses a clean project to a single tick and only grows
        // the counters once something is wrong. That means the one place you
        // would look to answer "does this compile?" changes shape depending on
        // the answer, so a clean project and a project whose diagnostics have
        // not arrived yet look nothing like each other -- and there is nowhere
        // stable to glance at. A zero is an answer; a tick is a different
        // widget.
        let error_count = self.summary.error_count;
        let warning_count = self.summary.warning_count;

        let count = |icon: IconName, count: usize, color: Color| {
            h_flex()
                .gap_0p5()
                .child(
                    Icon::new(icon)
                        .size(IconSize::Small)
                        // Muted at zero: the row stays put, but nothing about
                        // it asks for attention until there is something to
                        // attend to.
                        .color(if count == 0 { Color::Muted } else { color }),
                )
                .child(
                    Label::new(count.to_string())
                        .size(LabelSize::Small)
                        .color(if count == 0 { Color::Muted } else { color }),
                )
        };

        let diagnostic_indicator = h_flex()
            .gap_2()
            .child(count(IconName::XCircle, error_count, Color::Error))
            .child(count(IconName::Warning, warning_count, Color::Warning));

        let status = if let Some(diagnostic) = &self.current_diagnostic {
            let message = diagnostic
                .message
                .split_once('\n')
                .map_or(&*diagnostic.message, |(first, _)| first);
            let diagnostics_already_active = self.any_active_diagnostics(cx);
            let tooltip = if !diagnostics_already_active {
                "Expand Diagnostics"
            } else {
                "Next Diagnostic"
            };
            Some(
                Button::new("diagnostic_message", SharedString::new(message))
                    .label_size(LabelSize::Small)
                    .truncate(true)
                    .tab_index(0isize)
                    .tooltip(move |_window, cx| {
                        Tooltip::for_action(
                            tooltip,
                            &editor::actions::GoToDiagnostic::default(),
                            cx,
                        )
                    })
                    .on_click(
                        cx.listener(|this, _, window, cx| this.go_to_next_diagnostic(window, cx)),
                    ),
            )
        } else {
            None
        };

        let diagnostics_label = match (self.summary.error_count, self.summary.warning_count) {
            (0, 0) => "Project diagnostics: no problems".to_string(),
            (errors, warnings) => {
                let mut parts = Vec::new();
                if errors > 0 {
                    parts.push(format!(
                        "{errors} error{}",
                        if errors == 1 { "" } else { "s" }
                    ));
                }
                if warnings > 0 {
                    parts.push(format!(
                        "{warnings} warning{}",
                        if warnings == 1 { "" } else { "s" }
                    ));
                }
                format!("Project diagnostics: {}", parts.join(", "))
            }
        };

        indicator
            .child(
                ButtonLike::new("diagnostic-indicator")
                    .child(diagnostic_indicator)
                    .tab_index(0isize)
                    .aria_label(diagnostics_label)
                    .tooltip(move |_window, cx| {
                        Tooltip::for_action("Project Diagnostics", &Deploy, cx)
                    })
                    .on_click(cx.listener(|this, _, window, cx| {
                        if let Some(workspace) = this.workspace.upgrade() {
                            if this.summary.error_count == 0 && this.summary.warning_count > 0 {
                                cx.update_default_global(
                                    |show_warnings: &mut IncludeWarnings, _| show_warnings.0 = true,
                                );
                            }
                            // The dock panel, not a centre-pane tab: a problems
                            // list belongs beside the terminal, and opening it
                            // as an editor item costs you the file you were
                            // reading in order to look at what is wrong with it.
                            workspace.update(cx, |workspace, cx| {
                                workspace.toggle_panel_focus::<ProblemsPanel>(window, cx);
                            })
                        }
                    })),
            )
            .children(status)
    }
}

impl DiagnosticIndicator {
    pub fn new(workspace: &Workspace, cx: &mut Context<Self>) -> Self {
        let project = workspace.project();
        cx.subscribe(project, |this, project, event, cx| match event {
            project::Event::DiskBasedDiagnosticsStarted { .. } => {
                cx.notify();
            }

            project::Event::DiskBasedDiagnosticsFinished { .. }
            | project::Event::LanguageServerRemoved(_) => {
                this.summary = project.read(cx).diagnostic_summary(false, cx);
                cx.notify();
            }

            project::Event::DiagnosticsUpdated { .. } => {
                this.diagnostic_summary_update = cx.spawn(async move |this, cx| {
                    cx.background_executor()
                        .timer(Duration::from_millis(30))
                        .await;
                    this.update(cx, |this, cx| {
                        this.summary = project.read(cx).diagnostic_summary(false, cx);
                        cx.notify();
                    })
                    .log_err();
                });
            }

            _ => {}
        })
        .detach();

        Self {
            summary: project.read(cx).diagnostic_summary(false, cx),
            active_editor: None,
            workspace: workspace.weak_handle(),
            current_diagnostic: None,
            _observe_active_editor: None,
            diagnostics_update: Task::ready(()),
            diagnostic_summary_update: Task::ready(()),
        }
    }

    fn any_active_diagnostics(&self, cx: &mut Context<Self>) -> bool {
        if let Some(editor) = self.active_editor.as_ref().and_then(|e| e.upgrade()) {
            editor.read(cx).any_active_diagnostics()
        } else {
            false
        }
    }

    fn go_to_next_diagnostic(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(editor) = self.active_editor.as_ref().and_then(|e| e.upgrade()) {
            editor.update(cx, |editor, cx| {
                editor.go_to_diagnostic_at_cursor(
                    editor::Direction::Next,
                    GoToDiagnosticSeverityFilter::default(),
                    window,
                    cx,
                );
            })
        }
    }

    fn update(&mut self, editor: Entity<Editor>, window: &mut Window, cx: &mut Context<Self>) {
        let (buffer, cursor_position) = editor.update(cx, |editor, cx| {
            let buffer = editor.buffer().read(cx).snapshot(cx);
            let cursor_position = editor
                .selections
                .newest::<MultiBufferOffset>(&editor.display_snapshot(cx))
                .head();
            (buffer, cursor_position)
        });
        let new_diagnostic = buffer
            .diagnostics_in_range::<MultiBufferOffset>(cursor_position..cursor_position)
            .filter(|entry| !entry.range.is_empty())
            .min_by_key(|entry| {
                (
                    entry.diagnostic.severity,
                    entry.range.end - entry.range.start,
                )
            })
            .map(|entry| entry.diagnostic);
        if new_diagnostic != self.current_diagnostic.as_ref() {
            let new_diagnostic = new_diagnostic.cloned();
            self.diagnostics_update =
                cx.spawn_in(window, async move |diagnostics_indicator, cx| {
                    cx.background_executor()
                        .timer(Duration::from_millis(50))
                        .await;
                    diagnostics_indicator
                        .update(cx, |diagnostics_indicator, cx| {
                            diagnostics_indicator.current_diagnostic = new_diagnostic;
                            cx.notify();
                        })
                        .ok();
                });
        }
    }
}

impl EventEmitter<ToolbarItemEvent> for DiagnosticIndicator {}

impl StatusItemView for DiagnosticIndicator {
    fn set_active_pane_item(
        &mut self,
        active_pane_item: Option<&dyn ItemHandle>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(editor) = active_pane_item.and_then(|item| item.downcast::<Editor>()) {
            self.active_editor = Some(editor.downgrade());
            self._observe_active_editor = Some(cx.observe_in(&editor, window, Self::update));
            self.update(editor, window, cx);
        } else {
            self.active_editor = None;
            self.current_diagnostic = None;
            self._observe_active_editor = None;
        }
        cx.notify();
    }

    fn hide_setting(&self, _: &App) -> Option<HideStatusItem> {
        Some(HideStatusItem::new(|settings| {
            settings.diagnostics.get_or_insert_default().button = Some(false);
        }))
    }
}
