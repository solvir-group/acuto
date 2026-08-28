//! Named window layouts, and the status bar control that switches between them.
//!
//! A layout here is which panels are open on which edge, plus -- for the grid
//! presets -- how the centre is split. It is deliberately not the whole
//! workspace: restoring which files were open is what session restore is for,
//! and a preset that closed your editors would be a trap rather than a
//! convenience.
//!
//! Presets you save yourself capture the docks only. The centre is left exactly
//! as it is, because the panes you have open are your work and a named layout
//! has no business discarding it.

use agent_ui::AgentPanel;
use db::kvp::GlobalKeyValueStore;
use diagnostics::problems_panel::ProblemsPanel;
use editor::Editor;
use git_ui::git_panel::GitPanel;
use gpui::{DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, WeakEntity};
use outline_panel::OutlinePanel;
use project_panel::ProjectPanel;
use serde::{Deserialize, Serialize};
use terminal_view::terminal_panel::TerminalPanel;
use ui::prelude::*;
use ui::{ContextMenu, PopoverMenu, Tooltip};
use util::ResultExt as _;
use workspace::{
    HideStatusItem, ModalView, SplitDirection, StatusItemView, Workspace, item::ItemHandle,
};

/// Where saved presets live.
///
/// Global rather than per-workspace: a layout is a way of working, not a
/// property of one folder, and having to re-save "Agents" in every project
/// would make the feature not worth using.
const SAVED_LAYOUTS_KEY: &str = "acuto_saved_layouts";

/// The panels a preset can name.
///
/// A closed enum rather than the panel's `persistent_name` string, so a preset
/// saved against a panel that later disappears fails to deserialise loudly
/// instead of silently doing nothing.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
enum PanelKind {
    ProjectTree,
    Outline,
    Git,
    Agent,
    Terminal,
    Problems,
}

impl PanelKind {
    /// Matches on the panel's persistent name, which is what a dock reports for
    /// whatever it is currently showing.
    fn from_persistent_name(name: &str) -> Option<Self> {
        match name {
            "Project Panel" => Some(Self::ProjectTree),
            "Outline Panel" => Some(Self::Outline),
            "GitPanel" => Some(Self::Git),
            "AgentPanel" => Some(Self::Agent),
            "TerminalPanel" => Some(Self::Terminal),
            "ProblemsPanel" => Some(Self::Problems),
            _ => None,
        }
    }

    fn open(self, workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
        match self {
            Self::ProjectTree => workspace.open_panel::<ProjectPanel>(window, cx),
            Self::Outline => workspace.open_panel::<OutlinePanel>(window, cx),
            Self::Git => workspace.open_panel::<GitPanel>(window, cx),
            Self::Agent => workspace.open_panel::<AgentPanel>(window, cx),
            Self::Terminal => workspace.open_panel::<TerminalPanel>(window, cx),
            Self::Problems => workspace.open_panel::<ProblemsPanel>(window, cx),
        }
    }
}

/// Which panel, if any, each edge shows.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug, Serialize, Deserialize)]
struct DockLayout {
    left: Option<PanelKind>,
    right: Option<PanelKind>,
    bottom: Option<PanelKind>,
}

impl DockLayout {
    fn capture(workspace: &Workspace, cx: &App) -> Self {
        let read = |dock: &Entity<workspace::dock::Dock>| {
            let dock = dock.read(cx);
            if !dock.is_open() {
                return None;
            }
            PanelKind::from_persistent_name(dock.active_panel()?.persistent_name())
        };

        Self {
            left: read(workspace.left_dock()),
            right: read(workspace.right_dock()),
            bottom: read(workspace.bottom_dock()),
        }
    }

    fn apply(self, workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
        // Every preset normalises to "everything closed" first. Opening a panel
        // is idempotent but closing is not implied by it, so without this a
        // preset's result would depend on what happened to be open when it ran.
        workspace.close_all_docks(window, cx);

        for panel in [self.left, self.right, self.bottom].into_iter().flatten() {
            panel.open(workspace, window, cx);
        }
    }
}

/// How the centre is arranged, for presets that arrange it.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
enum CenterLayout {
    /// Left alone. What every saved preset uses.
    Untouched,
    /// A grid of agent threads, `rows` by `columns`, split from the active pane.
    AgentGrid { rows: usize, columns: usize },
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
struct Layout {
    name: String,
    docks: DockLayout,
    center: CenterLayout,
}

impl Layout {
    fn apply(&self, workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
        self.docks.apply(workspace, window, cx);
        if let CenterLayout::AgentGrid { rows, columns } = self.center {
            build_agent_grid(rows, columns, workspace, window, cx);
        }
    }
}

/// The layouts that ship with the editor.
///
/// Built rather than stored so that a change here reaches an existing install
/// without a migration, and so a user cannot end up with a broken copy of one.
fn builtin_layouts() -> Vec<Layout> {
    vec![
        Layout {
            name: "Default".into(),
            docks: DockLayout {
                left: Some(PanelKind::ProjectTree),
                right: Some(PanelKind::Agent),
                bottom: Some(PanelKind::Terminal),
            },
            center: CenterLayout::Untouched,
        },
        Layout {
            name: "Agents".into(),
            docks: DockLayout::default(),
            center: CenterLayout::AgentGrid {
                rows: 2,
                columns: 4,
            },
        },
        Layout {
            name: "Review".into(),
            docks: DockLayout {
                left: Some(PanelKind::ProjectTree),
                right: Some(PanelKind::Git),
                bottom: None,
            },
            center: CenterLayout::Untouched,
        },
        Layout {
            name: "Debug".into(),
            docks: DockLayout {
                left: Some(PanelKind::Outline),
                right: None,
                bottom: Some(PanelKind::Problems),
            },
            center: CenterLayout::Untouched,
        },
    ]
}

/// Splits the active pane into a grid and puts a fresh agent thread in each cell.
///
/// Splits outward from the active pane rather than clearing the centre first:
/// the panes already open are the user's work, and a layout switch is not a
/// reason to close them. The first cell is the pane that was already active, so
/// whatever was open there gains a thread beside it rather than losing it.
fn build_agent_grid(
    rows: usize,
    columns: usize,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(panel) = workspace.panel::<AgentPanel>(cx) else {
        return;
    };

    let first = workspace.active_pane().clone();

    // Rows first: splitting down from the top-left gives one pane per row,
    // and splitting each of those to the right fills that row. Doing it the
    // other way round would nest the rows inside the first column.
    let mut row_panes = vec![first.clone()];
    for _ in 1..rows {
        let previous = row_panes
            .last()
            .cloned()
            .unwrap_or_else(|| first.clone());
        row_panes.push(workspace.split_pane(previous, SplitDirection::Down, window, cx));
    }

    let mut cells = Vec::with_capacity(rows * columns);
    for row_pane in row_panes {
        let mut current = row_pane.clone();
        cells.push(row_pane);
        for _ in 1..columns {
            current = workspace.split_pane(current, SplitDirection::Right, window, cx);
            cells.push(current.clone());
        }
    }

    for cell in cells {
        panel.update(cx, |panel, cx| {
            panel.open_new_thread_in_pane(cell, window, cx);
        });
    }
}

fn read_saved_layouts() -> Vec<Layout> {
    GlobalKeyValueStore::global()
        .read_kvp(SAVED_LAYOUTS_KEY)
        .log_err()
        .flatten()
        .and_then(|json| serde_json::from_str::<Vec<Layout>>(&json).log_err())
        .unwrap_or_default()
}

fn write_saved_layouts(layouts: Vec<Layout>, cx: &mut App) {
    let Some(json) = serde_json::to_string(&layouts).log_err() else {
        return;
    };
    cx.background_spawn(async move {
        GlobalKeyValueStore::global()
            .write_kvp(SAVED_LAYOUTS_KEY.to_string(), json)
            .await
            .log_err();
    })
    .detach();
}

/// Asks for a name for the layout being saved.
///
/// A modal rather than a prompt because GPUI's prompts are alerts: they take a
/// choice between buttons, not a line of text.
pub struct SaveLayoutModal {
    name_editor: Entity<Editor>,
    workspace: WeakEntity<Workspace>,
    switcher: WeakEntity<LayoutPresetSwitcher>,
}

impl SaveLayoutModal {
    fn new(
        workspace: WeakEntity<Workspace>,
        switcher: WeakEntity<LayoutPresetSwitcher>,
        suggested_name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let name_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_text(suggested_name, window, cx);
            editor.select_all(&editor::actions::SelectAll, window, cx);
            editor
        });
        name_editor.focus_handle(cx).focus(window, cx);

        Self {
            name_editor,
            workspace,
            switcher,
        }
    }

    fn confirm(&mut self, _: &menu::Confirm, _window: &mut Window, cx: &mut Context<Self>) {
        let name = self.name_editor.read(cx).text(cx).trim().to_string();
        if name.is_empty() {
            return;
        }

        let Some(workspace) = self.workspace.upgrade() else {
            cx.emit(DismissEvent);
            return;
        };

        let layout = Layout {
            name,
            docks: DockLayout::capture(workspace.read(cx), cx),
            center: CenterLayout::Untouched,
        };

        let mut layouts = read_saved_layouts();
        // Saving over an existing name replaces it rather than adding a second
        // entry the menu could not tell apart.
        match layouts.iter().position(|saved| saved.name == layout.name) {
            Some(index) => layouts[index] = layout,
            None => layouts.push(layout),
        }
        write_saved_layouts(layouts.clone(), cx);

        self.switcher
            .update(cx, |switcher, cx| {
                switcher.saved = layouts;
                cx.notify();
            })
            .log_err();

        cx.emit(DismissEvent);
    }

    fn cancel(&mut self, _: &menu::Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }
}

impl Render for SaveLayoutModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("SaveLayoutModal")
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::cancel))
            .w(rems(24.))
            .p_4()
            .gap_2()
            .elevation_3(cx)
            .child(Label::new("Save Layout"))
            .child(
                Label::new("Saves which panels are open on each edge.")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(
                div()
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .bg(cx.theme().colors().editor_background)
                    .border_1()
                    .border_color(cx.theme().colors().border)
                    .child(self.name_editor.clone()),
            )
    }
}

impl Focusable for SaveLayoutModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.name_editor.focus_handle(cx)
    }
}

impl EventEmitter<DismissEvent> for SaveLayoutModal {}
impl ModalView for SaveLayoutModal {}

/// Switches between named layouts, and toggles focus mode.
///
/// Focus is a toggle rather than a preset because it is the only one you leave:
/// every other layout is somewhere you go and stay, while focus mode is
/// something you enter for a stretch and then undo. Making it a preset would
/// mean picking a second preset to get back, which is not the same gesture.
pub struct LayoutPresetSwitcher {
    workspace: WeakEntity<Workspace>,
    saved: Vec<Layout>,
    /// What the docks looked like before focus mode, so leaving restores them.
    /// `None` means focus mode is off.
    before_focus: Option<DockLayout>,
}

impl LayoutPresetSwitcher {
    pub fn new(workspace: WeakEntity<Workspace>) -> Self {
        Self {
            workspace,
            saved: read_saved_layouts(),
            before_focus: None,
        }
    }

    fn toggle_focus_mode(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };

        match self.before_focus.take() {
            Some(previous) => {
                workspace.update(cx, |workspace, cx| {
                    previous.apply(workspace, window, cx);
                });
            }
            None => {
                let previous =
                    workspace.update(cx, |workspace, cx| {
                        let previous = DockLayout::capture(workspace, cx);
                        workspace.close_all_docks(window, cx);
                        previous
                    });
                self.before_focus = Some(previous);
            }
        }
        cx.notify();
    }

    fn apply(&mut self, layout: Layout, window: &mut Window, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        workspace.update(cx, |workspace, cx| {
            layout.apply(workspace, window, cx);
        });
        // Any explicit layout choice ends focus mode: the docks it opened are
        // the opposite of what focus mode is for, and keeping the flag set
        // would make the next focus click restore a layout you already left.
        self.before_focus = None;
        cx.notify();
    }

    fn prompt_to_save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let switcher = cx.weak_entity();
        let workspace_handle = workspace.downgrade();
        let suggested = format!("Layout {}", self.saved.len() + 1);

        workspace.update(cx, |workspace, cx| {
            workspace.toggle_modal(window, cx, |window, cx| {
                SaveLayoutModal::new(workspace_handle, switcher, suggested, window, cx)
            });
        });
    }

    fn delete_saved(&mut self, name: String, cx: &mut Context<Self>) {
        self.saved.retain(|layout| layout.name != name);
        write_saved_layouts(self.saved.clone(), cx);
        cx.notify();
    }
}

impl Render for LayoutPresetSwitcher {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = cx.entity();
        let in_focus_mode = self.before_focus.is_some();
        let saved = self.saved.clone();

        let focus_button = Button::new("preset-focus", "Focus")
            .label_size(LabelSize::Small)
            // The colour is the whole state indicator: there is one button, and
            // whether it is lit says whether you are in focus mode.
            .color(if in_focus_mode {
                Color::Accent
            } else {
                Color::Muted
            })
            .tooltip(Tooltip::text(if in_focus_mode {
                "Leave focus mode and restore the panels"
            } else {
                "Focus mode - hide every panel"
            }))
            .on_click({
                let entity = entity.clone();
                move |_, window, cx| {
                    entity.update(cx, |this, cx| this.toggle_focus_mode(window, cx));
                }
            });

        let menu_entity = entity.clone();
        let layout_menu = PopoverMenu::new("layout-preset-menu")
            .trigger(
                Button::new("preset-layouts", "Layout")
                    .label_size(LabelSize::Small)
                    .color(Color::Muted)
                    .tooltip(Tooltip::text("Switch or save a window layout")),
            )
            .menu(move |window, cx| {
                let entity = menu_entity.clone();
                let saved = saved.clone();
                Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                    menu = menu.header("Presets");
                    for layout in builtin_layouts() {
                        let entity = entity.clone();
                        menu = menu.entry(layout.name.clone(), None, move |window, cx| {
                            let layout = layout.clone();
                            entity.update(cx, |this, cx| this.apply(layout, window, cx));
                        });
                    }

                    if !saved.is_empty() {
                        menu = menu.separator().header("Saved");
                        for layout in saved.clone() {
                            let entity = entity.clone();
                            menu = menu.entry(layout.name.clone(), None, move |window, cx| {
                                let layout = layout.clone();
                                entity.update(cx, |this, cx| this.apply(layout, window, cx));
                            });
                        }

                        menu = menu.separator().header("Delete Saved");
                        for layout in saved.clone() {
                            let entity = entity.clone();
                            let name = layout.name.clone();
                            menu = menu.entry(layout.name.clone(), None, move |_, cx| {
                                let name = name.clone();
                                entity.update(cx, |this, cx| this.delete_saved(name, cx));
                            });
                        }
                    }

                    let entity = entity.clone();
                    menu.separator()
                        .entry("Save Current Layout\u{2026}", None, move |window, cx| {
                            entity.update(cx, |this, cx| this.prompt_to_save(window, cx));
                        })
                }))
            });

        h_flex().gap_0p5().child(focus_button).child(layout_menu)
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
