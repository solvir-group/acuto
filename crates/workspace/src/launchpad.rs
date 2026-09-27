// ui::prelude re-exports gpui's prelude plus the builder traits these buttons
// need - `full_width` lives on FixedWidth, which must be in scope for the method
// to resolve at all - and Button, Label, LabelSize, Color along with them.
// Only the items it does not cover are imported explicitly.
use gpui::{EventEmitter, FocusHandle, Focusable};
use ui::{Icon, IconName, IconSize, prelude::*};
use util::ResultExt as _;

use crate::item::Item;

/// A tab offering the things you might want to start, rather than an empty
/// buffer.
///
/// Upstream's `+` creates an untitled file, which assumes the next thing you
/// want is always to type code. This gives the same click a short list of
/// entry points instead.
/// The registry id Claude Code is installed under.
///
/// Duplicated from `agent_servers` rather than imported: `workspace` does not
/// depend on that crate, and taking the dependency for one string would pull a
/// large subtree into this crate's rebuild graph. A wrong id degrades to a
/// logged warning from `build_action`, not a panic.
pub(crate) const CLAUDE_AGENT_ID: &str = "claude-acp";
pub(crate) const CODEX_AGENT_ID: &str = "codex-acp";
pub(crate) const COPILOT_AGENT_ID: &str = "github-copilot-cli";

pub struct Launchpad {
    focus_handle: FocusHandle,
}

impl Launchpad {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
        }
    }
}

impl Launchpad {
    /// Actions are dispatched by name through the global registry: `workspace`
    /// does not depend on the crates that own them, and taking those
    /// dependencies for a handful of buttons would pull large subtrees into its
    /// rebuild graph. `build_action` returns a `Result`, so an action renamed
    /// upstream degrades to a logged warning and an inert button, not a panic.
    fn entry(
        id: &'static str,
        label: &'static str,
        icon: IconName,
        action_name: &'static str,
    ) -> Button {
        Self::entry_with(id, label, icon, action_name, None)
    }

    /// An entry whose action carries data.
    ///
    /// Actions built by name take their payload as JSON because the registry
    /// deserializes them the same way a keymap entry would, so an agent id
    /// arrives here as a string rather than as the typed `AgentId` this crate
    /// cannot name.
    fn entry_with(
        id: &'static str,
        label: &'static str,
        icon: IconName,
        action_name: &'static str,
        payload: Option<serde_json::Value>,
    ) -> Button {
        Button::new(id, label)
            .full_width()
            .start_icon(Icon::new(icon).size(IconSize::Small).color(Color::Muted))
            .on_click(move |_, window, cx: &mut App| {
                if let Some(action) = cx.build_action(action_name, payload.clone()).log_err() {
                    window.dispatch_action(action, cx);
                }
            })
    }
}

impl Render for Launchpad {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            // Without track_focus the handle returned by Focusable is not part of
            // the element tree, so actions dispatched from these buttons have no
            // node to bubble from and silently reach no handler.
            .track_focus(&self.focus_handle)
            .size_full()
            .items_center()
            .justify_center()
            .child(
                v_flex()
                    .w_64()
                    .gap_1()
                    .child(Self::entry(
                        "launchpad-file",
                        "Open a File",
                        IconName::File,
                        "file_finder::Toggle",
                    ))
                    .child(Self::entry(
                        "launchpad-terminal",
                        "New Terminal",
                        IconName::Terminal,
                        "terminal_panel::Toggle",
                    ))
                    .child(Self::entry(
                        "launchpad-agent",
                        "New Agent Thread",
                        IconName::Sparkle,
                        "agent::NewThreadInPane",
                    ))
                    // Opens the panel straight onto Claude rather than only
                    // selecting it: `agent::SelectAgent` takes effect the next
                    // time the panel opens, which from a launchpad button reads
                    // as the click having done nothing.
                    .child(Self::entry_with(
                        "launchpad-claude-code",
                        "Claude Code",
                        IconName::AiClaude,
                        "agent::NewExternalAgentThreadInPane",
                        Some(serde_json::json!({ "agent": CLAUDE_AGENT_ID })),
                    ))
                    .child(Self::entry_with(
                        "launchpad-codex",
                        "Codex",
                        IconName::AiOpenAi,
                        "agent::NewExternalAgentThreadInPane",
                        Some(serde_json::json!({ "agent": CODEX_AGENT_ID })),
                    ))
                    .child(Self::entry_with(
                        "launchpad-copilot",
                        "GitHub Copilot",
                        IconName::Copilot,
                        "agent::NewExternalAgentThreadInPane",
                        Some(serde_json::json!({ "agent": COPILOT_AGENT_ID })),
                    ))
                    .child(Self::entry(
                        "launchpad-git",
                        "Git Status",
                        IconName::GitBranch,
                        "git_panel::ToggleFocus",
                    )),
            )
    }
}

impl EventEmitter<()> for Launchpad {}

impl Focusable for Launchpad {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Item for Launchpad {
    type Event = ();

    /// Nothing here is a location, so it should not appear in back/forward.
    fn include_in_nav_history() -> bool {
        false
    }

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        "New Tab".into()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<ui::Icon> {
        Some(ui::Icon::new(IconName::Plus))
    }
}
