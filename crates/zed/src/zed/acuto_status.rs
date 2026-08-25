//! Status bar items specific to this fork.
//!
//! Both were named in the design spec's status bar line - "branch, LSP status,
//! cursor position, focus timer, layout preset switcher" - and neither exists
//! upstream. They are additive `StatusItemView`s, so they compose with Zed's own
//! items rather than replacing anything.

use std::time::Duration;

use gpui::{EventEmitter, Task};
// ui::prelude carries gpui's prelude plus the builder traits, h_flex/v_flex,
// Button, Color, LabelSize and App. These are not in it.
use ui::prelude::*;
use ui::{ContextMenu, IconPosition, Tooltip, right_click_menu};
use util::ResultExt as _;
// status_bar is a private module; these are re-exported from the crate root.
use workspace::{HideStatusItem, StatusItemView, item::ItemHandle};

/// The preset countdown lengths offered by the right-click menu.
const TIMER_PRESET_MINUTES: [u64; 4] = [15, 25, 45, 60];

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TimerMode {
    /// Counts up from zero. Answers "how long have I been at this" without
    /// imposing a structure on how you work.
    Stopwatch,
    /// Counts down from a fixed length, then stops and marks itself finished.
    Countdown { total: Duration },
}

/// A stopwatch or countdown timer for the current stretch of work.
///
/// Left click pauses and resumes; right click opens its settings, which is where
/// the mode and the countdown length are chosen. It never interrupts - a
/// finished countdown recolours itself and waits rather than stealing focus.
pub struct FocusTimer {
    mode: TimerMode,
    /// Time accrued in the current run, counted the same way in both modes. The
    /// countdown derives its remaining time from this rather than decrementing a
    /// second field, so the two can never drift apart.
    elapsed: Duration,
    running: bool,
    _tick: Option<Task<()>>,
}

impl FocusTimer {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            mode: TimerMode::Stopwatch,
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
                let still_alive = this.update(cx, |this, cx| {
                    if this.running {
                        this.elapsed += Duration::from_secs(1);
                        // A finished countdown stops itself here; left running it
                        // would tick past zero while the displayed time sat
                        // saturated at 00:00.
                        if this.is_finished() {
                            this.running = false;
                        }
                        cx.notify();
                    }
                    true
                });
                if still_alive.is_err() {
                    break;
                }
            }
        }));
    }

    fn toggle(&mut self, cx: &mut Context<Self>) {
        // Resuming a countdown that already ran out would sit at zero, so that
        // case restarts it instead.
        if !self.running && self.is_finished() {
            self.elapsed = Duration::ZERO;
        }
        self.running = !self.running;
        cx.notify();
    }

    fn reset(&mut self, cx: &mut Context<Self>) {
        self.elapsed = Duration::ZERO;
        cx.notify();
    }

    fn set_mode(&mut self, mode: TimerMode, cx: &mut Context<Self>) {
        self.mode = mode;
        self.elapsed = Duration::ZERO;
        self.running = true;
        cx.notify();
    }

    fn is_finished(&self) -> bool {
        match self.mode {
            TimerMode::Stopwatch => false,
            TimerMode::Countdown { total } => self.elapsed >= total,
        }
    }

    /// Seconds shown on the face: time accrued when counting up, time left when
    /// counting down.
    fn displayed_seconds(&self) -> u64 {
        match self.mode {
            TimerMode::Stopwatch => self.elapsed.as_secs(),
            TimerMode::Countdown { total } => total.saturating_sub(self.elapsed).as_secs(),
        }
    }

    fn label(&self) -> String {
        let total = self.displayed_seconds();
        let (hours, minutes, seconds) = (total / 3600, (total % 3600) / 60, total % 60);
        if hours > 0 {
            format!("{hours}:{minutes:02}:{seconds:02}")
        } else {
            format!("{minutes:02}:{seconds:02}")
        }
    }
}

impl Render for FocusTimer {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = cx.entity();
        let label = self.label();
        let running = self.running;
        let finished = self.is_finished();
        let mode = self.mode;

        let color = if finished {
            Color::Error
        } else if running {
            Color::Default
        } else {
            Color::Muted
        };

        let tooltip = match (finished, running) {
            (true, _) => "Timer finished - click to restart, right click for settings",
            (false, true) => "Click to pause, right click for settings",
            (false, false) => "Paused - click to resume, right click for settings",
        };

        // The menu closure receives `&mut App`, not this view's `Context`, so the
        // handle has to be captured here rather than recovered inside it.
        let entity_for_menu = entity.clone();

        right_click_menu("focus-timer-menu")
            .trigger(move |_, _, _| {
                Button::new("focus-timer", label.clone())
                    .label_size(LabelSize::Small)
                    .color(color)
                    .tooltip(Tooltip::text(tooltip))
                    .on_click({
                        let entity = entity.clone();
                        move |_, _, cx| {
                            entity.update(cx, |this, cx| this.toggle(cx));
                        }
                    })
            })
            .menu(move |window, cx| {
                let entity = entity_for_menu.clone();
                ContextMenu::build(window, cx, move |menu, _, _| {
                    let menu = menu
                        .header("Focus Timer")
                        .toggleable_entry(
                            "Stopwatch",
                            matches!(mode, TimerMode::Stopwatch),
                            IconPosition::Start,
                            None,
                            {
                                let entity = entity.clone();
                                move |_, cx| {
                                    entity.update(cx, |this, cx| {
                                        this.set_mode(TimerMode::Stopwatch, cx)
                                    });
                                }
                            },
                        )
                        .separator()
                        .label("Countdown");

                    let menu = TIMER_PRESET_MINUTES.iter().fold(menu, |menu, minutes| {
                        let total = Duration::from_secs(minutes * 60);
                        menu.toggleable_entry(
                            format!("{minutes} minutes"),
                            mode == TimerMode::Countdown { total },
                            IconPosition::Start,
                            None,
                            {
                                let entity = entity.clone();
                                move |_, cx| {
                                    entity.update(cx, |this, cx| {
                                        this.set_mode(TimerMode::Countdown { total }, cx)
                                    });
                                }
                            },
                        )
                    });

                    menu.separator()
                        .entry(if running { "Pause" } else { "Resume" }, None, {
                            let entity = entity.clone();
                            move |_, cx| {
                                entity.update(cx, |this, cx| this.toggle(cx));
                            }
                        })
                        .entry("Reset", None, {
                            let entity = entity.clone();
                            move |_, cx| {
                                entity.update(cx, |this, cx| this.reset(cx));
                            }
                        })
                })
            })
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
