//! Tools that let the agent drive the editor itself.
//!
//! Everything the editor can do is already an action in a global registry, and
//! everything it can be configured to do is a key in one JSON file. Exposing
//! both means the agent can split a pane, change the theme, toggle a panel or
//! turn on a setting, instead of explaining to you which menu to find it in.
//!
//! Both tools ask for confirmation before acting. The action registry contains
//! destructive entries — closing windows, deleting paths, quitting — and a model
//! choosing among a thousand names by string match will sometimes choose the
//! wrong one. The prompt is what makes "call anything" safe to offer.

use crate::{AgentTool, ToolCallEventStream, ToolInput, ToolPermissionContext};
use agent_client_protocol::schema::v1 as acp;
use anyhow::Result;
use gpui::{App, SharedString, Task};
use language_model::LanguageModelToolResultContent;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt::Write as _;
use std::sync::Arc;

/// Cap on how many action names are listed at once, so a discovery call cannot
/// flood the context with the entire registry.
const MAX_LISTED_ACTIONS: usize = 60;

/// Run any editor command, or list the commands available.
///
/// Every menu item, keybinding and command-palette entry in the editor is an
/// action with a name like `workspace::ToggleLeftDock` or `editor::Format`. This
/// runs them.
///
/// - Call it with no `action` and an optional `filter` to discover what exists.
///   Filtering is a substring match on the name, so `filter: "dock"` finds the
///   dock actions.
/// - Call it with `action` to run one. Actions that take arguments accept them
///   as `input`.
/// - The user is asked to confirm before anything runs.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct RunIdeActionToolInput {
    /// The action to run, e.g. `workspace::ToggleBottomDock`. Leave this out to
    /// list actions instead of running one.
    #[serde(default)]
    pub action: Option<String>,
    /// Substring used to narrow the listing when `action` is not given.
    #[serde(default)]
    pub filter: Option<String>,
    /// Arguments for actions that take them, as JSON.
    ///
    /// <example>
    /// For `workspace::SendKeystrokes`, this is a string: "ctrl-shift-e"
    /// </example>
    #[serde(default)]
    pub input: Option<Value>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum IdeControlOutput {
    Success { message: String },
    Error { error: String },
}

impl From<IdeControlOutput> for LanguageModelToolResultContent {
    fn from(output: IdeControlOutput) -> Self {
        match output {
            IdeControlOutput::Success { message } => message.into(),
            IdeControlOutput::Error { error } => error.into(),
        }
    }
}

pub struct RunIdeActionTool;

impl AgentTool for RunIdeActionTool {
    type Input = RunIdeActionToolInput;
    type Output = IdeControlOutput;

    const NAME: &'static str = "run_ide_action";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Other
    }

    fn initial_title(
        &self,
        input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        match input {
            Ok(input) => match input.action {
                Some(action) => format!("Run `{action}`").into(),
                None => "List editor commands".into(),
            },
            Err(_) => "Run an editor command".into(),
        }
    }

    fn run(
        self: Arc<Self>,
        input: ToolInput<Self::Input>,
        event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<Self::Output, Self::Output>> {
        cx.spawn(async move |cx| {
            let input = input.recv().await.map_err(|error| IdeControlOutput::Error {
                error: error.to_string(),
            })?;

            let Some(action_name) = input.action.clone() else {
                let filter = input.filter.unwrap_or_default().to_lowercase();
                let listing = cx.update(|cx| {
                    let mut names: Vec<&str> = cx
                        .all_action_names()
                        .iter()
                        .copied()
                        .filter(|name| {
                            filter.is_empty() || name.to_lowercase().contains(&filter)
                        })
                        .collect();
                    names.sort_unstable();
                    let total = names.len();
                    names.truncate(MAX_LISTED_ACTIONS);
                    (names.join("\n"), total)
                });

                let (names, total) = listing;
                let mut message = if total == 0 {
                    "No actions matched.".to_string()
                } else {
                    format!("{total} action(s):\n{names}")
                };
                if total > MAX_LISTED_ACTIONS {
                    let _ = write!(
                        message,
                        "\n\n… {} more. Narrow with `filter`.",
                        total - MAX_LISTED_ACTIONS
                    );
                }
                return Ok(IdeControlOutput::Success { message });
            };

            // Confirmed before running rather than after: many actions are not
            // reversible, and the model is picking from a large registry by
            // name.
            let authorize = cx.update(|cx| {
                event_stream.authorize(
                    SharedString::new(format!("Run `{action_name}`")),
                    ToolPermissionContext::new(
                        RunIdeActionTool::NAME,
                        vec![action_name.clone()],
                    ),
                    cx,
                )
            });
            authorize.await.map_err(|error| IdeControlOutput::Error {
                error: error.to_string(),
            })?;

            let outcome = cx.update(|cx| {
                let action = cx
                    .build_action(&action_name, input.input.clone())
                    .map_err(|error| {
                        format!(
                            "`{action_name}` could not be built: {error}. \
                             Call this tool without `action` to list valid names."
                        )
                    })?;

                // Actions are dispatched into a window, so there has to be one.
                let window = cx
                    .active_window()
                    .ok_or_else(|| "No editor window is active.".to_string())?;

                window
                    .update(cx, |_, window, cx| {
                        window.dispatch_action(action, cx);
                    })
                    .map_err(|error| error.to_string())
            });

            match outcome {
                Ok(()) => Ok(IdeControlOutput::Success {
                    message: format!("Ran `{action_name}`."),
                }),
                Err(error) => Err(IdeControlOutput::Error { error }),
            }
        })
    }
}

/// Read or change any setting in the user's settings file.
///
/// Settings are addressed by their path through the JSON, so the theme is
/// `["theme"]` and the agent's dock side is `["agent", "dock"]`.
///
/// - Omit `value` to read the current setting.
/// - Provide `value` to change it. Comments and formatting in the file are
///   preserved; only the value being set is rewritten.
/// - The user is asked to confirm before anything is written.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct UpdateIdeSettingToolInput {
    /// Path through the settings JSON.
    ///
    /// <example>
    /// ["agent", "dock"] addresses the `dock` key inside the `agent` object.
    /// </example>
    pub key_path: Vec<String>,
    /// The new value as JSON. Omit to read instead of write.
    #[serde(default)]
    pub value: Option<Value>,
}

pub struct UpdateIdeSettingTool;

impl AgentTool for UpdateIdeSettingTool {
    type Input = UpdateIdeSettingToolInput;
    type Output = IdeControlOutput;

    const NAME: &'static str = "update_ide_setting";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Other
    }

    fn initial_title(
        &self,
        input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        match input {
            Ok(input) => {
                let path = input.key_path.join(".");
                if input.value.is_some() {
                    format!("Set `{path}`").into()
                } else {
                    format!("Read `{path}`").into()
                }
            }
            Err(_) => "Change a setting".into(),
        }
    }

    fn run(
        self: Arc<Self>,
        input: ToolInput<Self::Input>,
        event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<Self::Output, Self::Output>> {
        cx.spawn(async move |cx| {
            let input = input.recv().await.map_err(|error| IdeControlOutput::Error {
                error: error.to_string(),
            })?;

            if input.key_path.is_empty() {
                return Err(IdeControlOutput::Error {
                    error: "`key_path` cannot be empty.".into(),
                });
            }

            let settings_path = paths::settings_file().clone();
            let text = std::fs::read_to_string(&settings_path).map_err(|error| {
                IdeControlOutput::Error {
                    error: format!("Could not read {}: {error}", settings_path.display()),
                }
            })?;

            let current: Value = settings_json::parse_json_with_comments(&text).map_err(|error| {
                IdeControlOutput::Error {
                    error: format!("Settings file is not valid JSON: {error}"),
                }
            })?;

            let existing = input
                .key_path
                .iter()
                .try_fold(&current, |value, key| value.get(key.as_str()))
                .cloned();

            let Some(new_value) = input.value.clone() else {
                return Ok(IdeControlOutput::Success {
                    message: match existing {
                        Some(value) => format!(
                            "`{}` is currently {value}",
                            input.key_path.join(".")
                        ),
                        None => format!(
                            "`{}` is not set; the built-in default applies.",
                            input.key_path.join(".")
                        ),
                    },
                });
            };

            let path_label = input.key_path.join(".");
            let authorize = cx.update(|cx| {
                event_stream.authorize(
                    SharedString::new(format!("Set `{path_label}` to {new_value}")),
                    ToolPermissionContext::new(
                        UpdateIdeSettingTool::NAME,
                        vec![path_label.clone()],
                    ),
                    cx,
                )
            });
            authorize.await.map_err(|error| IdeControlOutput::Error {
                error: error.to_string(),
            })?;

            // Edited as text rather than reserialized, so the comments and
            // hand-formatting in the settings file survive the write.
            //
            // `update_value_in_json_text` applies each edit to `updated` as it
            // computes it, and *also* records it in `edits`. This used to apply
            // `edits` a second time afterwards, at offsets measured against the
            // original text -- which does not fail, it silently writes garbage
            // into the middle of an unrelated line. The recorded edits are only
            // useful to a caller that kept the string unmodified.
            let mut updated = text.clone();
            let mut edits = Vec::new();
            let mut key_path: Vec<&str> =
                input.key_path.iter().map(String::as_str).collect();
            settings_json::update_value_in_json_text(
                &mut updated,
                &mut key_path,
                settings_json::infer_json_indent_size(&text),
                &current,
                &build_nested(&input.key_path, new_value.clone(), &current),
                &mut edits,
            );

            // Every setting the editor reads comes through this one file, so a
            // bad write does not break one setting -- it drops the whole app
            // back to defaults, and the user has to find and repair the file by
            // hand. Cheaper to refuse the edit.
            if let Err(error) = settings_json::parse_json_with_comments::<Value>(&updated) {
                return Err(IdeControlOutput::Error {
                    error: format!(
                        "Refused to write: the result would not parse ({error}). \
                         The settings file is unchanged."
                    ),
                });
            }

            std::fs::write(&settings_path, &updated).map_err(|error| {
                IdeControlOutput::Error {
                    error: format!("Could not write {}: {error}", settings_path.display()),
                }
            })?;

            Ok(IdeControlOutput::Success {
                message: format!(
                    "Set `{path_label}` to {new_value}. The editor reloads settings on save."
                ),
            })
        })
    }
}

/// Produces a copy of `current` with `value` placed at `key_path`.
///
/// `update_value_in_json_text` diffs two whole documents, so the new value has
/// to be presented in the shape of the document rather than on its own.
fn build_nested(key_path: &[String], value: Value, current: &Value) -> Value {
    let mut root = current.clone();
    if !root.is_object() {
        root = Value::Object(serde_json::Map::new());
    }

    let mut cursor = &mut root;
    for (depth, key) in key_path.iter().enumerate() {
        let is_last = depth + 1 == key_path.len();
        let Some(object) = cursor.as_object_mut() else {
            break;
        };
        if is_last {
            object.insert(key.clone(), value.clone());
            break;
        }
        cursor = object
            .entry(key.clone())
            .or_insert_with(|| Value::Object(serde_json::Map::new()));
        if !cursor.is_object() {
            *cursor = Value::Object(serde_json::Map::new());
        }
    }

    root
}
