//! Suggestions while typing in the team chat: `#` for tickets, `@` for agents
//! and teammates.
//!
//! A `#ref` names a ticket in a sentence without leaving the chat, and an
//! `@agent` hands the message to that agent. Both only work if you can find
//! the name, so both are offered as you type rather than left to memory.

use anyhow::Result;
use editor::{CompletionProvider, Editor};
use gpui::{Context, Entity, Task, WeakEntity, Window};
use language::{Buffer, CodeLabel, ToOffset};
use project::{Completion, CompletionDisplayOptions, CompletionResponse, CompletionSource};

use crate::{AGENT_MENTIONS, Kind, NoteThread, panel::TeamNotesPanel, short_ref};

/// How many suggestions the menu shows at once.
const MAX_SUGGESTIONS: usize = 30;

pub(crate) struct ChatCompletionProvider {
    panel: WeakEntity<TeamNotesPanel>,
}

impl ChatCompletionProvider {
    pub(crate) fn new(panel: WeakEntity<TeamNotesPanel>) -> Self {
        Self { panel }
    }
}

/// What the caret is in the middle of typing: the trigger character, where
/// the trigger starts, and what has been typed after it.
///
/// Only a trigger that starts a word counts, so an email address or `C#` does
/// not open the menu.
fn token_before(buffer: &Buffer, position: language::Anchor) -> Option<(char, usize, String)> {
    let mut offset = position.to_offset(buffer);
    let mut typed = Vec::new();
    for character in buffer.reversed_chars_at(position) {
        if character == '@' || character == '#' {
            let start = offset - character.len_utf8();
            let starts_word = buffer
                .reversed_chars_at(start)
                .next()
                .is_none_or(|before| before.is_whitespace() || before == '(');
            return starts_word.then(|| (character, start, typed.into_iter().rev().collect()));
        }
        if character.is_alphanumeric() || character == '-' || character == '_' {
            typed.push(character);
            offset -= character.len_utf8();
        } else {
            return None;
        }
    }
    None
}

fn suggestion(
    replace_range: std::ops::Range<language::Anchor>,
    label: String,
    new_text: String,
) -> Completion {
    Completion {
        replace_range,
        label: CodeLabel::plain(label, None),
        new_text,
        documentation: None,
        source: CompletionSource::Custom,
        icon_path: None,
        icon_color: None,
        match_start: None,
        snippet_deduplication_key: None,
        insert_text_mode: None,
        confirm: None,
        group: None,
    }
}

fn ticket_matches(ticket: &NoteThread, query: &str) -> bool {
    query.is_empty()
        || short_ref(&ticket.id).starts_with(query)
        || ticket.headline().to_lowercase().contains(query)
}

impl CompletionProvider for ChatCompletionProvider {
    fn completions(
        &self,
        buffer: &Entity<Buffer>,
        buffer_position: language::Anchor,
        _trigger: editor::CompletionContext,
        _window: &mut Window,
        cx: &mut Context<Editor>,
    ) -> Task<Result<Vec<CompletionResponse>>> {
        let buffer = buffer.read(cx);
        let Some((trigger, start, typed)) = token_before(buffer, buffer_position) else {
            return Task::ready(Ok(Vec::new()));
        };
        let Some(panel) = self.panel.upgrade() else {
            return Task::ready(Ok(Vec::new()));
        };
        let panel = panel.read(cx);
        let replace_range = buffer.anchor_before(start)..buffer_position;
        let query = typed.to_lowercase();

        let mut completions = Vec::new();
        match trigger {
            '#' => {
                // Open tickets first, newest first; finished ones after them,
                // since "the ticket we closed yesterday" is still worth naming.
                let mut tickets: Vec<&NoteThread> = panel
                    .records()
                    .iter()
                    .filter(|record| record.kind == Kind::Ticket)
                    .filter(|ticket| ticket_matches(ticket, &query))
                    .collect();
                tickets.sort_by(|a, b| {
                    a.is_closed()
                        .cmp(&b.is_closed())
                        .then_with(|| b.id.cmp(&a.id))
                });
                for ticket in tickets.into_iter().take(MAX_SUGGESTIONS) {
                    let reference = short_ref(&ticket.id);
                    completions.push(suggestion(
                        replace_range.clone(),
                        format!(
                            "#{reference}  {}  · {}",
                            ticket.headline(),
                            ticket.status.label()
                        ),
                        format!("#{reference} "),
                    ));
                }
            }
            _ => {
                for (name, _) in AGENT_MENTIONS {
                    if name.starts_with(&query) {
                        completions.push(suggestion(
                            replace_range.clone(),
                            format!("@{name}  · agent"),
                            format!("@{name} "),
                        ));
                    }
                }
                for member in panel.team() {
                    if member.to_lowercase().starts_with(&query) {
                        completions.push(suggestion(
                            replace_range.clone(),
                            format!("@{member}"),
                            format!("@{member} "),
                        ));
                    }
                }
                completions.truncate(MAX_SUGGESTIONS);
            }
        }

        Task::ready(Ok(vec![CompletionResponse {
            completions,
            display_options: CompletionDisplayOptions {
                dynamic_width: true,
            },
            // Asked again on every keystroke, so the list narrows as you type
            // and closes when the word ends.
            is_incomplete: true,
        }]))
    }

    fn is_completion_trigger(
        &self,
        buffer: &Entity<Buffer>,
        position: language::Anchor,
        text: &str,
        _trigger_in_words: bool,
        cx: &mut Context<Editor>,
    ) -> bool {
        if text == "@" || text == "#" {
            return true;
        }
        text.chars()
            .last()
            .is_some_and(|character| character.is_alphanumeric())
            && token_before(buffer.read(cx), position).is_some()
    }

    fn sort_completions(&self) -> bool {
        false
    }

    fn filter_completions(&self) -> bool {
        false
    }
}
