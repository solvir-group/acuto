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

/// The text either side of the caret, for servers that cannot be handed a
/// fill-in-the-middle prompt directly.
pub(crate) struct CaretContext<'a> {
    pub path: &'a str,
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
    "You are a code completion engine inside a text editor. ",
    "The user sends one file with the caret position marked <|caret|>. ",
    "Reply with ONLY the exact characters to insert at the caret. ",
    "Never repeat text that already appears before or after the caret. ",
    "Never wrap the reply in markdown code fences. ",
    "Never explain, comment on, or restate the task. ",
    "Complete the current expression or statement and stop; do not write the rest of the file. ",
    "If no completion is appropriate, reply with nothing at all.",
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
    let user_message = format!(
        "File: {}\n\n{}{CARET_MARKER}{}",
        caret.path, caret.prefix, caret.suffix
    );

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

    Ok((clean_chat_completion(text), request_id))
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
    fn leading_newlines_are_dropped_but_indentation_is_kept() {
        assert_eq!(clean_chat_completion("\n\n    indented"), "    indented");
    }
}
