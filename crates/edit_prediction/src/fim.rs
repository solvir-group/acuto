use crate::{
    EditPredictionId, EditPredictionInputs, EditPredictionModelInput, cursor_excerpt,
    open_ai_compatible::{self, load_open_ai_compatible_api_key_if_needed},
    prediction::EditPredictionResult,
};
use anyhow::{Context as _, Result, anyhow};
use gpui::{App, AppContext as _, Entity, Task};
use language::{
    Anchor, Buffer, BufferSnapshot, EditPredictionPromptFormat, ToOffset, ToPoint as _,
    language_settings::all_language_settings,
};
use std::{path::Path, sync::Arc, time::Instant};
use zeta_prompt::{Zeta2PromptInput, compute_editable_and_context_ranges};

const FIM_CONTEXT_TOKENS: usize = 512;

struct FimRequestOutput {
    request_id: String,
    edits: Vec<(std::ops::Range<Anchor>, Arc<str>)>,
    editable_range: std::ops::Range<Anchor>,
    snapshot: BufferSnapshot,
    inputs: Zeta2PromptInput,
    buffer: Entity<Buffer>,
}

pub fn request_prediction(
    EditPredictionModelInput {
        buffer,
        snapshot,
        position,
        events,
        trigger,
        related_files,
        ..
    }: EditPredictionModelInput,
    prompt_format: EditPredictionPromptFormat,
    cx: &mut App,
) -> Task<Result<Option<EditPredictionResult>>> {
    let settings = &all_language_settings(None, cx).edit_predictions;
    let provider = settings.provider;

    let full_path: Arc<Path> = snapshot
        .file()
        .map(|file| file.full_path(cx))
        .unwrap_or_else(|| "untitled".into())
        .into();

    let http_client = cx.http_client();
    let cursor_point = position.to_point(&snapshot);
    let request_start = cx.background_executor().now();

    // The language name, so a chat model is not left inferring the syntax from
    // a filename it may not recognise.
    let language_name = snapshot
        .language()
        .map(|language| language.name().to_string());

    // The grammar, for checking afterwards that whatever comes back parses.
    // Taken here because `snapshot` moves into the background task.
    let grammar = snapshot
        .language()
        .and_then(|language| language.grammar())
        .map(|grammar| grammar.ts_language.clone());

    // Excerpts from elsewhere in the project, already retrieved for this
    // request by the same store the Zeta path uses. Dropping them -- which this
    // did -- is why completions invented functions that do not exist: the model
    // was shown a few hundred tokens around the caret and nothing else.
    let related_context = format_related_files(&related_files);

    let Some(settings) = (match provider {
        settings::EditPredictionProvider::Ollama => settings.ollama.clone(),
        settings::EditPredictionProvider::OpenAiCompatibleApi => {
            settings.open_ai_compatible_api.clone()
        }
        _ => None,
    }) else {
        return Task::ready(Err(anyhow!("Unsupported edit prediction provider for FIM")));
    };

    let api_key = load_open_ai_compatible_api_key_if_needed(provider, cx);

    let result = cx.background_spawn(async move {
        let cursor_offset = cursor_point.to_offset(&snapshot);
        let (excerpt_point_range, excerpt_offset_range, cursor_offset_in_excerpt) =
            cursor_excerpt::compute_cursor_excerpt(&snapshot, cursor_offset);
        let cursor_excerpt: Arc<str> = snapshot
            .text_for_range(excerpt_point_range.clone())
            .collect::<String>()
            .into();
        let syntax_ranges =
            cursor_excerpt::compute_syntax_ranges(&snapshot, cursor_offset, &excerpt_offset_range);
        let (editable_range, _) = compute_editable_and_context_ranges(
            &cursor_excerpt,
            cursor_offset_in_excerpt,
            &syntax_ranges,
            FIM_CONTEXT_TOKENS,
            0,
        );

        let inputs = Zeta2PromptInput {
            events,
            related_files: Some(related_files),
            active_buffer_diagnostics: Vec::new(),
            cursor_offset_in_excerpt: cursor_offset - excerpt_offset_range.start,
            cursor_path: full_path.clone(),
            excerpt_start_row: Some(excerpt_point_range.start.row),
            cursor_excerpt,
            excerpt_ranges: Default::default(),
            syntax_ranges: None,
            in_open_source_repo: false,
            can_collect_data: false,
            repo_url: None,
        };

        let editable_text = &inputs.cursor_excerpt[editable_range.clone()];
        let cursor_in_editable = cursor_offset_in_excerpt.saturating_sub(editable_range.start);
        let prefix = editable_text[..cursor_in_editable].to_string();
        let suffix = editable_text[cursor_in_editable..].to_string();
        let prompt = format_fim_prompt(prompt_format, &prefix, &suffix);
        let stop_tokens = get_fim_stop_tokens();
        let display_path = full_path.to_string_lossy().into_owned();

        // A chat model is given the whole cursor excerpt rather than the narrow
        // editable window the fill-in-the-middle template uses. The editable
        // window is deliberately small because a FIM model only needs to see
        // where the hole is; a chat model has to work out what the code around
        // it means, and 512 tokens is not enough to do that.
        let excerpt = inputs.cursor_excerpt.as_ref();
        let caret_in_excerpt = inputs.cursor_offset_in_excerpt.min(excerpt.len());
        let (excerpt_prefix, excerpt_suffix) = excerpt.split_at(caret_in_excerpt);

        let max_tokens = settings.max_output_tokens;

        let (response_text, request_id) = open_ai_compatible::send_custom_server_request(
            provider,
            &settings,
            prompt,
            // A chat endpoint cannot be handed a fill-in-the-middle prompt, so
            // the two halves travel alongside it and the transport picks.
            Some(open_ai_compatible::CaretContext {
                path: &display_path,
                language: language_name.as_deref(),
                related: &related_context,
                prefix: excerpt_prefix,
                suffix: excerpt_suffix,
            }),
            max_tokens,
            stop_tokens,
            api_key,
            &http_client,
        )
        .await?;

        let response_received_at = Instant::now();

        log::debug!(
            "fim: completion received ({:.2}s)",
            (response_received_at - request_start).as_secs_f64()
        );

        let completion = clean_fim_completion(&response_text);

        // Refuse anything that makes the file parse worse than it already did.
        //
        // A completion is inserted verbatim, so a model that closes a brace
        // that was already closed, or opens one it never closes, produces code
        // that does not compile -- and the person accepting it finds out later,
        // somewhere else. This is the one property that can be checked here
        // cheaply and exactly, so it is checked.
        //
        // Compared against the file as it stands rather than against zero:
        // half-typed code is full of parse errors by definition, and a
        // completion is not responsible for the ones that were already there.
        let completion = match &grammar {
            Some(language) if !completion.is_empty() => {
                let cursor_offset = cursor_point.to_offset(&snapshot);
                let text = snapshot.text();
                if worsens_syntax(language, &text, cursor_offset, &completion) {
                    log::debug!("fim: dropped a completion that added syntax errors");
                    String::new()
                } else {
                    completion
                }
            }
            _ => completion,
        };

        let completion: Arc<str> = completion.into();
        let edits = if completion.is_empty() {
            vec![]
        } else {
            let cursor_offset = cursor_point.to_offset(&snapshot);
            let anchor = snapshot.anchor_after(cursor_offset);
            vec![(anchor..anchor, completion)]
        };

        let editable_range = snapshot.anchor_range_inside(
            (excerpt_offset_range.start + editable_range.start)
                ..(excerpt_offset_range.start + editable_range.end),
        );

        anyhow::Ok(FimRequestOutput {
            request_id,
            edits,
            editable_range,
            snapshot,
            inputs,
            buffer,
        })
    });

    cx.spawn(async move |cx: &mut gpui::AsyncApp| {
        let output = result.await.context("fim edit prediction failed")?;
        anyhow::Ok(Some(
            EditPredictionResult::new(
                EditPredictionId(output.request_id.into()),
                &output.buffer,
                &output.snapshot,
                output.edits.into(),
                None,
                Some(output.editable_range),
                EditPredictionInputs::V2(output.inputs),
                None,
                trigger,
                cx.background_executor().now() - request_start,
                cx,
            )
            .await,
        ))
    })
}

/// Whether inserting `completion` at `offset` leaves the file with more syntax
/// errors than it has now.
///
/// Both texts are parsed from scratch. That is more work than reusing the
/// buffer's existing tree, and it is work done once per accepted-looking
/// completion on a background thread, against an excerpt-sized string -- next
/// to a network round trip it does not register.
///
/// Any failure to parse either version returns `false`. The gate exists to
/// remove bad completions, not to become a second way for good ones to
/// disappear.
fn worsens_syntax(
    language: &tree_sitter::Language,
    text: &str,
    offset: usize,
    completion: &str,
) -> bool {
    let Some(before) = error_count(language, text) else {
        return false;
    };

    let mut after_text = String::with_capacity(text.len() + completion.len());
    if !text.is_char_boundary(offset) {
        return false;
    }
    after_text.push_str(&text[..offset]);
    after_text.push_str(completion);
    after_text.push_str(&text[offset..]);

    let Some(after) = error_count(language, &after_text) else {
        return false;
    };

    after > before
}

/// Counts the error and missing nodes in `text`, or `None` if it cannot be
/// parsed at all.
fn error_count(language: &tree_sitter::Language, text: &str) -> Option<usize> {
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(language).ok()?;
    let tree = parser.parse(text, None)?;

    let mut count = 0;
    let mut cursor = tree.walk();
    let mut descend = true;
    loop {
        if descend {
            let node = cursor.node();
            // `is_error` is a node the parser could not place; `is_missing` is
            // one it inserted to recover. Both are the parser saying the text
            // is not what the grammar describes, and both matter here.
            if node.is_error() || node.is_missing() {
                count += 1;
            }
        }

        if descend && cursor.goto_first_child() {
            continue;
        }
        if cursor.goto_next_sibling() {
            descend = true;
            continue;
        }
        if !cursor.goto_parent() {
            break;
        }
        descend = false;
    }

    Some(count)
}

#[cfg(test)]
mod syntax_gate_tests {
    use super::*;

    fn rust() -> tree_sitter::Language {
        tree_sitter_rust::LANGUAGE.into()
    }

    #[test]
    fn a_completion_that_finishes_the_line_is_kept() {
        let text = "fn main() {\n    let x = \n}\n";
        let offset = text.find("\n}").unwrap();
        assert!(!worsens_syntax(&rust(), text, offset, "1;"));
    }

    #[test]
    fn a_completion_that_leaves_a_brace_open_is_refused() {
        let text = "fn main() {\n    \n}\n";
        let offset = text.find("\n}").unwrap();
        assert!(worsens_syntax(&rust(), text, offset, "if x {"));
    }

    #[test]
    fn a_completion_that_duplicates_a_closing_brace_is_refused() {
        let text = "fn main() {\n    let x = 1;\n}\n";
        let offset = text.find("\n}").unwrap();
        assert!(worsens_syntax(&rust(), text, offset, "\n}\n}"));
    }

    #[test]
    fn errors_already_in_the_file_are_not_blamed_on_the_completion() {
        // The stray `@#$` is broken before anything is inserted. A completion
        // that adds nothing wrong must still be allowed through.
        let text = "fn main() {\n    @#$\n    let x = \n}\n";
        let offset = text.find("\n}").unwrap();
        assert!(!worsens_syntax(&rust(), text, offset, "1;"));
    }

    #[test]
    fn an_offset_inside_a_character_is_refused_rather_than_panicking() {
        let text = "let caf\u{00e9} = 1;";
        // One byte into the two-byte `\u{00e9}`.
        let offset = text.find('\u{00e9}').unwrap() + 1;
        assert!(!worsens_syntax(&rust(), text, offset, "x"));
    }
}

/// Renders retrieved excerpts as a block a chat model can read.
///
/// Line numbers are included because they are what makes an excerpt locatable:
/// without them a model cannot tell a definition from a call site, and starts
/// treating a fragment as if it were the whole file.
///
/// Bounded, because these arrive from a retrieval store with no size contract
/// and a prompt that overflows the context window fails the whole request
/// rather than degrading.
fn format_related_files(related_files: &[zeta_prompt::RelatedFile]) -> String {
    /// Roughly a third of a 16k window, leaving the rest for the file being
    /// edited and the reply.
    const MAX_BYTES: usize = 12_000;

    let mut out = String::new();
    for file in related_files {
        for excerpt in &file.excerpts {
            let header = format!(
                "--- {} lines {}-{} ---\n",
                file.path.display(),
                excerpt.row_range.start + 1,
                excerpt.row_range.end + 1
            );
            if out.len() + header.len() + excerpt.text.len() > MAX_BYTES {
                return out;
            }
            out.push_str(&header);
            out.push_str(&excerpt.text);
            if !excerpt.text.ends_with('\n') {
                out.push('\n');
            }
            out.push('\n');
        }
    }
    out
}

fn format_fim_prompt(
    prompt_format: EditPredictionPromptFormat,
    prefix: &str,
    suffix: &str,
) -> String {
    match prompt_format {
        EditPredictionPromptFormat::CodeLlama => {
            format!("<PRE> {prefix} <SUF>{suffix} <MID>")
        }
        EditPredictionPromptFormat::StarCoder => {
            format!("<fim_prefix>{prefix}<fim_suffix>{suffix}<fim_middle>")
        }
        EditPredictionPromptFormat::DeepseekCoder => {
            format!("<｜fim▁begin｜>{prefix}<｜fim▁hole｜>{suffix}<｜fim▁end｜>")
        }
        EditPredictionPromptFormat::Qwen | EditPredictionPromptFormat::CodeGemma => {
            format!("<|fim_prefix|>{prefix}<|fim_suffix|>{suffix}<|fim_middle|>")
        }
        EditPredictionPromptFormat::Codestral => {
            format!("[SUFFIX]{suffix}[PREFIX]{prefix}")
        }
        EditPredictionPromptFormat::Glm => {
            format!("<|code_prefix|>{prefix}<|code_suffix|>{suffix}<|code_middle|>")
        }
        _ => {
            format!("<fim_prefix>{prefix}<fim_suffix>{suffix}<fim_middle>")
        }
    }
}

fn get_fim_stop_tokens() -> Vec<String> {
    vec![
        "<|endoftext|>".to_string(),
        "<|file_separator|>".to_string(),
        "<|fim_pad|>".to_string(),
        "<|fim_prefix|>".to_string(),
        "<|fim_middle|>".to_string(),
        "<|fim_suffix|>".to_string(),
        "<fim_prefix>".to_string(),
        "<fim_middle>".to_string(),
        "<fim_suffix>".to_string(),
        "<PRE>".to_string(),
        "<SUF>".to_string(),
        "<MID>".to_string(),
        "[PREFIX]".to_string(),
        "[SUFFIX]".to_string(),
    ]
}

fn clean_fim_completion(response: &str) -> String {
    let mut result = response.to_string();

    let end_tokens = [
        "<|endoftext|>",
        "<|file_separator|>",
        "<|fim_pad|>",
        "<|fim_prefix|>",
        "<|fim_middle|>",
        "<|fim_suffix|>",
        "<fim_prefix>",
        "<fim_middle>",
        "<fim_suffix>",
        "<PRE>",
        "<SUF>",
        "<MID>",
        "[PREFIX]",
        "[SUFFIX]",
    ];

    for token in &end_tokens {
        if let Some(pos) = result.find(token) {
            result.truncate(pos);
        }
    }

    result
}
