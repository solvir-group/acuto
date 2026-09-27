use anyhow::{Context as _, Result};
use cloud_llm_client::predict_edits_v3::{RawCompletionRequest, RawCompletionResponse};
use futures::AsyncReadExt as _;
use gpui::{App, AppContext as _, Entity, Global, SharedString, Task, http_client};
use language::language_settings::{OpenAiCompatibleEditPredictionSettings, all_language_settings};
use language_model::{ApiKeyState, EnvVar, env_var};
use std::sync::Arc;

pub fn open_ai_compatible_api_url(cx: &App) -> SharedString {
    all_language_settings(None, cx)
        .edit_predictions
        .open_ai_compatible_api
        .as_ref()
        .map(|settings| settings.api_url.clone())
        .unwrap_or_default()
        .into()
}

pub const OPEN_AI_COMPATIBLE_CREDENTIALS_USERNAME: &str = "openai-compatible-api-token";
pub static OPEN_AI_COMPATIBLE_TOKEN_ENV_VAR: std::sync::LazyLock<EnvVar> =
    env_var!("ZED_OPEN_AI_COMPATIBLE_EDIT_PREDICTION_API_KEY");

struct GlobalOpenAiCompatibleApiKey(Entity<ApiKeyState>);

impl Global for GlobalOpenAiCompatibleApiKey {}

pub fn open_ai_compatible_api_token(cx: &mut App) -> Entity<ApiKeyState> {
    if let Some(global) = cx.try_global::<GlobalOpenAiCompatibleApiKey>() {
        return global.0.clone();
    }

    let entity = cx.new(|cx| {
        ApiKeyState::new(
            open_ai_compatible_api_url(cx),
            OPEN_AI_COMPATIBLE_TOKEN_ENV_VAR.clone(),
        )
    });
    cx.set_global(GlobalOpenAiCompatibleApiKey(entity.clone()));
    entity
}

pub fn load_open_ai_compatible_api_token(
    cx: &mut App,
) -> Task<Result<(), language_model::AuthenticateError>> {
    let credentials_provider = zed_credentials_provider::global(cx);
    let api_url = open_ai_compatible_api_url(cx);
    open_ai_compatible_api_token(cx).update(cx, |key_state, cx| {
        key_state.load_if_needed(api_url, |s| s, credentials_provider, cx)
    })
}

pub fn load_open_ai_compatible_api_key_if_needed(
    provider: settings::EditPredictionProvider,
    cx: &mut App,
) -> Option<Arc<str>> {
    if provider != settings::EditPredictionProvider::OpenAiCompatibleApi {
        return None;
    }
    _ = load_open_ai_compatible_api_token(cx);
    let url = open_ai_compatible_api_url(cx);
    return open_ai_compatible_api_token(cx).read(cx).key(&url);
}

/// Everything a chat model is told about where the caret is.
///
/// A fill-in-the-middle model needs only the two halves of the hole; it learned
/// the task during training. A chat model has to be told what the task is, what
/// language it is looking at, and enough of the surrounding project to avoid
/// inventing names -- so this carries considerably more.
pub(crate) struct CaretContext<'a> {
    pub path: &'a str,
    /// The buffer's language, when the buffer has one.
    pub language: Option<&'a str>,
    /// Excerpts from elsewhere in the project, already formatted. Empty when
    /// retrieval found nothing.
    pub related: &'a str,
    pub prefix: &'a str,
    pub suffix: &'a str,
}

/// Whether an endpoint is the chat one rather than the raw-completions one.
///
/// Plenty of servers that describe themselves as OpenAI-compatible implement
/// only `/v1/chat/completions`; NVIDIA's `integrate.api.nvidia.com` answers
/// `/v1/completions` with a 404 for every model it serves. Rather than add a
/// second provider for that case, the shape of the configured URL selects the
/// request body, which is the one place the difference actually shows up.
fn is_chat_endpoint(api_url: &str) -> bool {
    api_url.trim_end_matches('/').ends_with("/chat/completions")
}

/// Whether the configured URL is Anthropic's Messages API.
///
/// Same trick as `is_chat_endpoint`, for the same reason: the difference between
/// these providers is one request shape and one response shape, and the URL is
/// the only place it is already written down.
fn is_anthropic_endpoint(api_url: &str) -> bool {
    api_url.trim_end_matches('/').ends_with("/v1/messages")
}

pub(crate) async fn send_custom_server_request(
    provider: settings::EditPredictionProvider,
    settings: &OpenAiCompatibleEditPredictionSettings,
    prompt: String,
    caret: Option<CaretContext<'_>>,
    max_tokens: u32,
    stop_tokens: Vec<String>,
    api_key: Option<Arc<str>>,
    http_client: &Arc<dyn http_client::HttpClient>,
) -> Result<(String, String)> {
    match provider {
        settings::EditPredictionProvider::Ollama => {
            let response = crate::ollama::make_request(
                settings.clone(),
                prompt,
                stop_tokens,
                http_client.clone(),
            )
            .await?;
            Ok((response.response, response.created_at))
        }
        // Only the fill-in-the-middle path can be expressed as a chat turn: the
        // others send a prompt shaped for a base model, and rewriting those for
        // a chat model is a different piece of work. They keep going to the raw
        // endpoint, which is where they already went.
        _ if caret.is_some() && is_anthropic_endpoint(&settings.api_url) => {
            let caret = caret.expect("checked immediately above");
            send_anthropic_messages_request(settings, caret, max_tokens, api_key, http_client).await
        }
        _ if caret.is_some() && is_chat_endpoint(&settings.api_url) => {
            let caret = caret.expect("checked immediately above");
            send_chat_completion_request(settings, caret, max_tokens, api_key, http_client).await
        }
        _ => {
            let request = RawCompletionRequest {
                model: settings.model.clone(),
                prompt,
                max_tokens: Some(max_tokens),
                temperature: None,
                stop: stop_tokens
                    .into_iter()
                    .map(std::borrow::Cow::Owned)
                    .collect(),
                environment: None,
            };

            let request_body = serde_json::to_string(&request)?;
            let mut http_request_builder = http_client::Request::builder()
                .method(http_client::Method::POST)
                .uri(settings.api_url.as_ref())
                .header("Content-Type", "application/json");

            if let Some(api_key) = api_key {
                http_request_builder =
                    http_request_builder.header("Authorization", format!("Bearer {}", api_key));
            }

            let http_request =
                http_request_builder.body(http_client::AsyncBody::from(request_body))?;

            let mut response = http_client.send(http_request).await?;
            let status = response.status();

            if !status.is_success() {
                let mut body = String::new();
                response.body_mut().read_to_string(&mut body).await?;
                anyhow::bail!("custom server error: {} - {}", status, body);
            }

            let mut body = String::new();
            response.body_mut().read_to_string(&mut body).await?;

            let parsed: RawCompletionResponse =
                serde_json::from_str(&body).context("Failed to parse completion response")?;
            let text = parsed
                .choices
                .into_iter()
                .next()
                .map(|choice| choice.text)
                .unwrap_or_default();
            Ok((text, parsed.id))
        }
    }
}

/// The caret marker handed to a chat model.
///
/// Chosen to be something no source file contains and no tokenizer is likely to
/// treat as a special token, so the model sees it as text it has been told about
/// rather than as an instruction it half-remembers from training.
const CARET_MARKER: &str = "<|caret|>";

const COMPLETION_SYSTEM_PROMPT: &str = concat!(
    "You are a code completion engine inside a text editor.\n\n",
    "The user sends the file being edited with the caret marked <|caret|>, and \
     may send excerpts from elsewhere in the same project first. Reply with the \
     exact characters to insert at the caret, and nothing else.\n\n",
    "Rules:\n",
    "1. Output raw code. No markdown fences, no prose, no explanation, no \
     restating the task.\n",
    "2. Never repeat text that already appears immediately before or after the \
     caret. Your output is inserted between them verbatim.\n",
    "3. Write what the user is clearly in the middle of writing: finish the \
     current line, and when the intent is plain, the rest of the block or \
     function body -- several lines is normal. Stop at the end of that block. \
     Do not write the rest of the file.\n",
    "4. Use only identifiers that appear in the file or in the project excerpts \
     you were given. If you need something that does not exist, stop instead of \
     inventing a name.\n",
    "5. Match the surrounding code: its language, its indentation width and \
     character, its quote style, and whether it uses semicolons.\n",
    "6. Continue the current line before starting a new one. If the caret sits \
     mid-line, your first character continues that line.\n",
    "7. Always offer your best completion, including the next line or lines \
     after a finished statement when the code makes the next step clear. Reply \
     with nothing only when no code could sensibly go at the caret.\n",
    "8. Line breaks are part of the answer. When the caret is at the end of a \
     line and your code belongs on the next line, start your reply with a \
     newline. Put every statement on its own line, exactly as the file lays \
     out its code, and give every line after the first its full indentation.\n",
    "9. The result must be valid code once inserted: close every bracket, \
     quote, string and tag you open, unless the text after the caret already \
     closes it.",
);

/// A worked example, in the exact shape a real request arrives in.
///
/// Rules in a system prompt tell a chat model what to do; a turn it can see
/// itself having taken tells it what its output is supposed to look like. This
/// is the one that stops the two commonest failures at once -- answering in
/// prose, and re-typing the line the caret is already on.
const SHOWN_MID_LINE: &str = concat!(
    "Language: JavaScript\n",
    "File being edited: cart.js\n\n",
    "function total(items) {\n",
    "  let sum = 0;\n",
    "  for (const item of items) {\n",
    "    sum += item.pri",
    "<|caret|>",
    "\n  }\n",
    "  return sum;\n",
    "}\n",
);

/// Note what is *not* here: no `sum += `, which is already before the caret,
/// and no `}` or `return sum;`, which are already after it.
const SHOWN_MID_LINE_REPLY: &str = "ce * item.quantity;";

/// Asks a chat model to fill in at the caret.
///
/// This is a worse instrument than a fill-in-the-middle base model: it costs a
/// system prompt, it will occasionally answer in prose, and its latency is a
/// chat turn rather than a few tokens. It exists because a chat endpoint is
/// frequently the only one on offer, and a slower completion is worth more than
/// the nothing a 404 produces.
/// The turn describing where the caret is and what surrounds it.
///
/// Shared by both transports so the two cannot drift: a prompt change that
/// improved one and not the other would show up as one provider quietly getting
/// worse, which is the kind of bug nobody thinks to look for.
fn caret_user_message(caret: &CaretContext<'_>) -> String {
    let mut user_message = String::new();
    if !caret.related.is_empty() {
        user_message.push_str(
            "Excerpts from elsewhere in this project, for reference only. Do not \
             continue these; they are not where the caret is.\n\n",
        );
        user_message.push_str(caret.related);
        user_message.push('\n');
    }
    if let Some(language) = caret.language {
        user_message.push_str(&format!("Language: {language}\n"));
    }
    user_message.push_str(&format!(
        "File being edited: {}\n\n{}{CARET_MARKER}{}",
        caret.path, caret.prefix, caret.suffix
    ));
    user_message
}

/// Asks Claude to fill in at the caret, over Anthropic's Messages API.
///
/// Two differences from the chat-completions path, both forced by the API rather
/// than chosen: the system prompt is a top-level field instead of a message, and
/// the "correct answer is nothing" example is dropped, because Anthropic rejects
/// an assistant turn whose content is empty. Rule 7 of the system prompt still
/// says an empty reply is correct; it just cannot be demonstrated here.
async fn send_anthropic_messages_request(
    settings: &OpenAiCompatibleEditPredictionSettings,
    caret: CaretContext<'_>,
    max_tokens: u32,
    api_key: Option<Arc<str>>,
    http_client: &Arc<dyn http_client::HttpClient>,
) -> Result<(String, String)> {
    let user_message = caret_user_message(&caret);

    let body = serde_json::json!({
        "model": settings.model,
        "max_tokens": max_tokens,
        "temperature": 0.0,
        "system": COMPLETION_SYSTEM_PROMPT,
        "messages": [
            { "role": "user", "content": SHOWN_MID_LINE },
            { "role": "assistant", "content": SHOWN_MID_LINE_REPLY },
            { "role": "user", "content": user_message },
        ],
    });

    let mut http_request_builder = http_client::Request::builder()
        .method(http_client::Method::POST)
        .uri(settings.api_url.as_ref())
        .header("Content-Type", "application/json")
        // Pinned rather than tracking latest: a version bump can change response
        // shapes, and a completion engine that breaks on someone else's release
        // day is worse than one that asks for an old contract.
        .header("anthropic-version", "2023-06-01");

    if let Some(api_key) = api_key {
        http_request_builder = http_request_builder.header("x-api-key", api_key.as_ref());
    }

    let http_request =
        http_request_builder.body(http_client::AsyncBody::from(serde_json::to_string(&body)?))?;

    let mut response = http_client.send(http_request).await?;
    let status = response.status();

    let mut response_body = String::new();
    response
        .body_mut()
        .read_to_string(&mut response_body)
        .await?;

    if !status.is_success() {
        anyhow::bail!("anthropic messages error: {} - {}", status, response_body);
    }

    let parsed: serde_json::Value =
        serde_json::from_str(&response_body).context("Failed to parse Anthropic response")?;

    // `content` is a list of blocks. Only text blocks carry a completion; a
    // thinking block or a tool-use block is not something to insert.
    let text = parsed
        .get("content")
        .and_then(|content| content.as_array())
        .map(|blocks| {
            blocks
                .iter()
                .filter(|block| block.get("type").and_then(|kind| kind.as_str()) == Some("text"))
                .filter_map(|block| block.get("text").and_then(|text| text.as_str()))
                .collect::<String>()
        })
        .unwrap_or_default();

    let request_id = parsed
        .get("id")
        .and_then(|id| id.as_str())
        .unwrap_or_default()
        .to_string();

    let completion = clean_chat_completion(&text);
    let completion = fit_to_caret_line(completion, caret.prefix, caret.suffix);
    let completion = trim_overlap_with_buffer(completion, caret.prefix, caret.suffix);
    let completion = stop_at_repeated_line(completion, caret.suffix);
    let completion = cap_lines(completion);

    log_exchange(&user_message, &text, &completion);

    Ok((completion, request_id))
}

/// Posts one chat-completion body and returns the status with the whole reply.
async fn post_chat_completion(
    settings: &OpenAiCompatibleEditPredictionSettings,
    api_key: &Option<Arc<str>>,
    body: &serde_json::Value,
    http_client: &Arc<dyn http_client::HttpClient>,
) -> Result<(http_client::StatusCode, String)> {
    let mut http_request_builder = http_client::Request::builder()
        .method(http_client::Method::POST)
        .uri(settings.api_url.as_ref())
        .header("Content-Type", "application/json");

    if let Some(api_key) = api_key {
        http_request_builder =
            http_request_builder.header("Authorization", format!("Bearer {}", api_key));
    }

    let http_request =
        http_request_builder.body(http_client::AsyncBody::from(serde_json::to_string(body)?))?;

    let mut response = http_client.send(http_request).await?;
    let status = response.status();

    let mut response_body = String::new();
    response
        .body_mut()
        .read_to_string(&mut response_body)
        .await?;
    Ok((status, response_body))
}

async fn send_chat_completion_request(
    settings: &OpenAiCompatibleEditPredictionSettings,
    caret: CaretContext<'_>,
    max_tokens: u32,
    api_key: Option<Arc<str>>,
    http_client: &Arc<dyn http_client::HttpClient>,
) -> Result<(String, String)> {
    let user_message = caret_user_message(&caret);

    let mut body = serde_json::json!({
        "model": settings.model,
        "max_tokens": max_tokens,
        "temperature": 0.0,
        // Reasoning models otherwise narrate their way to the answer inside
        // `content`, and the whole narration arrives as the completion. This is
        // a vendor extension that servers which do not recognise it ignore;
        // drop it if a server rejects unknown request fields outright.
        "chat_template_kwargs": { "enable_thinking": false },
        // The switch that actually works. GLM on NVIDIA ignores the flag above
        // and reasons anyway -- a few hundred characters before the answer --
        // so a completion's token budget ran out mid-reasoning and every reply
        // came back empty after four to eleven seconds. "low" skips the
        // reasoning entirely: the same request answers in about a second.
        "reasoning_effort": "low",
        "messages": [
            { "role": "system", "content": COMPLETION_SYSTEM_PROMPT },
            { "role": "user", "content": SHOWN_MID_LINE },
            { "role": "assistant", "content": SHOWN_MID_LINE_REPLY },
            // No worked example of declining. It taught the model that a
            // finished line is a place to say nothing -- which is exactly where
            // someone wants the next line suggested -- and most predictions
            // came back empty.
            { "role": "user", "content": user_message },
        ],
    });

    let rejects_reasoning_effort =
        servers_rejecting_reasoning_effort(|servers| servers.contains(&*settings.api_url));
    if rejects_reasoning_effort && let Some(fields) = body.as_object_mut() {
        fields.remove("reasoning_effort");
    }

    let (mut status, mut response_body) =
        post_chat_completion(settings, &api_key, &body, http_client).await?;

    // Some servers reject the field outright rather than ignoring it -- OpenAI
    // does for models that do not reason. Those get the request again without
    // it, and are remembered, so the switch costs them one round trip once
    // rather than on every keystroke.
    if !rejects_reasoning_effort
        && status == http_client::StatusCode::BAD_REQUEST
        && response_body.contains("reasoning_effort")
    {
        log::info!("fim: server rejects reasoning_effort; retrying without it");
        servers_rejecting_reasoning_effort(|servers| {
            servers.insert(settings.api_url.to_string());
        });
        if let Some(fields) = body.as_object_mut() {
            fields.remove("reasoning_effort");
        }
        (status, response_body) =
            post_chat_completion(settings, &api_key, &body, http_client).await?;
    }

    if !status.is_success() {
        anyhow::bail!(
            "chat completion server error: {} - {}",
            status,
            response_body
        );
    }

    let parsed: serde_json::Value =
        serde_json::from_str(&response_body).context("Failed to parse chat completion response")?;

    let text = parsed
        .get("choices")
        .and_then(|choices| choices.get(0))
        .and_then(|choice| choice.get("message"))
        .and_then(|message| message.get("content"))
        .and_then(|content| content.as_str())
        .unwrap_or_default();

    let request_id = parsed
        .get("id")
        .and_then(|id| id.as_str())
        .unwrap_or_default()
        .to_string();

    let completion = clean_chat_completion(text);
    let completion = fit_to_caret_line(completion, caret.prefix, caret.suffix);
    let completion = trim_overlap_with_buffer(completion, caret.prefix, caret.suffix);
    let completion = stop_at_repeated_line(completion, caret.suffix);
    let completion = cap_lines(completion);

    log_exchange(&user_message, text, &completion);

    Ok((completion, request_id))
}

/// Endpoints that answered a request carrying `reasoning_effort` with an error
/// about it, for the rest of the session.
fn servers_rejecting_reasoning_effort<R>(
    f: impl FnOnce(&mut std::collections::HashSet<String>) -> R,
) -> R {
    static SERVERS: std::sync::Mutex<Option<std::collections::HashSet<String>>> =
        std::sync::Mutex::new(None);
    // A poisoned lock only means another prediction panicked mid-insert; the
    // set is still a set, so it is used as it stands.
    let mut servers = SERVERS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    f(servers.get_or_insert_with(Default::default))
}

/// Where the wire log goes, if anywhere.
///
/// Read once: this is on the path of every keystroke that triggers a
/// prediction, and reading the environment per request would be a syscall for
/// nothing on the overwhelmingly common path where it is unset.
static WIRE_LOG: std::sync::LazyLock<Option<std::path::PathBuf>> =
    std::sync::LazyLock::new(|| std::env::var_os("ACUTO_PREDICTION_LOG").map(Into::into));

/// Appends one request and its reply to the wire log.
///
/// Inline predictions are judged on output nobody can see the input for, so a
/// bad suggestion is otherwise diagnosed by guessing at the prompt. This writes
/// what was actually sent, what actually came back, and what survived the
/// cleanup, which turns "the suggestions are wrong" into a file you can read.
///
/// Off unless `ACUTO_PREDICTION_LOG` names a path, because the prompt contains
/// the source being edited and that should never be written anywhere by
/// default. Failures are logged rather than propagated: a diagnostic that can
/// fail a completion is worse than no diagnostic.
fn log_exchange(user_message: &str, raw: &str, kept: &str) {
    use std::io::Write as _;

    let Some(path) = WIRE_LOG.as_ref() else {
        return;
    };

    let record = format!(
        "================================ REQUEST\n{user_message}\n         -------------------------------- RAW REPLY\n{raw}\n         -------------------------------- SHOWN\n{kept}\n\n"
    );

    let appended = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut file| file.write_all(record.as_bytes()));
    if let Err(error) = appended {
        log::warn!("could not write the prediction wire log to {path:?}: {error}");
    }
}

/// Cuts the completion at the first line that already appears just after the
/// caret.
///
/// The overlap trim only catches repetition that touches the caret exactly. The
/// commoner failure is a model that writes two plausible lines and then
/// re-emits a line from further down the file -- the text still parses, so the
/// syntax gate passes it, and the user gets a duplicated statement they have to
/// spot and undo.
///
/// Only lines with something on them: a blank line or a lone brace appears
/// everywhere, and cutting on those would truncate almost every completion.
fn stop_at_repeated_line(completion: String, suffix: &str) -> String {
    /// A line shorter than this is punctuation, not content.
    const MIN_MEANINGFUL: usize = 4;
    /// How far ahead to look. Repetition beyond this is not the model echoing
    /// what it was shown, it is a coincidence.
    const LOOKAHEAD_LINES: usize = 30;

    let ahead: Vec<&str> = suffix
        .lines()
        .take(LOOKAHEAD_LINES)
        .map(str::trim)
        .filter(|line| line.len() >= MIN_MEANINGFUL)
        .collect();
    if ahead.is_empty() {
        return completion;
    }

    let mut kept = String::new();
    let mut past_first_line = false;
    for line in completion.split_inclusive('\n') {
        let trimmed = line.trim();
        // The first line of code continues what the caret is on, so it is
        // judged by the overlap trim rather than here. Counted from the first
        // line with something on it: a completion for the next line begins
        // with a bare newline, and that is not the line being continued.
        if past_first_line && trimmed.len() >= MIN_MEANINGFUL && ahead.contains(&trimmed) {
            break;
        }
        if !trimmed.is_empty() {
            past_first_line = true;
        }
        kept.push_str(line);
    }

    kept
}

/// Bounds how much a completion may be.
///
/// Inline completions are read at a glance while typing. Past a few lines the
/// reader cannot check it faster than writing it, so a long one is not a better
/// suggestion, it is a worse interaction -- and a chat model handed a file will
/// happily produce twenty.
fn cap_lines(completion: String) -> String {
    /// Enough for a whole block -- a function body, a loop with its contents,
    /// a component's markup -- rather than finishing one line at a time. The
    /// prompt already tells the model to stop at the end of the block, and
    /// `stop_at_repeated_line` cuts it off if it starts re-typing what is
    /// below the caret, so this is a ceiling, not the usual length.
    const MAX_LINES: usize = 40;

    let mut kept = String::new();
    for (index, line) in completion.split_inclusive('\n').enumerate() {
        if index >= MAX_LINES {
            break;
        }
        kept.push_str(line);
    }

    // A trailing newline would put the caret on a blank line after accepting,
    // which is never what was wanted.
    kept.trim_end_matches(['\n', '\r']).to_string()
}

/// Removes the parts of a completion that duplicate what is already in the
/// buffer.
///
/// Told not to repeat the surrounding text, a chat model does it anyway often
/// enough to matter: it restates the line it is completing, or closes a block
/// that the text after the caret already closes. The completion is inserted
/// between the two halves verbatim, so either one produces visibly broken code
/// the moment it is accepted.
fn trim_overlap_with_buffer(completion: String, prefix: &str, suffix: &str) -> String {
    /// A single character in common is coincidence: a completion may honestly
    /// begin with the same bracket the preceding text ended with.
    const MIN_OVERLAP: usize = 2;
    /// How far into the surrounding text to look. Beyond this the model is not
    /// repeating itself, it is rewriting the file, and that is a different
    /// failure -- one this cannot repair by trimming.
    const WINDOW: usize = 400;

    let mut completion = completion;

    // Opening by re-typing what is already behind the caret.
    if let Some(overlap) = longest_overlap(window_end(prefix, WINDOW), &completion, MIN_OVERLAP) {
        completion = completion[overlap..].to_string();
    }

    // Closing by re-typing what is already ahead of it.
    if let Some(overlap) = longest_overlap(&completion, window_start(suffix, WINDOW), MIN_OVERLAP) {
        let keep = completion.len() - overlap;
        completion.truncate(keep);
    }

    completion
}

/// The length in bytes of the longest string that both ends `left` and starts
/// `right`, or `None` if the longest is shorter than `minimum`.
fn longest_overlap(left: &str, right: &str, minimum: usize) -> Option<usize> {
    let max = left.len().min(right.len());
    if max < minimum {
        return None;
    }
    (minimum..=max).rev().find(|&length| {
        let split = left.len() - length;
        left.is_char_boundary(split)
            && right.is_char_boundary(length)
            && left[split..] == right[..length]
    })
}

/// The last `max` bytes of `text`, moved forward to a character boundary.
fn window_end(text: &str, max: usize) -> &str {
    let mut start = text.len().saturating_sub(max);
    while start < text.len() && !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}

/// The first `max` bytes of `text`, moved back to a character boundary.
fn window_start(text: &str, max: usize) -> &str {
    let mut end = max.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Drops leading line breaks unless the caret ends a line of code.
///
/// In the middle of a line the completion has to continue it: a newline first
/// would split the line and push the rest of it down. On a blank line the
/// caret is already where the code goes, and a newline would leave the blank
/// line behind. Only after the last character of a line of code does a
/// leading newline mean what it says -- this belongs on the next line.
fn fit_to_caret_line(completion: String, prefix: &str, suffix: &str) -> String {
    let before_caret = prefix.rsplit('\n').next().unwrap_or_default();
    let after_caret = suffix.split('\n').next().unwrap_or_default();
    let ends_a_line_of_code = !before_caret.trim().is_empty() && after_caret.trim().is_empty();
    if ends_a_line_of_code {
        return completion;
    }
    completion.trim_start_matches(['\n', '\r']).to_string()
}

/// Strips the shapes a chat model reaches for even when told not to.
///
/// A model that ignores "no markdown" wraps the answer in a fence. Fenced
/// content is unwrapped because the completion is inside it; prose is not
/// detectable in general and is left alone, since guessing wrong would silently
/// discard a real completion.
fn clean_chat_completion(response: &str) -> String {
    // A leading newline is kept: it is how a completion that belongs on the
    // next line says so. Stripping it glued that line onto the caret's own,
    // `{` followed by the body on the same line. `fit_to_caret_line` removes
    // it in the one case it is wrong.
    let Some(after_open) = response.trim_start().strip_prefix("```") else {
        return response.to_string();
    };

    // The opening fence may carry a language tag, which runs to the end of that
    // line and is not part of the completion.
    let Some((_language, body)) = after_open.split_once('\n') else {
        return String::new();
    };

    match body.rfind("```") {
        Some(close) => body[..close].trim_end_matches(['\n', '\r']).to_string(),
        None => body.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_endpoints_are_recognised_by_shape() {
        assert!(is_chat_endpoint(
            "https://integrate.api.nvidia.com/v1/chat/completions"
        ));
        assert!(is_chat_endpoint(
            "https://integrate.api.nvidia.com/v1/chat/completions/"
        ));
        assert!(!is_chat_endpoint(
            "https://integrate.api.nvidia.com/v1/completions"
        ));
        assert!(!is_chat_endpoint("http://localhost:11434/api/generate"));
    }

    #[test]
    fn a_completion_for_the_next_line_keeps_its_line_break() {
        let completion = clean_chat_completion("\n    total += 1;");
        assert_eq!(
            fit_to_caret_line(completion, "    let mut total = 0;", "\n}\n"),
            "\n    total += 1;"
        );
    }

    #[test]
    fn a_completion_in_the_middle_of_a_line_continues_it() {
        let completion = clean_chat_completion("\nitem.price");
        assert_eq!(
            fit_to_caret_line(completion, "    sum(", ");\n}\n"),
            "item.price"
        );
    }

    #[test]
    fn a_completion_on_a_blank_line_does_not_leave_it_behind() {
        let completion = clean_chat_completion("\n    total += 1;");
        assert_eq!(
            fit_to_caret_line(completion, "{\n    ", "\n}\n"),
            "    total += 1;"
        );
    }

    #[test]
    fn fenced_replies_are_unwrapped() {
        assert_eq!(clean_chat_completion("a + b"), "a + b");
        assert_eq!(clean_chat_completion("```python\na + b\n```"), "a + b");
        assert_eq!(clean_chat_completion("```\na + b\n```"), "a + b");
        // An unterminated fence still carries a usable completion.
        assert_eq!(clean_chat_completion("```rust\n.sum()"), ".sum()");
        // A fence with nothing after it says nothing.
        assert_eq!(clean_chat_completion("```"), "");
    }

    #[test]
    fn a_line_that_already_appears_ahead_ends_the_completion() {
        let suffix = "\n    document.getElementById(\"score\").innerText = score;\n}\n";
        let completion = "score++;\n    document.getElementById(\"score\").innerText = score;\n";
        assert_eq!(
            stop_at_repeated_line(completion.into(), suffix),
            "score++;\n"
        );
    }

    #[test]
    fn punctuation_lines_do_not_end_the_completion() {
        // `}` appears everywhere; cutting on it would truncate almost anything.
        let suffix = "\n}\n";
        let completion = "if (x) {\n  y();\n}\n";
        assert_eq!(
            stop_at_repeated_line(completion.clone().into(), suffix),
            completion
        );
    }

    #[test]
    fn a_completion_is_capped_and_loses_its_trailing_newline() {
        // A whole block survives: predictions write function bodies, not one
        // line at a time.
        let block = "one\ntwo\nthree\nfour\n";
        assert_eq!(cap_lines(block.into()), "one\ntwo\nthree\nfour");
        // A runaway reply is still stopped at the ceiling.
        let runaway: String = (0..100).map(|line| format!("line {line}\n")).collect();
        assert_eq!(cap_lines(runaway).lines().count(), 40);
        assert_eq!(cap_lines("only\n".into()), "only");
        assert_eq!(cap_lines("a + b".into()), "a + b");
        assert_eq!(cap_lines(String::new()), "");
    }

    #[test]
    fn a_completion_that_retypes_the_buffer_is_trimmed() {
        // Restates what is already behind the caret.
        assert_eq!(
            trim_overlap_with_buffer("total.sum()".into(), "    let x = total", ""),
            ".sum()"
        );
        // Closes a block the text ahead of the caret already closes.
        assert_eq!(
            trim_overlap_with_buffer("a + b;\n}".into(), "    return ", "\n}"),
            "a + b;"
        );
        // Wholly duplicated: everything it offered is already there.
        assert_eq!(
            trim_overlap_with_buffer(");".into(), "    foo(bar", ");"),
            ""
        );
        // Nothing in common survives untouched.
        assert_eq!(
            trim_overlap_with_buffer("a + b".into(), "    return ", "\n"),
            "a + b"
        );
        // One character in common is coincidence, not repetition.
        assert_eq!(trim_overlap_with_buffer("(x)".into(), "foo(", ""), "(x)");
    }

    #[test]
    fn overlap_is_measured_across_lines_not_just_the_current_one() {
        // The duplicated part spans a newline, which a line-at-a-time
        // comparison would miss entirely.
        assert_eq!(
            trim_overlap_with_buffer("value;\n    }\n}".into(), "        return ", "\n    }\n}"),
            "value;"
        );
    }

    #[test]
    fn trimming_never_splits_a_character() {
        // A multi-byte character on the overlap boundary must not be cut
        // through. Rust would panic on a non-boundary slice, so reaching the
        // assertion at all is the property under test.
        let trimmed = trim_overlap_with_buffer("é_value".into(), "let café", "");
        assert!(trimmed.is_char_boundary(0));
        let trimmed = trim_overlap_with_buffer("value_é".into(), "", "é_rest");
        assert!(trimmed.is_char_boundary(0));
    }

    #[test]
    fn leading_newlines_are_dropped_on_a_blank_line_but_indentation_is_kept() {
        // Cleaning leaves the newlines alone; where the caret is decides.
        let completion = clean_chat_completion("\n\n    indented");
        assert_eq!(completion, "\n\n    indented");
        assert_eq!(
            fit_to_caret_line(completion, "fn main() {\n    ", "\n}\n"),
            "    indented"
        );
    }
}
