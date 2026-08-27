//! Semantic prompt tracking from OSC 133 / 633.
//!
//! Terminal completion needs one thing before anything else works: what the
//! user has typed at the current prompt. Every tool that solves this from
//! outside the terminal — Fig, inshellisense, Warp's early work — spends most
//! of its complexity shimming a pty and guessing where the prompt ends. We own
//! the emulator, so we can be told instead of guessing.
//!
//! Shells report their own structure through OSC 133:
//!
//! | Sequence | Meaning |
//! | --- | --- |
//! | `133;A` | prompt starts |
//! | `133;B` | prompt ends, user input begins |
//! | `133;C` | command is about to run |
//! | `133;D[;exit]` | command finished |
//!
//! VS Code's `633` carries the same letters, so a user who already has VS Code
//! shell integration installed gets prompt tracking with no injection of ours.
//!
//! # No heuristics
//!
//! Without shell integration the phase stays [`PromptPhase::Unknown`] and every
//! completion surface stays dark. There is deliberately no fallback prompt
//! detector: a regex over "looks like a prompt" is wrong constantly, wrong
//! unpredictably, and wrong in a way the user cannot correct. A feature that is
//! absent is a smaller problem than one that fires at the wrong moment.

use std::path::PathBuf;

/// Where the shell is in its prompt/execute cycle.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PromptPhase {
    /// No shell integration, or nothing reported yet. Completions stay off.
    #[default]
    Unknown,
    /// Between `133;B` and `133;C`: the user is typing. Completions are live.
    AtPrompt,
    /// Between `133;C` and `133;D`: a command is running. Completions are
    /// suppressed, because the keystrokes belong to the program, not a prompt.
    Executing,
}

/// A point in the terminal grid, in absolute lines from the top of scrollback.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GridPoint {
    pub line: i32,
    pub column: usize,
}

/// What the shell has told us about itself.
#[derive(Clone, Debug, Default)]
pub struct ShellState {
    phase: PromptPhase,
    /// Where the user's input begins — reported at `133;B`. Completions read
    /// the edit buffer from here to the cursor.
    command_start: Option<GridPoint>,
    cwd: Option<PathBuf>,
    last_exit_code: Option<i32>,
    alt_screen: bool,
    /// Set once any recognised marker arrives, so the editor can tell "no
    /// integration installed" from "integration installed, currently between
    /// prompts". Only the former warrants an install prompt.
    integration_seen: bool,
    /// The last non-empty line seen at the prompt.
    ///
    /// Recorded continuously while the user types rather than captured at a
    /// start-of-execution marker, because PowerShell has no dependable
    /// pre-execution hook — its only one is a PSReadLine key handler, which
    /// does not fire over a pty. By the time a command finishes its output has
    /// scrolled the line away, so the only way to know what ran is to have been
    /// watching.
    running_command: Option<String>,
    /// Unix seconds when the running command started.
    started_at: Option<i64>,
}

/// What handling an OSC produced.
#[derive(Clone, Debug, PartialEq)]
pub enum HandledOsc {
    /// State the editor cares about changed; re-render.
    Changed,
    /// A command finished and should be written to history.
    CommandFinished(FinishedCommand),
}

/// A command that finished, ready to be recorded in history.
#[derive(Clone, Debug, PartialEq)]
pub struct FinishedCommand {
    pub command: String,
    pub exit_code: Option<i32>,
    pub started_at: i64,
    pub duration_ms: i64,
    pub cwd: Option<PathBuf>,
}

impl ShellState {
    pub fn phase(&self) -> PromptPhase {
        self.phase
    }

    pub fn command_start(&self) -> Option<GridPoint> {
        self.command_start
    }

    pub fn cwd(&self) -> Option<&PathBuf> {
        self.cwd.as_ref()
    }

    pub fn last_exit_code(&self) -> Option<i32> {
        self.last_exit_code
    }

    pub fn alt_screen(&self) -> bool {
        self.alt_screen
    }

    pub fn integration_seen(&self) -> bool {
        self.integration_seen
    }

    /// Whether completions may be offered right now.
    ///
    /// The single place the guards are enforced, so a new surface cannot
    /// accidentally skip one by forgetting to check.
    pub fn completions_allowed(&self) -> bool {
        self.phase == PromptPhase::AtPrompt && !self.alt_screen
    }

    pub fn set_alt_screen(&mut self, alt_screen: bool) {
        self.alt_screen = alt_screen;
    }

    /// Remembers what is currently typed at the prompt.
    ///
    /// Blank lines are ignored so that pressing Enter at an empty prompt, or
    /// the prompt being redrawn, does not erase the command that is about to
    /// run.
    pub fn observe_prompt_line(&mut self, line: &str, now: i64) {
        if self.phase != PromptPhase::AtPrompt {
            return;
        }
        let line = line.trim();
        if line.is_empty() {
            return;
        }
        if self.running_command.as_deref() != Some(line) {
            self.running_command = Some(line.to_string());
            self.started_at = Some(now);
        }
    }

    /// Folds one passthrough OSC into the state.
    ///
    /// Returns whether anything the editor cares about changed, so callers can
    /// avoid re-rendering on the many sequences that change nothing.
    /// `typed_line` is what the grid currently shows at the prompt, supplied by
    /// the caller because the grid lives on the terminal rather than here. It is
    /// only meaningful at `133;C`, which is the last moment the command is still
    /// on screen where it was typed.
    ///
    /// `now` is Unix seconds, passed in rather than read so the whole type stays
    /// testable without a clock.
    pub fn handle_osc(
        &mut self,
        params: &[Vec<u8>],
        cursor: GridPoint,
        typed_line: Option<&str>,
        now: i64,
    ) -> Option<HandledOsc> {
        let code = params.first()?;

        match code.as_slice() {
            b"7" => self
                .handle_cwd_report(params.get(1))
                .then_some(HandledOsc::Changed),
            // 133 and 633 carry the same letters. VS Code adds sequences of its
            // own beyond them, which fall through the match below unhandled —
            // deliberately, since acting on a half-understood dialect is worse
            // than ignoring it.
            b"133" | b"633" => {
                self.handle_prompt_marker(params.get(1), params.get(2), cursor, typed_line, now)
            }
            _ => None,
        }
    }

    fn handle_prompt_marker(
        &mut self,
        kind: Option<&Vec<u8>>,
        argument: Option<&Vec<u8>>,
        cursor: GridPoint,
        typed_line: Option<&str>,
        now: i64,
    ) -> Option<HandledOsc> {
        let kind = kind?;

        match kind.as_slice() {
            b"A" => {
                self.integration_seen = true;
                self.phase = PromptPhase::Unknown;
                self.command_start = None;
                Some(HandledOsc::Changed)
            }
            b"B" => {
                self.integration_seen = true;
                self.phase = PromptPhase::AtPrompt;
                self.command_start = Some(cursor);
                Some(HandledOsc::Changed)
            }
            b"C" => {
                self.integration_seen = true;
                // A last chance to capture the line, for shells that do report
                // execution start. Nothing depends on this arriving.
                if let Some(line) = typed_line {
                    self.observe_prompt_line(line, now);
                }
                self.phase = PromptPhase::Executing;
                Some(HandledOsc::Changed)
            }
            b"D" => {
                self.integration_seen = true;
                self.phase = PromptPhase::Unknown;
                self.command_start = None;
                // A missing or unparsable exit code is recorded as absent
                // rather than as zero: "we do not know" and "it succeeded"
                // rank very differently when suggesting from history.
                self.last_exit_code = argument
                    .and_then(|code| std::str::from_utf8(code).ok())
                    .and_then(|code| code.trim().parse::<i32>().ok());

                let started_at = self.started_at.take();
                match (self.running_command.take(), started_at) {
                    (Some(command), Some(started_at)) => {
                        Some(HandledOsc::CommandFinished(FinishedCommand {
                            command,
                            exit_code: self.last_exit_code,
                            started_at,
                            duration_ms: (now - started_at).max(0) * 1000,
                            cwd: self.cwd.clone(),
                        }))
                    }
                    // Nothing was typed at the previous prompt — the first
                    // prompt of a session, or a bare Enter. The shell reports D
                    // unconditionally, so this is the common case, not an error.
                    _ => Some(HandledOsc::Changed),
                }
            }
            _ => None,
        }
    }

    fn handle_cwd_report(&mut self, target: Option<&Vec<u8>>) -> bool {
        let Some(target) = target else {
            return false;
        };
        let Ok(target) = std::str::from_utf8(target) else {
            return false;
        };

        let Some(path) = parse_file_url(target) else {
            return false;
        };

        if self.cwd.as_deref() == Some(path.as_path()) {
            return false;
        }
        self.cwd = Some(path);
        true
    }
}

/// Extracts a local path from an OSC 7 `file://host/path` report.
///
/// Returns `None` for a non-local host: the path would be meaningful on the
/// remote machine and actively misleading here, which is the same reason remote
/// sessions suppress completions entirely.
fn parse_file_url(target: &str) -> Option<PathBuf> {
    let rest = target.strip_prefix("file://")?;
    let (host, path) = match rest.find('/') {
        Some(index) => rest.split_at(index),
        // `file://` with no path component reports nothing usable.
        None => return None,
    };

    if !(host.is_empty() || host.eq_ignore_ascii_case("localhost")) {
        return None;
    }

    let decoded = percent_decode(path);

    // Windows shells report `/C:/Users/...`; the leading slash is part of the
    // URL, not the path.
    #[cfg(windows)]
    let decoded = decoded
        .strip_prefix('/')
        .filter(|rest| {
            let mut chars = rest.chars();
            matches!(
                (chars.next(), chars.next()),
                (Some(drive), Some(':')) if drive.is_ascii_alphabetic()
            )
        })
        .map(str::to_string)
        .unwrap_or(decoded);

    if decoded.is_empty() {
        return None;
    }
    Some(PathBuf::from(decoded))
}

/// Decodes `%XX` escapes. Invalid escapes are left as written rather than
/// dropped, so a malformed report degrades to a wrong-but-visible path instead
/// of a silently truncated one.
fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;

    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = &input[index + 1..index + 3];
            if let Ok(byte) = u8::from_str_radix(hex, 16) {
                out.push(byte);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }

    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn osc(parts: &[&str]) -> Vec<Vec<u8>> {
        parts.iter().map(|part| part.as_bytes().to_vec()).collect()
    }

    const CURSOR: GridPoint = GridPoint { line: 4, column: 7 };
    const NOW: i64 = 1_700_000_000;

    #[test]
    fn completions_stay_dark_without_integration() {
        let state = ShellState::default();
        assert_eq!(state.phase(), PromptPhase::Unknown);
        assert!(!state.completions_allowed());
        assert!(!state.integration_seen());
    }

    #[test]
    fn prompt_cycle_moves_through_phases() {
        let mut state = ShellState::default();

        state.handle_osc(&osc(&["133", "A"]), CURSOR, None, NOW);
        assert_eq!(state.phase(), PromptPhase::Unknown);
        assert!(state.integration_seen());

        state.handle_osc(&osc(&["133", "B"]), CURSOR, None, NOW);
        assert_eq!(state.phase(), PromptPhase::AtPrompt);
        assert_eq!(state.command_start(), Some(CURSOR));
        assert!(state.completions_allowed());

        state.handle_osc(&osc(&["133", "C"]), CURSOR, None, NOW);
        assert_eq!(state.phase(), PromptPhase::Executing);
        assert!(
            !state.completions_allowed(),
            "keystrokes during a command belong to the program"
        );

        state.handle_osc(&osc(&["133", "D", "0"]), CURSOR, None, NOW);
        assert_eq!(state.phase(), PromptPhase::Unknown);
        assert_eq!(state.last_exit_code(), Some(0));
        assert_eq!(state.command_start(), None);
    }

    #[test]
    fn vscode_dialect_is_equivalent() {
        let mut state = ShellState::default();
        state.handle_osc(&osc(&["633", "B"]), CURSOR, None, NOW);
        assert!(
            state.completions_allowed(),
            "an existing VS Code shell integration should work with no injection"
        );
    }

    #[test]
    fn missing_exit_code_is_unknown_not_success() {
        let mut state = ShellState::default();
        state.handle_osc(&osc(&["133", "D"]), CURSOR, None, NOW);
        assert_eq!(state.last_exit_code(), None);

        let mut state = ShellState::default();
        state.handle_osc(&osc(&["133", "D", "not-a-number"]), CURSOR, None, NOW);
        assert_eq!(state.last_exit_code(), None);
    }

    #[test]
    fn alt_screen_suppresses_completions() {
        let mut state = ShellState::default();
        state.handle_osc(&osc(&["133", "B"]), CURSOR, None, NOW);
        assert!(state.completions_allowed());

        state.set_alt_screen(true);
        assert!(
            !state.completions_allowed(),
            "a popup inside vim or less destroys trust permanently"
        );
    }

    #[test]
    fn cwd_reports_are_parsed() {
        let mut state = ShellState::default();

        assert!(state.handle_osc(&osc(&["7", "file:///tmp/project"]), CURSOR, None, NOW).is_some());
        assert_eq!(state.cwd(), Some(&PathBuf::from("/tmp/project")));

        // Unchanged directory is not a change.
        assert!(state.handle_osc(&osc(&["7", "file:///tmp/project"]), CURSOR, None, NOW).is_none());

        // Percent escapes.
        state.handle_osc(&osc(&["7", "file:///tmp/with%20space"]), CURSOR, None, NOW);
        assert_eq!(state.cwd(), Some(&PathBuf::from("/tmp/with space")));

        // localhost is us.
        state.handle_osc(&osc(&["7", "file://localhost/tmp/local"]), CURSOR, None, NOW);
        assert_eq!(state.cwd(), Some(&PathBuf::from("/tmp/local")));
    }

    #[test]
    fn remote_cwd_reports_are_ignored() {
        let mut state = ShellState::default();
        state.handle_osc(&osc(&["7", "file:///tmp/local"]), CURSOR, None, NOW);

        assert!(
            state.handle_osc(&osc(&["7", "file://build-server/srv/app"]), CURSOR, None, NOW).is_none(),
            "a remote path is meaningless locally and worse than none"
        );
        assert_eq!(state.cwd(), Some(&PathBuf::from("/tmp/local")));
    }

    #[cfg(windows)]
    #[test]
    fn windows_drive_letters_lose_the_url_slash() {
        let mut state = ShellState::default();
        state.handle_osc(&osc(&["7", "file:///C:/Users/dev/project"]), CURSOR, None, NOW);
        assert_eq!(state.cwd(), Some(&PathBuf::from("C:/Users/dev/project")));
    }

    #[test]
    fn a_finished_command_is_reported_once_with_its_outcome() {
        let mut state = ShellState::default();
        state.handle_osc(&osc(&["7", "file:///work/repo"]), CURSOR, None, NOW);
        state.handle_osc(&osc(&["133", "B"]), CURSOR, None, NOW);

        // The line is captured at C, while it is still on screen.
        assert_eq!(
            state.handle_osc(&osc(&["133", "C"]), CURSOR, Some("cargo build "), NOW),
            Some(HandledOsc::Changed),
        );

        let finished = state.handle_osc(&osc(&["133", "D", "0"]), CURSOR, None, NOW + 3);
        assert_eq!(
            finished,
            Some(HandledOsc::CommandFinished(FinishedCommand {
                command: "cargo build".to_string(),
                exit_code: Some(0),
                started_at: NOW,
                duration_ms: 3000,
                cwd: Some(PathBuf::from("/work/repo")),
            })),
            "the trimmed command, its outcome, its duration and where it ran"
        );

        // A second D reports nothing: the command was already taken, and
        // recording it twice would double its weight in history.
        assert_eq!(
            state.handle_osc(&osc(&["133", "D", "0"]), CURSOR, None, NOW + 4),
            Some(HandledOsc::Changed),
        );
    }

    #[test]
    fn a_prompt_with_nothing_typed_records_nothing() {
        let mut state = ShellState::default();
        state.handle_osc(&osc(&["133", "B"]), CURSOR, None, NOW);
        state.handle_osc(&osc(&["133", "C"]), CURSOR, Some("   "), NOW);

        assert_eq!(
            state.handle_osc(&osc(&["133", "D", "0"]), CURSOR, None, NOW + 1),
            Some(HandledOsc::Changed),
            "pressing Enter at an empty prompt is not a command"
        );
    }

    #[test]
    fn unknown_sequences_change_nothing() {
        let mut state = ShellState::default();
        state.handle_osc(&osc(&["133", "B"]), CURSOR, None, NOW);

        assert!(state.handle_osc(&osc(&["133", "P", "Cwd=/tmp"]), CURSOR, None, NOW).is_none());
        assert!(state.handle_osc(&osc(&["0", "some title"]), CURSOR, None, NOW).is_none());
        assert_eq!(
            state.phase(),
            PromptPhase::AtPrompt,
            "an unrecognised dialect extension must not disturb known state"
        );
    }
}
