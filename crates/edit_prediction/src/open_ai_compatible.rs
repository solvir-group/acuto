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
    "3. Complete the current expression, statement or block and stop. Do not \
     write the rest of the file.\n",
    "4. Use only identifiers that appear in the file or in the project excerpts \
     you were given. If you need something that does not exist, stop instead of \
     inventing a name.\n",
    "5. Match the surrounding code: its language, its indentation width and \
     character, its quote style, and whether it uses semicolons.\n",
    "6. Continue the current line before starting a new one. If the caret sits \
     mid-line, your first character continues that line.\n",
    "7. If you cannot complete confidently, reply with nothing at all. An empty \
     reply is correct and costs the user nothing; a wrong one costs them a \
     read and an undo.",
);

/// Asks a chat model to fill in at the caret.
///
/// This is a worse instrument than a fill-in-the-middle base model: it costs a
/// system prompt, it will occasionally answer in prose, and its latency is a
/// chat turn rather than a few tokens. It exists because a chat endpoint is
/// frequently the only one on offer, and a slower completion is worth more than
/// the nothing a 404 produces.
async fn send_chat_completion_request(
    settings: &OpenAiCompatibleEditPredictionSettings,
    caret: CaretContext<'_>,
    max_tokens: u32,
    api_key: Option<Arc<str>>,
    http_client: &Arc<dyn http_client::HttpClient>,
) -> Result<(String, String)> {
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

    let body = serde_json::json!({
        "model": settings.model,
        "max_tokens": max_tokens,
        "temperature": 0.0,
        // Reasoning models otherwise narrate their way to the answer inside
        // `content`, and the whole narration arrives as the completion. This is
        // a vendor extension that servers which do not recognise it ignore;
        // drop it if a server rejects unknown request fields outright.
        "chat_template_kwargs": { "enable_thinking": false },
        "messages": [
            { "role": "system", "content": COMPLETION_SYSTEM_PROMPT },
            { "role": "user", "content": user_message },
        ],
    });

    let mut http_request_builder = http_client::Request::builder()
        .method(http_client::Method::POST)
        .uri(settings.api_url.as_ref())
        .header("Content-Type", "application/json");

    if let Some(api_key) = api_key {
        http_request_builder =
            http_request_builder.header("Authorization", format!("Bearer {}", api_key));
    }

    let http_request = http_request_builder
        .body(http_client::AsyncBody::from(serde_json::to_string(&body)?))?;

    let mut response = http_client.send(http_request).await?;
    let status = response.status();

    let mut response_body = String::new();
    response
        .body_mut()
        .read_to_string(&mut response_body)
        .await?;

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
    let completion = trim_overlap_with_buffer(completion, caret.prefix, caret.suffix);
    let completion = stop_at_repeated_line(completion, caret.suffix);
    Ok((cap_lines(completion), request_id))
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
    for (index, line) in completion.split_inclusive('\n').enumerate() {
        let trimmed = line.trim();
        // The first line continues what the caret is on, so it is judged by the
        // overlap trim rather than here.
        if index > 0 && trimmed.len() >= MIN_MEANINGFUL && ahead.contains(&trimmed) {
            break;
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
    /// Two lines after the one the caret is on. Enough for a closing brace or
    /// a return, not enough to write a function nobody asked for.
    const MAX_LINES: usize = 3;

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

/// Strips the shapes a chat model reaches for even when told not to.
///
/// A model that ignores "no markdown" wraps the answer in a fence. Fenced
/// content is unwrapped because the completion is inside it; prose is not
/// detectable in general and is left alone, since guessing wrong would silently
/// discard a real completion.
fn clean_chat_completion(response: &str) -> String {
    let trimmed = response.trim_start_matches(['\n', '\r']);

    let Some(after_open) = trimmed.trim_start().strip_prefix("```") else {
        return trimmed.to_string();
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
        assert_eq!(cap_lines("one\ntwo\nthree\nfour\n".into()), "one\ntwo\nthree");
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
        assert_eq!(trim_overlap_with_buffer(");".into(), "    foo(bar", ");"), "");
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
            trim_overlap_with_buffer(
                "value;\n    }\n}".into(),
                "        return ",
                "\n    }\n}"
            ),
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
    fn leading_newlines_are_dropped_but_indentation_is_kept() {
        assert_eq!(clean_chat_completion("\n\n    indented"), "    indented");
    }
}
