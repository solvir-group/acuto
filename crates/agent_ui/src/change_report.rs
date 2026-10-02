//! Whether the agent's change actually holds up.
//!
//! The bottleneck in agent-assisted work is no longer writing the code, it is
//! trusting it. The standard answer -- read every line the model wrote -- gives
//! back most of the time the model saved, which is why people say AI editing
//! feels fast and isn't.
//!
//! The editor is in a position to answer the question directly. Language servers
//! are already analysing every open buffer continuously, so the moment the agent
//! stops there is a fresh, authoritative opinion available on whether what it
//! wrote is valid. Nobody has to run anything, and nobody has to wait for a
//! build.
//!
//! # Why errors inside hunks, and not a project-wide count
//!
//! A project-wide error count is the obvious design and it is wrong in both
//! directions. It blames the agent for problems that were there before it
//! started, and -- worse -- it goes quiet when the agent breaks a file that was
//! already broken.
//!
//! What is asked here instead is narrower and actually answerable: of the ranges
//! this agent edited, how many now contain errors? That attributes correctly
//! without needing a before-and-after snapshot, and it is the question a
//! reviewer is really asking when they read a diff.

use std::time::Duration;

use action_log::ActionLog;
use buffer_diff::BufferDiff;
use gpui::{App, Entity, Task, WeakEntity};
use language::{Buffer, DiagnosticSeverity, OffsetRangeExt as _};
use util::ResultExt as _;
use workspace::Workspace;

/// How long to let the language servers catch up before asking them.
///
/// Diagnostics arrive asynchronously: at the instant the agent stops, the
/// servers have not yet seen its last edit, so reading immediately reports the
/// state before the change. Long enough for rust-analyzer to turn an edit around
/// on a warm project, short enough that the answer still feels like part of the
/// agent finishing.
const SETTLE: Duration = Duration::from_millis(2_500);

/// How many files to name in the message before summarising.
const NAMED_FILES: usize = 3;

/// What the language servers make of the agent's edits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// The agent edited files and nothing it wrote carries an error.
    Clean { files: usize },
    /// Errors fall inside ranges the agent edited.
    Broken { errors: usize, files: Vec<String> },
}

impl Verdict {
    /// From per-file error counts over the files the agent changed.
    ///
    /// Split out from everything that touches the app so the rule -- which
    /// counts, which files get named, when it is clean -- is testable without a
    /// project, a language server, or a running agent.
    pub fn from_counts(counts: &[(String, usize)]) -> Option<Self> {
        if counts.is_empty() {
            // The agent answered without editing anything. There is nothing to
            // have an opinion about, and saying "clean" would imply there was.
            return None;
        }

        let errors: usize = counts.iter().map(|(_, count)| count).sum();
        if errors == 0 {
            return Some(Self::Clean {
                files: counts.len(),
            });
        }

        let mut broken: Vec<(String, usize)> = counts
            .iter()
            .filter(|(_, count)| *count > 0)
            .cloned()
            .collect();
        // Worst first, then by name so the message is stable between runs on
        // the same set of problems.
        broken.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));

        Some(Self::Broken {
            errors,
            files: broken.into_iter().map(|(path, _)| path).collect(),
        })
    }

    /// One line, for a toast.
    pub fn message(&self) -> String {
        match self {
            Self::Clean { files } => {
                let plural = if *files == 1 { "file" } else { "files" };
                format!("Agent change verified: no errors in the {files} edited {plural}.")
            }
            Self::Broken { errors, files } => {
                let plural = if *errors == 1 { "error" } else { "errors" };
                let named = files
                    .iter()
                    .take(NAMED_FILES)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ");
                let rest = files.len().saturating_sub(NAMED_FILES);
                if rest > 0 {
                    format!("Agent change has {errors} {plural} in {named} and {rest} more.")
                } else {
                    format!("Agent change has {errors} {plural} in {named}.")
                }
            }
        }
    }
}

/// Reports on the agent's edits once the language servers have caught up.
///
/// Returns a task the caller keeps: dropping it cancels the report, which is
/// what should happen when the agent starts another turn before this one has
/// been judged.
pub fn report(
    action_log: Entity<ActionLog>,
    workspace: WeakEntity<Workspace>,
    cx: &mut App,
) -> Task<()> {
    cx.spawn(async move |cx| {
        cx.background_executor().timer(SETTLE).await;

        let counts = cx.update(|cx| error_counts(&action_log, cx));

        let Some(verdict) = Verdict::from_counts(&counts) else {
            return;
        };
        // Only a verdict worth interrupting for. A clean turn used to raise a
        // toast of its own, which meant a notification after every turn that
        // edited anything -- saying, each time, that there was nothing to say.
        // The review panel already shows what changed.
        if !matches!(verdict, Verdict::Broken { .. }) {
            return;
        }

        workspace
            .update(cx, |workspace, cx| {
                workspace.show_toast(
                    workspace::Toast::new(
                        workspace::notifications::NotificationId::unique::<ChangeReportToast>(),
                        verdict.message(),
                    ),
                    cx,
                );
            })
            .log_err();
    })
}

/// Marker for the toast's identity, so a new verdict replaces the last one
/// rather than stacking beneath it.
struct ChangeReportToast;

/// Errors inside the ranges the agent edited, per file.
fn error_counts(action_log: &Entity<ActionLog>, cx: &App) -> Vec<(String, usize)> {
    action_log
        .read(cx)
        .changed_buffers(cx)
        .map(|(buffer, diff)| {
            let name = buffer_name(&buffer, cx);
            (name, errors_in_hunks(&buffer, &diff, cx))
        })
        .collect()
}

fn buffer_name(buffer: &Entity<Buffer>, cx: &App) -> String {
    buffer
        .read(cx)
        .file()
        .map(|file| file.path().as_std_path().to_string_lossy().into_owned())
        .unwrap_or_else(|| "untitled".to_string())
}

/// Counts errors that land inside a hunk the agent produced.
///
/// Intersecting the diff with the diagnostics is what makes this attributable:
/// a pre-existing error elsewhere in the file is not the agent's, and is not
/// counted.
fn errors_in_hunks(buffer: &Entity<Buffer>, diff: &Entity<BufferDiff>, cx: &App) -> usize {
    let snapshot = buffer.read(cx).snapshot();
    // `hunks` lives on the snapshot, not the entity: the diff is recomputed in
    // the background, and iterating the live one would be reading a structure
    // that can change under the loop.
    let diff = diff.read(cx).snapshot(cx);

    // Counted once each: review splits a change into a hunk per line, and an
    // error spanning several changed lines was counted on every one of them.
    let mut errors = std::collections::HashSet::new();
    for hunk in diff.hunks(&snapshot.text) {
        let mut range = hunk.buffer_range.to_offset(&snapshot);
        if range.is_empty() {
            // A deletion has no text of its own; the line it left behind is
            // where an error it caused shows up.
            let row = snapshot.offset_to_point(range.start).row;
            range = snapshot.point_to_offset(language::Point::new(row, 0))
                ..snapshot.point_to_offset(language::Point::new(row, snapshot.line_len(row)));
        }
        for entry in snapshot.diagnostics_in_range::<_, usize>(range, false) {
            if entry.diagnostic.severity == DiagnosticSeverity::ERROR {
                errors.insert((entry.range.start, entry.range.end));
            }
        }
    }
    errors.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_edits_means_no_opinion() {
        // The agent answered a question without touching anything. Reporting
        // "clean" here would claim it had verified work that does not exist.
        assert_eq!(Verdict::from_counts(&[]), None);
    }

    #[test]
    fn edits_with_no_errors_are_clean() {
        let counts = vec![("src/main.rs".to_string(), 0), ("src/lib.rs".to_string(), 0)];
        assert_eq!(
            Verdict::from_counts(&counts),
            Some(Verdict::Clean { files: 2 })
        );
        assert_eq!(
            Verdict::from_counts(&counts).expect("clean").message(),
            "Agent change verified: no errors in the 2 edited files."
        );
    }

    #[test]
    fn one_file_reads_as_singular() {
        let counts = vec![("src/main.rs".to_string(), 0)];
        assert_eq!(
            Verdict::from_counts(&counts).expect("clean").message(),
            "Agent change verified: no errors in the 1 edited file."
        );
    }

    #[test]
    fn broken_files_are_named_worst_first() {
        let counts = vec![
            ("a.rs".to_string(), 1),
            ("b.rs".to_string(), 4),
            ("clean.rs".to_string(), 0),
        ];
        let verdict = Verdict::from_counts(&counts).expect("a verdict");
        assert_eq!(
            verdict,
            Verdict::Broken {
                errors: 5,
                files: vec!["b.rs".to_string(), "a.rs".to_string()],
            },
            "files with no errors must not be named"
        );
        assert_eq!(verdict.message(), "Agent change has 5 errors in b.rs, a.rs.");
    }

    #[test]
    fn a_long_list_is_summarised() {
        let counts: Vec<(String, usize)> = (0..6)
            .map(|index| (format!("file{index}.rs"), 1))
            .collect();
        let verdict = Verdict::from_counts(&counts).expect("a verdict");
        let message = verdict.message();
        assert!(message.starts_with("Agent change has 6 errors in"), "{message}");
        assert!(message.ends_with("and 3 more."), "{message}");
    }

    #[test]
    fn a_single_error_reads_as_singular() {
        let counts = vec![("a.rs".to_string(), 1)];
        assert_eq!(
            Verdict::from_counts(&counts).expect("a verdict").message(),
            "Agent change has 1 error in a.rs."
        );
    }

    #[test]
    fn ties_are_broken_by_name_so_the_message_is_stable() {
        let counts = vec![("z.rs".to_string(), 2), ("a.rs".to_string(), 2)];
        let Some(Verdict::Broken { files, .. }) = Verdict::from_counts(&counts) else {
            panic!("expected a broken verdict");
        };
        assert_eq!(files, vec!["a.rs".to_string(), "z.rs".to_string()]);
    }
}
