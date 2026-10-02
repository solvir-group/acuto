//! Commands as blocks, not as a wall of text.
//!
//! A terminal is a scrollback buffer: everything that ever happened, flattened
//! into one stream with no structure. You cannot ask it "what failed", "how long
//! did that take", or "show me just the output of the build" -- not because the
//! information is missing, but because it was thrown away the moment it was
//! printed.
//!
//! The shell already tells us the structure through OSC 133 (see
//! [`crate::shell_state`]). This turns that stream into records: one per command,
//! carrying what ran, where, how long it took, how it exited, and what it
//! printed. That is enough to jump between failures, rerun something, or hand a
//! failing build to a model without the user copying anything.
//!
//! # Why the output is captured rather than pointed at
//!
//! The obvious design stores a line range into scrollback and reads it back on
//! demand. It does not work: alacritty's scrollback is a ring buffer, so the
//! lines a range points at are evicted as new output arrives, and every line
//! number shifts under you when the grid scrolls. A range is a reference into a
//! buffer that is actively rewriting itself.
//!
//! Copying the text at completion costs memory, which is bounded here, and buys
//! records that stay correct for the rest of the session.

use std::{collections::VecDeque, path::PathBuf, sync::LazyLock};

use regex::Regex;

/// How much of one command's output is kept.
///
/// A `cargo build` of this repository prints a few hundred kilobytes; a runaway
/// loop prints until the disk fills. The cap is per block so one pathological
/// command cannot evict the history of every well-behaved one.
const MAX_OUTPUT_BYTES: usize = 128 * 1024;

/// How many blocks are retained.
///
/// Bounded because this lives for the life of the terminal, and an editor that
/// grows without limit while sitting idle is an editor people restart.
const MAX_BLOCKS: usize = 500;

/// When output is trimmed, how much of the head is kept.
///
/// The head carries the command echo and the first error, which is almost always
/// the real one; the tail carries the summary and exit. The middle of a long
/// build log is the least useful part, so that is what goes.
const HEAD_FRACTION: usize = 3;

/// One command, from the prompt that launched it to the exit code it returned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandBlock {
    /// Monotonic within a terminal. Used to address a block from the UI without
    /// holding a reference into the history while it is being mutated.
    pub id: BlockId,
    pub command: String,
    pub cwd: Option<PathBuf>,
    /// Unix seconds.
    pub started_at: i64,
    pub duration_ms: i64,
    /// `None` when the shell reported completion without a code, which some
    /// shells do for signals.
    pub exit_code: Option<i32>,
    /// What the command printed, redacted and capped.
    pub output: String,
    /// Whether [`Self::output`] is missing a middle section.
    pub truncated: bool,
    /// Whether anything in the original output looked like a credential.
    ///
    /// Kept as a flag rather than silently dropping the block, so a surface that
    /// sends output elsewhere can refuse, while the user can still see their own
    /// history.
    pub redacted: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BlockId(pub u64);

impl CommandBlock {
    /// Whether the command reported failure.
    ///
    /// A missing exit code is not a failure. Shells omit it in cases the user
    /// did not cause, and treating "unknown" as "failed" would light up the
    /// failure UI on commands that worked.
    pub fn failed(&self) -> bool {
        self.exit_code.is_some_and(|code| code != 0)
    }

    /// A duration worth showing, if any.
    ///
    /// Below the threshold the number is noise: everything interactive finishes
    /// in milliseconds, and labelling all of it teaches the user to ignore the
    /// label.
    pub fn slow_duration(&self) -> Option<std::time::Duration> {
        const WORTH_SHOWING_MS: i64 = 2_000;
        (self.duration_ms >= WORTH_SHOWING_MS)
            .then(|| std::time::Duration::from_millis(self.duration_ms.max(0) as u64))
    }

    /// The last `lines` lines of output.
    ///
    /// What a model needs to diagnose a failure is the end of the log, where the
    /// error summary is, not the beginning, where the toolchain banner is.
    pub fn output_tail(&self, lines: usize) -> String {
        let mut tail: Vec<&str> = self.output.lines().rev().take(lines).collect();
        tail.reverse();
        tail.join("\n")
    }
}

/// The commands a terminal has run, oldest first.
#[derive(Debug, Default)]
pub struct BlockHistory {
    blocks: VecDeque<CommandBlock>,
    next_id: u64,
}

impl BlockHistory {
    /// Records a finished command, returning the block it became.
    ///
    /// Takes the raw captured output; redaction and capping happen here so no
    /// caller can skip them by constructing a block directly.
    pub fn record(
        &mut self,
        command: String,
        cwd: Option<PathBuf>,
        started_at: i64,
        duration_ms: i64,
        exit_code: Option<i32>,
        raw_output: &str,
    ) -> BlockId {
        let (output, output_redacted) = redact_secrets(raw_output);
        // The command line holds secrets as often as its output does --
        // `export TOKEN=...`, `curl -H "Authorization: ..."`.
        let (command, command_redacted) = redact_secrets(&command);
        let redacted = output_redacted || command_redacted;
        let (output, truncated) = cap_output(output);

        let id = BlockId(self.next_id);
        self.next_id += 1;

        self.blocks.push_back(CommandBlock {
            id,
            command,
            cwd,
            started_at,
            duration_ms,
            exit_code,
            output,
            truncated,
            redacted,
        });
        while self.blocks.len() > MAX_BLOCKS {
            self.blocks.pop_front();
        }
        id
    }

    pub fn get(&self, id: BlockId) -> Option<&CommandBlock> {
        self.blocks.iter().find(|block| block.id == id)
    }

    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &CommandBlock> {
        self.blocks.iter()
    }

    pub fn len(&self) -> usize {
        self.blocks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    pub fn last(&self) -> Option<&CommandBlock> {
        self.blocks.back()
    }

    /// The most recent command that failed.
    pub fn last_failure(&self) -> Option<&CommandBlock> {
        self.blocks.iter().rev().find(|block| block.failed())
    }

    /// Every command that failed, most recent first.
    pub fn failures(&self) -> impl Iterator<Item = &CommandBlock> {
        self.blocks.iter().rev().filter(|block| block.failed())
    }
}

/// Patterns that mean "this text is a credential".
///
/// Two kinds, because either alone misses too much. The shaped patterns catch
/// tokens by their issuer's format even when nothing around them says "secret".
/// The assignment pattern catches everything else by the name it was given,
/// which is how a secret usually appears in a shell: `export FOO_TOKEN=...`.
static SECRET_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        // Issuer-shaped tokens.
        r"sk-[A-Za-z0-9_-]{16,}",
        r"nvapi-[A-Za-z0-9_-]{16,}",
        r"gh[pousr]_[A-Za-z0-9]{20,}",
        r"github_pat_[A-Za-z0-9_]{20,}",
        r"AKIA[0-9A-Z]{16}",
        r"xox[baprs]-[A-Za-z0-9-]{10,}",
        r"AIza[0-9A-Za-z_-]{30,}",
        // JSON Web Tokens: three base64url segments, the first two starting with
        // the `{"` that base64 turns into `eyJ`.
        r"eyJ[A-Za-z0-9_-]{8,}\.eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}",
        // PEM private keys, on the header alone: the body is the secret and it
        // spans lines, so the header is what marks the region.
        // The whole block, not just its first line: the header is not the
        // secret, the base64 under it is.
        r"(?s)-----BEGIN [A-Z ]*PRIVATE KEY-----.*?(?:-----END [A-Z ]*PRIVATE KEY-----|\z)",
        // Named assignments. Deliberately requires a value of some length: a
        // bare `TOKEN=` in a script is not a leak.
        r"(?i)\b[A-Za-z0-9_]*(?:api[_-]?key|secret|token|password|passwd|credential|auth)[A-Za-z0-9_]*\s*[=:]\s*\S{8,}",
    ]
    .iter()
    .filter_map(|pattern| Regex::new(pattern).ok())
    .collect()
});

/// What replaces a matched secret.
const REDACTION: &str = "[redacted]";

/// Replaces anything that looks like a credential.
///
/// Runs before the output is stored, not before it is sent somewhere, because a
/// secret that reaches memory can reach a crash dump, a session file, or a
/// support bundle. The only safe place to drop it is on the way in.
///
/// Returns whether anything was replaced. False positives here cost the user a
/// redacted word in their own scrollback; false negatives cost them a leaked
/// key, so the patterns lean towards matching.
pub fn redact_secrets(output: &str) -> (String, bool) {
    let mut text = output.to_string();
    let mut redacted = false;
    for pattern in SECRET_PATTERNS.iter() {
        if pattern.is_match(&text) {
            redacted = true;
            text = pattern.replace_all(&text, REDACTION).into_owned();
        }
    }
    (text, redacted)
}

/// Trims output to the cap, keeping the head and the tail.
///
/// Cuts on a character boundary; slicing a UTF-8 string at an arbitrary byte
/// offset panics, and terminal output is full of box-drawing and emoji.
fn cap_output(output: String) -> (String, bool) {
    if output.len() <= MAX_OUTPUT_BYTES {
        return (output, false);
    }

    let head_budget = MAX_OUTPUT_BYTES / HEAD_FRACTION;
    let tail_budget = MAX_OUTPUT_BYTES - head_budget;

    let head_end = floor_boundary(&output, head_budget);
    let tail_start = ceil_boundary(&output, output.len() - tail_budget);

    let mut trimmed = String::with_capacity(MAX_OUTPUT_BYTES + 64);
    trimmed.push_str(&output[..head_end]);
    trimmed.push_str("\n\n[… output trimmed …]\n\n");
    trimmed.push_str(&output[tail_start..]);
    (trimmed, true)
}

/// The largest character boundary at or below `index`.
fn floor_boundary(text: &str, index: usize) -> usize {
    let mut index = index.min(text.len());
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

/// The smallest character boundary at or above `index`.
fn ceil_boundary(text: &str, index: usize) -> usize {
    let mut index = index.min(text.len());
    while index < text.len() && !text.is_char_boundary(index) {
        index += 1;
    }
    index
}

#[cfg(test)]
mod tests {
    use super::*;

    fn history_with(exit: Option<i32>, output: &str) -> (BlockHistory, BlockId) {
        let mut history = BlockHistory::default();
        let id = history.record("cargo build".into(), None, 0, 0, exit, output);
        (history, id)
    }

    #[test]
    fn a_nonzero_exit_is_a_failure_and_a_missing_one_is_not() {
        let (history, id) = history_with(Some(101), "");
        assert!(history.get(id).expect("recorded").failed());

        let (history, id) = history_with(Some(0), "");
        assert!(!history.get(id).expect("recorded").failed());

        // Shells omit the code for cases the user did not cause. Treating that
        // as failure would light up the failure UI on commands that worked.
        let (history, id) = history_with(None, "");
        assert!(!history.get(id).expect("recorded").failed());
    }

    #[test]
    fn issuer_shaped_tokens_are_redacted() {
        let (text, redacted) = redact_secrets("using sk-abcdefghijklmnopqrstuvwxyz now");
        assert_eq!(text, "using [redacted] now");
        assert!(redacted);

        let (text, _) = redact_secrets("nvapi-0123456789abcdefghij");
        assert_eq!(text, "[redacted]");

        let (text, _) = redact_secrets("ghp_0123456789abcdefghijklmnopqrstuvwx");
        assert_eq!(text, "[redacted]");
    }

    #[test]
    fn named_assignments_are_redacted_whatever_the_name() {
        let (text, redacted) = redact_secrets("export MY_API_KEY=hunter2hunter2");
        assert!(redacted, "an api key assignment should be caught");
        assert!(!text.contains("hunter2"), "got: {text}");

        let (text, _) = redact_secrets("PASSWORD: correcthorsebattery");
        assert!(!text.contains("correcthorse"), "got: {text}");
    }

    #[test]
    fn ordinary_output_is_left_alone() {
        let log = "   Compiling zed v1.18.0\nerror[E0599]: no method named `ok`\n";
        let (text, redacted) = redact_secrets(log);
        assert_eq!(text, log);
        assert!(!redacted);

        // A bare name with no value is a script, not a leak.
        let (text, redacted) = redact_secrets("TOKEN=");
        assert_eq!(text, "TOKEN=");
        assert!(!redacted);
    }

    #[test]
    fn long_output_keeps_both_ends() {
        let output = format!(
            "FIRST\n{}\nLAST",
            "x".repeat(MAX_OUTPUT_BYTES * 2)
        );
        let (trimmed, truncated) = cap_output(output);
        assert!(truncated);
        assert!(trimmed.starts_with("FIRST"), "head lost");
        assert!(trimmed.ends_with("LAST"), "tail lost");
        assert!(trimmed.contains("output trimmed"));
    }

    #[test]
    fn trimming_never_splits_a_character() {
        // Every character is 4 bytes, so a byte-offset cut lands mid-character
        // unless boundaries are respected.
        let output = "🙂".repeat(MAX_OUTPUT_BYTES);
        let (trimmed, truncated) = cap_output(output);
        assert!(truncated);
        assert!(trimmed.contains('🙂'));
    }

    #[test]
    fn the_history_is_bounded_and_keeps_the_newest() {
        let mut history = BlockHistory::default();
        for index in 0..(MAX_BLOCKS + 25) {
            history.record(format!("command {index}"), None, 0, 0, Some(0), "");
        }
        assert_eq!(history.len(), MAX_BLOCKS);
        assert_eq!(
            history.last().expect("non-empty").command,
            format!("command {}", MAX_BLOCKS + 24)
        );
    }

    #[test]
    fn the_last_failure_skips_later_successes() {
        let mut history = BlockHistory::default();
        history.record("ok".into(), None, 0, 0, Some(0), "");
        history.record("bad".into(), None, 0, 0, Some(1), "");
        history.record("ok again".into(), None, 0, 0, Some(0), "");

        assert_eq!(
            history.last_failure().map(|block| block.command.as_str()),
            Some("bad")
        );
        assert_eq!(history.failures().count(), 1);
    }

    #[test]
    fn only_slow_commands_report_a_duration() {
        let mut history = BlockHistory::default();
        let quick = history.record("ls".into(), None, 0, 40, Some(0), "");
        let slow = history.record("cargo build".into(), None, 0, 9_000, Some(0), "");

        assert!(history.get(quick).expect("recorded").slow_duration().is_none());
        assert_eq!(
            history.get(slow).expect("recorded").slow_duration(),
            Some(std::time::Duration::from_millis(9_000))
        );
    }

    #[test]
    fn the_tail_is_the_end_of_the_log() {
        let mut history = BlockHistory::default();
        let id = history.record(
            "cargo build".into(),
            None,
            0,
            0,
            Some(101),
            "line one\nline two\nline three\nline four",
        );
        assert_eq!(
            history.get(id).expect("recorded").output_tail(2),
            "line three\nline four"
        );
        // Asking for more lines than exist returns what there is.
        assert_eq!(
            history.get(id).expect("recorded").output_tail(99),
            "line one\nline two\nline three\nline four"
        );
    }

    #[test]
    fn a_recorded_block_remembers_it_was_redacted() {
        let mut history = BlockHistory::default();
        let id = history.record(
            "env".into(),
            None,
            0,
            0,
            Some(0),
            "AWS_SECRET_ACCESS_KEY=abcdefghijklmnop",
        );
        let block = history.get(id).expect("recorded");
        assert!(block.redacted);
        assert!(!block.output.contains("abcdefghijklmnop"));
    }
}
