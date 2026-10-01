//! An MCP server that hands the editor's controls to an external agent.
//!
//! The built-in agent can already run any editor action and change any setting,
//! because it runs inside the process and its tools are ordinary Rust. External
//! agents -- Claude Code, Codex, Copilot -- speak ACP over a pipe and can only
//! call tools that the client offers them as MCP servers, which until now meant
//! whatever MCP servers the *user* had configured. So the agents people
//! actually pay for were the ones that could not open a panel, switch a theme,
//! or turn on a setting they had just recommended.
//!
//! This closes that gap by serving the same two capabilities over MCP on
//! loopback, and advertising it to every external agent session.
//!
//! # Trust
//!
//! The socket binds to 127.0.0.1 on an ephemeral port and every request must
//! carry a bearer token generated at startup, so another user on the machine
//! cannot drive the editor by guessing a port. That is a guard against other
//! processes, not against the agent.
//!
//! Against the agent, the guard is what the tools will do at all. The server is
//! offered to agents running in modes that skip permission prompts, and an
//! agent can be steered by text it reads, so neither tool may reach anything
//! that runs a program or destroys work:
//!
//! - `update_ide_setting` reads and writes only the settings in
//!   [`WRITABLE_SETTINGS`] -- appearance and editing preferences. Anything that
//!   names a command, shell, server, URL, key or environment is refused, since
//!   writing one is code execution the next time it is read.
//! - `run_ide_action` refuses any action whose name matches
//!   [`BLOCKED_ACTION_WORDS`]: deleting, quitting, running tasks or terminal
//!   input, git history and remotes, installing, starting agents.

use std::io::Read as _;
use std::path::PathBuf;
use std::rc::Rc;

use anyhow::{Context as _, Result, anyhow};
use futures::channel::{mpsc, oneshot};
use futures::{SinkExt as _, StreamExt as _};
use gpui::{App, Global, Task};
use serde_json::{Value, json};

/// Cap on how many action names one listing returns.
///
/// The registry holds well over a thousand actions. Returning all of them would
/// spend more of the agent's context on this one call than the task it was
/// asked to do.
const MAX_LISTED_ACTIONS: usize = 60;

/// The MCP protocol version this server speaks.
const PROTOCOL_VERSION: &str = "2024-11-05";

/// Largest request body read. Tool calls are a few hundred bytes; this only
/// stops a runaway client from exhausting the editor's memory.
const MAX_REQUEST_BYTES: u64 = 1024 * 1024;

/// The settings an agent may read or change, as paths through the settings
/// JSON. A path matches when it starts with one of these, so `["tab_bar"]`
/// covers everything under it.
///
/// An allowlist rather than a denylist: new settings arrive all the time, and
/// the dangerous ones -- `context_servers`, `agent_servers`, `terminal.shell`,
/// `lsp`, `languages.*.formatter`, API URLs -- have to stay out by default.
const WRITABLE_SETTINGS: &[&[&str]] = &[
    &["theme"],
    &["icon_theme"],
    &["ui_font_size"],
    &["ui_font_family"],
    &["ui_font_weight"],
    &["ui_font_features"],
    &["buffer_font_size"],
    &["buffer_font_family"],
    &["buffer_font_weight"],
    &["buffer_font_features"],
    &["buffer_line_height"],
    &["agent_ui_font_size"],
    &["agent_buffer_font_size"],
    &["tab_size"],
    &["hard_tabs"],
    &["soft_wrap"],
    &["preferred_line_length"],
    &["show_wrap_guides"],
    &["wrap_guides"],
    &["show_whitespaces"],
    &["indent_guides"],
    &["minimap"],
    &["scrollbar"],
    &["gutter"],
    &["toolbar"],
    &["tab_bar"],
    &["tabs"],
    &["title_bar"],
    &["status_bar"],
    &["project_panel"],
    &["outline_panel"],
    &["git_panel"],
    &["centered_layout"],
    &["cursor_blink"],
    &["cursor_shape"],
    &["current_line_highlight"],
    &["selection_highlight"],
    &["relative_line_numbers"],
    &["inlay_hints"],
    &["hover_popover_enabled"],
    &["show_completions_on_input"],
    &["show_completion_documentation"],
    &["preview_tabs"],
    &["active_pane_modifiers"],
    &["bottom_dock_layout"],
    &["vim_mode"],
    &["helix_mode"],
    &["base_keymap"],
    &["autosave"],
    &["restore_on_startup"],
    &["search"],
    &["use_smartcase_search"],
    &["agent", "dock"],
    &["agent", "default_width"],
    &["agent", "default_height"],
    &["terminal", "font_size"],
    &["terminal", "font_family"],
    &["terminal", "line_height"],
    &["terminal", "blinking"],
    &["terminal", "cursor_shape"],
    &["terminal", "dock"],
];

/// Words that, anywhere in an action's name, keep it out of an agent's reach.
///
/// Matched against the lowercased name, so `git::Push`, `task::Spawn` and
/// `editor::DeleteLine` are all caught. Some harmless actions are caught too;
/// that is the right side to err on for a tool no one is asked to approve.
const BLOCKED_ACTION_WORDS: &[&str] = &[
    "quit",
    "close_window",
    "closewindow",
    "delete",
    "trash",
    "remove",
    "uninstall",
    "install",
    "reset",
    "discard",
    "revert",
    "restore",
    "push",
    "pull",
    "fetch",
    "force",
    "commit",
    "amend",
    "stash",
    "checkout",
    "branch",
    "sign_out",
    "signout",
    "log_out",
    "logout",
    "restart",
    "reload",
    "shutdown",
    "kill",
    "spawn",
    "rerun",
    "run",
    "send",
    "exec",
    "debug",
    "task",
    "terminal",
    "repl",
    "remote",
    "ssh",
    "container",
    "extension",
    "agent",
    "acp",
    "team_notes",
    "credential",
    "token",
    "key",
];

/// Whether an agent may read or write the setting at `key_path`.
fn setting_is_writable(key_path: &[String]) -> bool {
    WRITABLE_SETTINGS.iter().any(|allowed| {
        key_path.len() >= allowed.len()
            && allowed
                .iter()
                .zip(key_path)
                .all(|(allowed, key)| *allowed == key.as_str())
    })
}

/// Whether an agent may run the action called `name`.
fn action_is_allowed(name: &str) -> bool {
    let lowered = name.to_lowercase();
    !BLOCKED_ACTION_WORDS
        .iter()
        .any(|word| lowered.contains(word))
}

/// Where an external agent can reach the running editor.
#[derive(Clone, Debug)]
pub struct Endpoint {
    /// Full URL, including the path MCP requests are posted to.
    pub url: String,
    /// Value for the `Authorization` header, including the `Bearer ` prefix.
    pub authorization: String,
}

struct GlobalEndpoint(Option<Endpoint>);

impl Global for GlobalEndpoint {}

/// Where external agents should send MCP requests, if the server started.
///
/// `None` when the socket could not be bound. Failing to start is not fatal:
/// the editor works, the agents simply cannot drive it, which is the behaviour
/// that shipped before this existed.
pub fn endpoint(cx: &App) -> Option<Endpoint> {
    cx.try_global::<GlobalEndpoint>()
        .and_then(|global| global.0.clone())
}

/// Starts the loopback MCP server and registers its endpoint.
pub fn init(cx: &mut App) {
    match start(cx) {
        Ok(endpoint) => {
            log::info!("IDE control MCP server listening at {}", endpoint.url);
            cx.set_global(GlobalEndpoint(Some(endpoint)));
        }
        Err(error) => {
            log::warn!("could not start the IDE control MCP server: {error:#}");
            cx.set_global(GlobalEndpoint(None));
        }
    }
}

type DiagnosticsProvider = Rc<dyn Fn(Option<PathBuf>, &mut App) -> Task<Result<String, String>>>;

struct GlobalDiagnosticsProvider(DiagnosticsProvider);

impl Global for GlobalDiagnosticsProvider {}

/// Supplies the answer to `get_diagnostics`.
///
/// Diagnostics live in projects and workspaces, which this crate stays clear
/// of so that every agent crate can depend on it cheaply; whoever owns them
/// registers the provider.
pub fn set_diagnostics_provider(
    provider: impl Fn(Option<PathBuf>, &mut App) -> Task<Result<String, String>> + 'static,
    cx: &mut App,
) {
    cx.set_global(GlobalDiagnosticsProvider(Rc::new(provider)));
}

/// One tool call, and the channel its answer goes back on.
struct Call {
    name: String,
    arguments: Value,
    respond: oneshot::Sender<Result<String, String>>,
}

fn start(cx: &mut App) -> Result<Endpoint> {
    // Port 0: the OS picks a free one. A fixed port would collide with a second
    // window of the editor, and with whatever else happens to hold it.
    let server = tiny_http::Server::http("127.0.0.1:0")
        .map_err(|error| anyhow!("could not bind a loopback socket: {error}"))?;
    let port = server
        .server_addr()
        .to_ip()
        .context("loopback server did not report an IP address")?
        .port();
    let token = uuid::Uuid::new_v4().to_string();

    let (calls_tx, mut calls_rx) = mpsc::unbounded::<Call>();

    // The tool bodies touch the action registry and the settings file, both of
    // which belong to the main thread, so the socket thread never executes
    // anything itself -- it parses, forwards, and waits.
    cx.spawn(async move |cx| {
        while let Some(call) = calls_rx.next().await {
            let Call {
                name,
                arguments,
                respond,
            } = call;
            let result = execute(&name, arguments, cx).await;
            // A closed receiver means the HTTP request was abandoned, which is
            // ordinary: the agent gave up, or its process exited.
            let _ = respond.send(result);
        }
    })
    .detach();

    // A dedicated thread rather than the background executor: `recv` blocks
    // until a request arrives, and parking one of a small pool of executor
    // threads for the lifetime of the process would starve everything else that
    // wanted to run on it.
    let expected_authorization = format!("Bearer {token}");
    std::thread::Builder::new()
        .name("ide-control-mcp".into())
        .spawn(move || serve(server, expected_authorization, calls_tx))
        .context("could not spawn the MCP server thread")?;

    Ok(Endpoint {
        url: format!("http://127.0.0.1:{port}/mcp"),
        authorization: format!("Bearer {token}"),
    })
}

fn serve(
    server: tiny_http::Server,
    expected_authorization: String,
    calls_tx: mpsc::UnboundedSender<Call>,
) {
    for mut request in server.incoming_requests() {
        let authorized = request.headers().iter().any(|header| {
            header.field.equiv("Authorization") && header.value.as_str() == expected_authorization
        });

        if !authorized {
            respond_with(request, 401, json!({"error": "unauthorized"}));
            continue;
        }

        if request
            .body_length()
            .is_some_and(|length| length as u64 > MAX_REQUEST_BYTES)
        {
            respond_with(request, 413, json!({"error": "request too large"}));
            continue;
        }

        let mut body = String::new();
        // Bounded even without a Content-Length, which a chunked request omits.
        let read = request
            .as_reader()
            .take(MAX_REQUEST_BYTES + 1)
            .read_to_string(&mut body);
        if body.len() as u64 > MAX_REQUEST_BYTES {
            respond_with(request, 413, json!({"error": "request too large"}));
            continue;
        }
        if let Err(error) = read {
            respond_with(
                request,
                400,
                error_response(Value::Null, -32700, &format!("unreadable body: {error}")),
            );
            continue;
        }

        let message: Value = match serde_json::from_str(&body) {
            Ok(message) => message,
            Err(error) => {
                respond_with(
                    request,
                    400,
                    error_response(Value::Null, -32700, &format!("invalid JSON: {error}")),
                );
                continue;
            }
        };

        let id = message.get("id").cloned().unwrap_or(Value::Null);
        let method = message
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();

        // Notifications carry no id and expect no result, only an ack.
        if id.is_null() {
            respond_empty(request);
            continue;
        }

        let response = match method.as_str() {
            "initialize" => success(id, initialize_result()),
            "tools/list" => success(id, json!({ "tools": tool_definitions() })),
            "tools/call" => {
                let params = message.get("params").cloned().unwrap_or(Value::Null);
                let name = params
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let arguments = params
                    .get("arguments")
                    .cloned()
                    .unwrap_or_else(|| json!({}));

                match dispatch(&calls_tx, name, arguments) {
                    Ok(text) => success(id, tool_content(&text, false)),
                    // Reported as a successful call carrying an error result,
                    // not as a JSON-RPC error: a tool that refused is something
                    // the model should read and act on, whereas a protocol
                    // error is something it can only give up on.
                    Err(text) => success(id, tool_content(&text, true)),
                }
            }
            other => error_response(id, -32601, &format!("unknown method `{other}`")),
        };

        respond_with(request, 200, response);
    }
}

/// Sends one call to the main thread and blocks this thread for its answer.
fn dispatch(
    calls_tx: &mpsc::UnboundedSender<Call>,
    name: String,
    arguments: Value,
) -> Result<String, String> {
    let (respond, response) = oneshot::channel();
    let mut sender = calls_tx.clone();
    futures::executor::block_on(sender.send(Call {
        name,
        arguments,
        respond,
    }))
    .map_err(|_| "the editor is shutting down".to_string())?;

    futures::executor::block_on(response)
        .map_err(|_| "the editor dropped the request".to_string())?
}

fn initialize_result() -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": { "tools": {} },
        "serverInfo": { "name": "acuto-ide-control", "version": env!("CARGO_PKG_VERSION") },
    })
}

fn tool_definitions() -> Value {
    json!([
        {
            "name": "run_ide_action",
            "description": "Run a command in the editor, or list the commands available. \
                            Commands that delete work, quit, run programs or touch git \
                            history are not available. \
                            Every menu item, keybinding and command-palette entry is an action \
                            with a name like `workspace::ToggleBottomDock` or `editor::Format`. \
                            Call with no `action` and an optional `filter` to discover what \
                            exists; call with `action` to run one.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "description": "The action to run, e.g. `workspace::ToggleBottomDock`. \
                                        Omit to list actions instead."
                    },
                    "filter": {
                        "type": "string",
                        "description": "Substring used to narrow the listing when `action` is \
                                        not given."
                    },
                    "input": {
                        "description": "Arguments for actions that take them, as JSON."
                    }
                }
            }
        },
        {
            "name": "get_diagnostics",
            "description": "Get the editor's current language-server diagnostics (errors, \
                            warnings and info). Pass an absolute `path` for one file; omit it \
                            for every file the language servers have reported problems in. \
                            Faster than a full build for checking whether code compiles, \
                            and it reflects unsaved edits in the editor.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Absolute path of a file in the open project. \
                                        Omit for all files with diagnostics."
                    }
                }
            }
        },
        {
            "name": "update_ide_setting",
            "description": "Read or change an appearance or editing setting in the user's \
                            settings file (theme, fonts, panels, wrapping and the like). \
                            Settings that name commands, servers or URLs are not available. \
                            Settings \
                            are addressed by their path through the JSON, so the theme is \
                            [\"theme\"] and the agent's dock side is [\"agent\", \"dock\"]. \
                            Omit `value` to read; provide it to write. Comments and formatting \
                            in the file are preserved.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "key_path": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Path through the settings JSON."
                    },
                    "value": {
                        "description": "The new value as JSON. Omit to read instead of write."
                    }
                },
                "required": ["key_path"]
            }
        }
    ])
}

fn tool_content(text: &str, is_error: bool) -> Value {
    json!({
        "content": [{ "type": "text", "text": text }],
        "isError": is_error,
    })
}

fn success(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error_response(id: Value, code: i32, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message },
    })
}

fn respond_with(request: tiny_http::Request, status: u16, body: Value) {
    let body = body.to_string();
    let header = tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..])
        .expect("a literal header name and value are always valid");
    let response = tiny_http::Response::from_string(body)
        .with_status_code(status)
        .with_header(header);
    if let Err(error) = request.respond(response) {
        log::debug!("MCP client went away before the response was written: {error}");
    }
}

fn respond_empty(request: tiny_http::Request) {
    if let Err(error) = request.respond(tiny_http::Response::empty(202)) {
        log::debug!("MCP client went away before the ack was written: {error}");
    }
}

async fn execute(name: &str, arguments: Value, cx: &mut gpui::AsyncApp) -> Result<String, String> {
    match name {
        "run_ide_action" => run_ide_action(arguments, cx),
        "get_diagnostics" => get_diagnostics(arguments, cx).await,
        "update_ide_setting" => update_ide_setting(arguments),
        other => Err(format!("unknown tool `{other}`")),
    }
}

fn run_ide_action(arguments: Value, cx: &mut gpui::AsyncApp) -> Result<String, String> {
    let action_name = arguments.get("action").and_then(Value::as_str);
    let input = arguments.get("input").cloned();

    let Some(action_name) = action_name else {
        let filter = arguments
            .get("filter")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_lowercase();

        return Ok(cx.update(|cx| {
            let mut names: Vec<&str> = cx
                .all_action_names()
                .iter()
                .copied()
                .filter(|name| action_is_allowed(name))
                .filter(|name| filter.is_empty() || name.to_lowercase().contains(&filter))
                .collect();
            names.sort_unstable();
            let total = names.len();
            names.truncate(MAX_LISTED_ACTIONS);

            if total == 0 {
                "No actions matched.".to_string()
            } else if total > MAX_LISTED_ACTIONS {
                format!(
                    "{total} action(s):\n{}\n\n… {} more. Narrow with `filter`.",
                    names.join("\n"),
                    total - MAX_LISTED_ACTIONS
                )
            } else {
                format!("{total} action(s):\n{}", names.join("\n"))
            }
        }));
    };

    let action_name = action_name.to_string();
    if !action_is_allowed(&action_name) {
        return Err(format!(
            "`{action_name}` is not available to agents: it could delete work, run a \
             program, or change something outside the editor. Ask the user to run it."
        ));
    }
    cx.update(|cx| {
        let action = cx.build_action(&action_name, input).map_err(|error| {
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
            .update(cx, |_, window, cx| window.dispatch_action(action, cx))
            .map_err(|error| error.to_string())?;

        Ok(format!("Ran `{action_name}`."))
    })
}

async fn get_diagnostics(arguments: Value, cx: &mut gpui::AsyncApp) -> Result<String, String> {
    let path = match arguments.get("path") {
        None | Some(Value::Null) => None,
        Some(Value::String(path)) => {
            let path = PathBuf::from(path);
            if !path.is_absolute() {
                return Err(format!(
                    "`path` must be absolute; got `{}`.",
                    path.display()
                ));
            }
            Some(path)
        }
        Some(other) => return Err(format!("`path` must be a string; got {other}.")),
    };

    let task = cx.update(|cx| {
        let provider = cx
            .try_global::<GlobalDiagnosticsProvider>()
            .map(|provider| provider.0.clone())?;
        Some(provider(path, cx))
    });
    match task {
        Some(task) => task.await,
        None => Err("Diagnostics are not available in this editor build.".into()),
    }
}

fn update_ide_setting(arguments: Value) -> Result<String, String> {
    let key_path: Vec<String> = arguments
        .get("key_path")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();

    if key_path.is_empty() {
        return Err("`key_path` cannot be empty.".into());
    }
    if !setting_is_writable(&key_path) {
        return Err(format!(
            "`{}` is not available to agents. Only appearance and editing preferences \
             can be read or changed here; ask the user to change anything else.",
            key_path.join(".")
        ));
    }

    let settings_path = paths::settings_file().clone();
    let text = std::fs::read_to_string(&settings_path)
        .map_err(|error| format!("Could not read {}: {error}", settings_path.display()))?;
    let current: Value = settings_json::parse_json_with_comments(&text)
        .map_err(|error| format!("Settings file is not valid JSON: {error}"))?;

    let existing = key_path
        .iter()
        .try_fold(&current, |value, key| value.get(key.as_str()))
        .cloned();
    let path_label = key_path.join(".");

    let Some(new_value) = arguments.get("value").cloned() else {
        return Ok(match existing {
            Some(value) => format!("`{path_label}` is currently {value}"),
            None => format!("`{path_label}` is not set; the built-in default applies."),
        });
    };

    // Edited as text rather than reserialized, so the comments and
    // hand-formatting in the settings file survive the write.
    let mut updated = text.clone();
    let mut edits = Vec::new();
    let mut path: Vec<&str> = key_path.iter().map(String::as_str).collect();
    settings_json::update_value_in_json_text(
        &mut updated,
        &mut path,
        settings_json::infer_json_indent_size(&text),
        &current,
        &build_nested(&key_path, new_value.clone(), &current),
        &mut edits,
    );

    // Every setting the editor reads comes through this one file, so a bad
    // write does not break one setting -- it drops the whole app back to
    // defaults, and the user has to find and repair the file by hand.
    if let Err(error) = settings_json::parse_json_with_comments::<Value>(&updated) {
        return Err(format!(
            "Refused to write: the result would not parse ({error}). \
             The settings file is unchanged."
        ));
    }

    // Written beside the file and moved into place, so a crash or a full disk
    // mid-write leaves the old settings rather than a truncated file.
    let staging = settings_path.with_extension(format!("json.{}.tmp", std::process::id()));
    std::fs::write(&staging, &updated)
        .and_then(|()| std::fs::rename(&staging, &settings_path))
        .map_err(|error| {
            std::fs::remove_file(&staging).ok();
            format!("Could not write {}: {error}", settings_path.display())
        })?;

    Ok(format!(
        "Set `{path_label}` to {new_value}. The editor reloads settings on save."
    ))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_appearance_settings_are_reachable() {
        let path = |keys: &[&str]| keys.iter().map(|key| key.to_string()).collect::<Vec<_>>();
        assert!(setting_is_writable(&path(&["theme"])));
        assert!(setting_is_writable(&path(&["tab_bar", "show"])));
        assert!(setting_is_writable(&path(&["terminal", "font_size"])));
        assert!(!setting_is_writable(&path(&["terminal", "shell"])));
        assert!(!setting_is_writable(&path(&["terminal"])));
        assert!(!setting_is_writable(&path(&["context_servers"])));
        assert!(!setting_is_writable(&path(&[
            "agent_servers",
            "claude",
            "command"
        ])));
        assert!(!setting_is_writable(&path(&["lsp"])));
        assert!(!setting_is_writable(&path(&["agent"])));
    }

    #[test]
    fn destructive_and_executing_actions_are_refused() {
        assert!(action_is_allowed("workspace::ToggleBottomDock"));
        assert!(action_is_allowed("theme_selector::Toggle"));
        for blocked in [
            "zed::Quit",
            "task::Spawn",
            "terminal::SendText",
            "git::Push",
            "git::Commit",
            "project_panel::Delete",
            "editor::DeleteLine",
            "extensions::InstallExtension",
            "agent::AskAgent",
            "workspace::CloseWindow",
        ] {
            assert!(!action_is_allowed(blocked), "{blocked} should be refused");
        }
    }

    #[test]
    fn nests_a_value_under_a_missing_path() {
        let current = json!({ "theme": "One Dark" });
        let updated = build_nested(
            &["agent".to_string(), "dock".to_string()],
            json!("right"),
            &current,
        );

        assert_eq!(updated["agent"]["dock"], json!("right"));
        // Untouched keys survive, or the diff would rewrite the whole file.
        assert_eq!(updated["theme"], json!("One Dark"));
    }

    #[test]
    fn replaces_a_non_object_on_the_way_down() {
        // A scalar where the path expects an object would otherwise be indexed
        // into and silently drop the write.
        let current = json!({ "agent": 3 });
        let updated = build_nested(
            &["agent".to_string(), "dock".to_string()],
            json!("left"),
            &current,
        );

        assert_eq!(updated["agent"]["dock"], json!("left"));
    }

    #[test]
    fn lists_the_tools_with_schemas() {
        let tools = tool_definitions();
        let tools = tools.as_array().expect("tool definitions are an array");

        let names = tools
            .iter()
            .filter_map(|tool| tool["name"].as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            ["run_ide_action", "get_diagnostics", "update_ide_setting"]
        );
        for tool in tools {
            assert!(tool["name"].is_string());
            assert!(tool["description"].is_string());
            assert_eq!(tool["inputSchema"]["type"], json!("object"));
        }
    }

    #[test]
    fn reports_tool_failures_as_readable_results() {
        // Not a JSON-RPC error: a refused tool call is something the model
        // should read and act on, not something it can only give up on.
        let content = tool_content("no such action", true);

        assert_eq!(content["isError"], json!(true));
        assert_eq!(content["content"][0]["text"], json!("no such action"));
    }
}
