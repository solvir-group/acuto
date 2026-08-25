//! Fish-style command suggestions for the terminal.
//!
//! The terminal is the one surface in the editor with no completion at all: you
//! retype the same commands, and long paths, from memory. Shells vary in what
//! they offer and Windows PowerShell offers little, so this sits in the editor
//! rather than depending on shell support.
//!
//! # Why the typed line is tracked rather than read
//!
//! Nothing reports the shell's current input line back to us — the PTY carries
//! rendered bytes, not editor state. So the line is reconstructed from what the
//! user types, which is exact for ordinary typing and wrong the moment anything
//! else edits the line: the shell's own tab-completion, history recall with the
//! arrow keys, a multi-line continuation, a program that takes over the screen.
//!
//! Rather than suggest from a line that might be stale, tracking is abandoned
//! the moment a keystroke arrives that this cannot model, and stays abandoned
//! until the next `Enter` gives a known-empty line to start from. A missing
//! suggestion costs nothing; a confidently wrong one costs a mistyped command.

use std::{collections::VecDeque, path::PathBuf};

/// Commands remembered per terminal. Enough to cover a working session without
/// letting a long-lived terminal grow without bound.
const MAX_HISTORY: usize = 200;

/// Below this, almost everything matches and the suggestion is noise rather
/// than help.
const MIN_PREFIX_LEN: usize = 2;

/// Filesystem entries considered when completing a path fragment. A directory
/// with thousands of entries should not stall the keystroke that triggered it.
const MAX_DIRECTORY_SCAN: usize = 500;

#[derive(Default)]
pub struct CommandSuggester {
    /// The line as typed since the last `Enter`, or `None` when tracking has
    /// been abandoned because something happened that cannot be modelled.
    typed: Option<String>,
    history: VecDeque<String>,
    working_directory: Option<PathBuf>,
    suggestion: Option<String>,
}

impl CommandSuggester {
    pub fn set_working_directory(&mut self, directory: Option<PathBuf>) {
        if self.working_directory != directory {
            self.working_directory = directory;
            self.refresh();
        }
    }

    /// The remainder that would be appended if the suggestion were accepted.
    pub fn completion(&self) -> Option<&str> {
        let typed = self.typed.as_deref()?;
        let suggestion = self.suggestion.as_deref()?;
        suggestion.strip_prefix(typed).filter(|rest| !rest.is_empty())
    }

    /// The whole suggested command, for display.
    pub fn suggestion(&self) -> Option<&str> {
        self.completion().and(self.suggestion.as_deref())
    }

    /// Consumes the current suggestion, returning the text to send to the PTY.
    pub fn accept(&mut self) -> Option<String> {
        let completion = self.completion()?.to_string();
        if let Some(typed) = self.typed.as_mut() {
            typed.push_str(&completion);
        }
        self.refresh();
        Some(completion)
    }

    /// Folds one keystroke into the tracked line.
    ///
    /// Returns whether the suggestion changed, so the caller only re-renders
    /// when there is something new to show.
    pub fn observe(&mut self, key: &str, has_modifiers: bool) -> bool {
        let before = self.suggestion.clone();

        match key {
            "enter" => {
                if let Some(line) = self.typed.take() {
                    self.remember(line);
                }
                // A submitted line leaves the shell at a fresh prompt, which is
                // the one moment the tracked line is known to be empty — so it
                // is also the only place abandoned tracking resumes.
                self.typed = Some(String::new());
                self.suggestion = None;
            }
            "backspace" if !has_modifiers => {
                if let Some(typed) = self.typed.as_mut() {
                    typed.pop();
                }
                self.refresh();
            }
            "space" if !has_modifiers => {
                if let Some(typed) = self.typed.as_mut() {
                    typed.push(' ');
                }
                self.refresh();
            }
            _ => {
                let is_plain_character =
                    !has_modifiers && key.chars().count() == 1 && !key.starts_with('\u{1b}');
                if is_plain_character {
                    if let Some(typed) = self.typed.as_mut()
                        && let Some(character) = key.chars().next()
                    {
                        typed.push(character);
                    }
                    self.refresh();
                } else {
                    // Anything else — arrows, tab, ctrl chords — may have moved
                    // or rewritten the line in ways this cannot see.
                    self.typed = None;
                    self.suggestion = None;
                }
            }
        }

        before != self.suggestion
    }

    fn remember(&mut self, line: String) {
        let line = line.trim().to_string();
        if line.is_empty() {
            return;
        }
        // Re-running a command moves it to the front rather than duplicating
        // it, so the most recent form of a command is the one suggested.
        if let Some(existing) = self.history.iter().position(|entry| entry == &line) {
            self.history.remove(existing);
        }
        self.history.push_front(line);
        while self.history.len() > MAX_HISTORY {
            self.history.pop_back();
        }
    }

    fn refresh(&mut self) {
        self.suggestion = self.compute();
    }

    fn compute(&self) -> Option<String> {
        let typed = self.typed.as_deref()?;
        if typed.trim().len() < MIN_PREFIX_LEN {
            return None;
        }

        // History first: a command the user has actually run here beats a
        // filename that merely shares a prefix.
        if let Some(entry) = self
            .history
            .iter()
            .find(|entry| entry.starts_with(typed) && entry.as_str() != typed)
        {
            return Some(entry.clone());
        }

        self.complete_path(typed)
    }

    /// Completes the final whitespace-separated token as a path under the
    /// terminal's working directory.
    fn complete_path(&self, typed: &str) -> Option<String> {
        let directory = self.working_directory.as_deref()?;

        let token_start = typed.rfind(char::is_whitespace).map_or(0, |index| index + 1);
        let token = typed.get(token_start..)?;
        if token.len() < MIN_PREFIX_LEN {
            return None;
        }

        // Split the token so that `src/comp` searches `src` for `comp` rather
        // than searching the working directory for the whole thing.
        let (relative_directory, fragment) = match token.rfind(['/', '\\']) {
            Some(index) => (token.get(..index)?, token.get(index + 1..)?),
            None => ("", token),
        };
        if fragment.is_empty() {
            return None;
        }

        let search_root = if relative_directory.is_empty() {
            directory.to_path_buf()
        } else {
            directory.join(relative_directory)
        };

        let entries = std::fs::read_dir(&search_root).ok()?;
        let mut best: Option<String> = None;

        for entry in entries.take(MAX_DIRECTORY_SCAN).flatten() {
            let name = entry.file_name();
            let name = name.to_str()?;
            if !name.starts_with(fragment) || name == fragment {
                continue;
            }
            // Shortest match wins: it is the least presumptuous completion, and
            // continuing to type narrows towards the longer ones.
            if best.as_ref().is_none_or(|current| name.len() < current.len()) {
                let mut completed = name.to_string();
                if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                    completed.push(std::path::MAIN_SEPARATOR);
                }
                best = Some(completed);
            }
        }

        let completed_name = best?;
        let prefix = typed.get(..token_start)?;
        Some(if relative_directory.is_empty() {
            format!("{prefix}{completed_name}")
        } else {
            let separator = token.get(relative_directory.len()..relative_directory.len() + 1)?;
            format!("{prefix}{relative_directory}{separator}{completed_name}")
        })
    }
}

/// Formats a suggestion for display, shortening a long one from the left so the
/// part being completed stays visible.
pub fn display_suggestion(suggestion: &str, max_len: usize) -> String {
    if suggestion.chars().count() <= max_len {
        return suggestion.to_string();
    }
    let tail: String = suggestion
        .chars()
        .skip(suggestion.chars().count().saturating_sub(max_len.saturating_sub(1)))
        .collect();
    format!("…{tail}")
}
