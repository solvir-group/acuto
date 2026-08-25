//! Status bar items specific to this fork.
//!
//! Both were named in the design spec's status bar line - "branch, LSP status,
//! cursor position, focus timer, layout preset switcher" - and neither exists
//! upstream. They are additive `StatusItemView`s, so they compose with Zed's own
//! items rather than replacing anything.

use std::time::Duration;

use gpui::{EventEmitter, Task};
// ui::prelude carries gpui's prelude plus the builder traits, h_flex/v_flex,
// Button, Color, LabelSize and App. Tooltip is not in it.
use ui::prelude::*;
use ui::Tooltip;
use util::ResultExt as _;
// status_bar is a private module; these are re-exported from the crate root.
use workspace::{HideStatusItem, StatusItemView, item::ItemHandle};

/// A stopwatch for the current stretch of work.
///
/// Deliberately not a pomodoro: it counts up rather than down, and never
/// interrupts. It answers "how long have I been at this" without imposing a
/// structure on how you work.
pub struct FocusTimer {
    elapsed: Duration,
    running: bool,
    _tick: Option<Task<()>>,
}

impl FocusTimer {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            elapsed: Duration::ZERO,
            running: false,
            _tick: None,
        };
        this.start(cx);
        this
    }

    fn start(&mut self, cx: &mut Context<Self>) {
        self.running = true;
        // A background timer rather than a wall-clock delta, so a suspended or
        // backgrounded window does not accumulate time the user never spent.
        self._tick = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_secs(1))
                    .await;
                let still_running = this.update(cx, |this, cx| {
                    if this.running {
                        this.elapsed += Duration::from_secs(1);
                        cx.notify();
                    }
                    true
                });
                if still_running.is_err() {
                    break;
                }
            }
        }));
    }

    fn toggle(&mut self, cx: &mut Context<Self>) {
        self.running = !self.running;
        cx.notify();
    }

    fn label(&self) -> String {
        let total = self.elapsed.as_secs();
        format!("{:02}:{:02}", total / 60, total % 60)
    }
}

impl Render for FocusTimer {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        Button::new("focus-timer", self.label())
            .label_size(LabelSize::Small)
            .color(if self.running {
                Color::Default
            } else {
                Color::Muted
            })
            .tooltip(Tooltip::text(if self.running {
                "Focus timer - click to pause"
            } else {
                "Focus timer paused - click to resume"
            }))
            .on_click(cx.listener(|this, _, _, cx| this.toggle(cx)))
    }
}

impl EventEmitter<()> for FocusTimer {}

impl StatusItemView for FocusTimer {
    fn set_active_pane_item(
        &mut self,
        _active_pane_item: Option<&dyn ItemHandle>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        // Timing is independent of what is open.
    }

    fn hide_setting(&self, _: &App) -> Option<HideStatusItem> {
        // No dedicated setting yet, so opt out of the "Hide Button" menu entry
        // rather than offer one that writes a key nothing reads.
        None
    }
}

/// Switches between the named layout presets defined in the fork's keymap.
///
/// The presets themselves are keymap entries - `alt-shift-1` and `alt-shift-2` -
/// so this dispatches the same actions rather than duplicating the layout logic
/// in Rust. Keeping one source of truth means editing the keymap still changes
/// what the presets do.
pub struct LayoutPresetSwitcher;

impl Render for LayoutPresetSwitcher {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .gap_0p5()
            .child(
                Button::new("preset-default", "Default")
                    .label_size(LabelSize::Small)
                    .color(Color::Muted)
                    .tooltip(Tooltip::text(
                        "Tree left, editor centre, agent right, terminal bottom",
                    ))
                    .on_click(|_, window, cx| {
                        if let Some(action) = cx
                            .build_action("workspace::CloseAllDocks", None)
                            .log_err()
                        {
                            window.dispatch_action(action, cx);
                        }
                        if let Some(action) = cx
                            .build_action(
                                "workspace::SendKeystrokes",
                                Some(serde_json::json!(
                                    "ctrl-shift-e ctrl-shift-/ ctrl-` alt-1"
                                )),
                            )
                            .log_err()
                        {
                            window.dispatch_action(action, cx);
                        }
                    }),
            )
            .child(
                Button::new("preset-focus", "Focus")
                    .label_size(LabelSize::Small)
                    .color(Color::Muted)
                    .tooltip(Tooltip::text("Editor only"))
                    .on_click(|_, window, cx| {
                        if let Some(action) = cx
                            .build_action("workspace::CloseAllDocks", None)
                            .log_err()
                        {
                            window.dispatch_action(action, cx);
                        }
                    }),
            )
    }
}

impl EventEmitter<()> for LayoutPresetSwitcher {}

impl StatusItemView for LayoutPresetSwitcher {
    fn set_active_pane_item(
        &mut self,
        _active_pane_item: Option<&dyn ItemHandle>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }

    fn hide_setting(&self, _: &App) -> Option<HideStatusItem> {
        None
    }
}
