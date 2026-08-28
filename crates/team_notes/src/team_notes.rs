//! Conversations anchored to lines of code, carried by the repository.
//!
//! This replaces the collaboration panel, which talked to a server this fork
//! does not have and so did nothing at all. The question it answers is narrower
//! and more useful: not "how do I chat with my team", which Slack does better
//! than an editor ever will, but "how do I ask about *this line* in a way my
//! teammate will see when they are looking at it".
//!
//! # Why the repository is the transport
//!
//! A fork with no server has exactly one channel that every teammate is already
//! connected to, already authenticated against, and already synchronises
//! deliberately rather than in the background: the git repository. Notes stored
//! in it arrive with `git pull`, survive going offline, review as part of a
//! diff, and need no account. Nothing about them can break because a service is
//! down, because there is no service.
//!
//! The cost is that notes move at the speed of pushes rather than keystrokes.
//! That is the right trade for the thing being built -- a question about a line
//! of code is not a chat message, and answering it hours later in the same
//! place is fine.
//!
//! # Why JSONL
//!
//! One thread per line, sorted by id. A pretty-printed JSON array conflicts in
//! git the moment two people add a thread, because both edits land on the same
//! closing bracket. Line-delimited records with stable ordering merge cleanly:
//! two people adding threads touch different lines, and git resolves it without
//! anyone being asked. A file format that generates merge conflicts is a file
//! format nobody will keep using.

pub mod panel;

pub use panel::{AddNote, TeamNotesPanel, ToggleFocus};

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

/// Where notes live inside a worktree.
///
/// Under a dot-directory so it is out of the way, and with a name that says
/// which tool wrote it so nobody has to guess before deleting it.
pub const NOTES_PATH: &str = ".acuto/notes.jsonl";

/// How far from its recorded line a note will look for its anchor text.
///
/// Wide enough to survive a function moving within a file, narrow enough that a
/// common line like `}` does not match somewhere unrelated and drag the note
/// across the file with it.
const ANCHOR_SEARCH_RADIUS: u32 = 60;

/// A single message in a thread.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    /// Display name of whoever wrote it, from `git config user.name`.
    pub author: String,
    /// RFC 3339, so the file is readable and sorts correctly as text.
    pub at: String,
    pub body: String,
}

/// Where a thread is attached.
///
/// Both a line number and the text that was on it. The line number is where to
/// look first; the text is what makes the note findable after the line has
/// moved. Storing only one of them gives a note that is either always right for
/// a file nobody edited, or always vague.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Anchor {
    /// Zero-based, as the editor counts them.
    pub line: u32,
    /// The line's text when the note was written, trimmed of surrounding
    /// whitespace so re-indentation does not orphan the note.
    pub text: String,
}

/// A conversation about one place in one file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NoteThread {
    /// Sortable and unique. Sortable matters: the file is kept in id order so
    /// two people writing notes produce a diff git can merge.
    pub id: String,
    /// Worktree-relative, with forward slashes on every platform so a note
    /// written on Windows resolves on Linux.
    pub file: String,
    pub anchor: Anchor,
    #[serde(default)]
    pub resolved: bool,
    pub messages: Vec<Message>,
}

impl NoteThread {
    pub fn title(&self) -> &str {
        self.messages
            .first()
            .map(|message| message.body.as_str())
            .unwrap_or_default()
    }
}

/// Where a thread's anchor resolved to in the current text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resolution {
    /// The recorded line still holds the recorded text.
    Exact(u32),
    /// The text was found elsewhere; the note has followed it.
    Moved(u32),
    /// The text is gone. The note is shown at its last known line and marked,
    /// rather than hidden -- a question about deleted code is often the most
    /// interesting one in the file.
    Drifted(u32),
}

impl Resolution {
    pub fn line(self) -> u32 {
        match self {
            Self::Exact(line) | Self::Moved(line) | Self::Drifted(line) => line,
        }
    }

    pub fn has_drifted(self) -> bool {
        matches!(self, Self::Drifted(_))
    }
}

/// Finds where a thread's anchor sits in `lines` now.
///
/// Nearest match wins, searched outward from the recorded line, so a note on
/// one of several identical lines stays with the one it was written about
/// rather than jumping to the first in the file.
pub fn resolve_anchor(anchor: &Anchor, lines: &[&str]) -> Resolution {
    if lines.is_empty() {
        return Resolution::Drifted(anchor.line);
    }

    let needle = anchor.text.trim();
    let matches_at = |row: u32| -> bool {
        lines
            .get(row as usize)
            .is_some_and(|line| line.trim() == needle)
    };

    // An empty anchor cannot be matched by text, so the recorded line is all
    // there is to go on.
    if needle.is_empty() {
        let last = lines.len() as u32 - 1;
        return if anchor.line <= last {
            Resolution::Exact(anchor.line)
        } else {
            Resolution::Drifted(last)
        };
    }

    if matches_at(anchor.line) {
        return Resolution::Exact(anchor.line);
    }

    for distance in 1..=ANCHOR_SEARCH_RADIUS {
        // Downward first: code more often gains lines above it than below.
        let below = anchor.line.saturating_add(distance);
        if below != anchor.line && matches_at(below) {
            return Resolution::Moved(below);
        }
        if let Some(above) = anchor.line.checked_sub(distance)
            && matches_at(above)
        {
            return Resolution::Moved(above);
        }
    }

    Resolution::Drifted(anchor.line.min(lines.len() as u32 - 1))
}

/// Parses a notes file.
///
/// A line that does not parse is skipped rather than failing the file. These
/// arrive through git merges, and one badly resolved conflict must not take
/// every other note in the repository with it.
pub fn parse(contents: &str) -> Vec<NoteThread> {
    let mut threads: Vec<NoteThread> = contents
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| match serde_json::from_str::<NoteThread>(line) {
            Ok(thread) => Some(thread),
            Err(error) => {
                log::warn!("skipping unreadable team note: {error}");
                None
            }
        })
        .collect();

    // Later wins for a duplicated id, which is what a merge that kept both
    // sides of an edited thread produces.
    let mut by_id: BTreeMap<String, NoteThread> = BTreeMap::new();
    for thread in threads.drain(..) {
        by_id.insert(thread.id.clone(), thread);
    }
    by_id.into_values().collect()
}

/// Renders threads back to the file format.
///
/// Sorted by id, one per line, with a trailing newline: all three exist so that
/// writing the same set of threads twice produces the same bytes, and so a diff
/// shows only what changed.
pub fn render(threads: &[NoteThread]) -> Result<String> {
    let mut sorted: Vec<&NoteThread> = threads.iter().collect();
    sorted.sort_by(|a, b| a.id.cmp(&b.id));

    let mut out = String::new();
    for thread in sorted {
        out.push_str(&serde_json::to_string(thread).context("serializing a team note")?);
        out.push('\n');
    }
    Ok(out)
}

/// A worktree-relative path in the form the file records.
///
/// Forward slashes always, because a note written on Windows has to be findable
/// on Linux and vice versa -- the file travels between them by definition.
pub fn normalize_path(path: &Path) -> String {
    path.components()
        .filter_map(|component| component.as_os_str().to_str())
        .collect::<Vec<_>>()
        .join("/")
}

/// The absolute path of the notes file for a worktree root.
pub fn notes_file(worktree_root: &Path) -> PathBuf {
    worktree_root.join(".acuto").join("notes.jsonl")
}

/// A time-ordered, unique id.
///
/// Time-ordered so the file's sort order is roughly chronological, which makes
/// diffs read like a log rather than a shuffle.
pub fn new_thread_id() -> String {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis())
        .unwrap_or_default();
    format!("{millis:013x}-{}", uuid::Uuid::new_v4().simple())
}

/// RFC 3339, in UTC.
pub fn now_timestamp() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// Who to attribute a note to.
///
/// `git config user.name` first, because that is the name their commits already
/// carry and the one a teammate will recognise. The OS user name is a fallback
/// for a machine with no git identity, and "unknown" is better than refusing to
/// write the note.
pub async fn author_name(executor: &gpui::BackgroundExecutor) -> Arc<str> {
    let from_git = executor
        .spawn(async {
            let output = std::process::Command::new("git")
                .args(["config", "user.name"])
                .output()
                .ok()?;
            if !output.status.success() {
                return None;
            }
            let name = String::from_utf8_lossy(&output.stdout).trim().to_string();
            (!name.is_empty()).then_some(name)
        })
        .await;

    if let Some(name) = from_git {
        return name.into();
    }

    std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_else(|_| "unknown".to_string())
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn thread(id: &str, line: u32, text: &str) -> NoteThread {
        NoteThread {
            id: id.to_string(),
            file: "src/main.rs".to_string(),
            anchor: Anchor {
                line,
                text: text.to_string(),
            },
            resolved: false,
            messages: vec![Message {
                author: "drew".into(),
                at: "2026-08-28T00:00:00Z".into(),
                body: "why?".into(),
            }],
        }
    }

    #[test]
    fn an_unmoved_line_resolves_exactly() {
        let lines = ["fn main() {", "    let x = 1;", "}"];
        assert_eq!(
            resolve_anchor(&thread("a", 1, "let x = 1;").anchor, &lines),
            Resolution::Exact(1)
        );
    }

    #[test]
    fn a_note_follows_its_line() {
        let lines = ["// added", "// added", "fn main() {", "    let x = 1;", "}"];
        // Recorded at line 1, the text is now at line 3.
        assert_eq!(
            resolve_anchor(&thread("a", 1, "let x = 1;").anchor, &lines),
            Resolution::Moved(3)
        );
    }

    #[test]
    fn the_nearest_of_several_identical_lines_wins() {
        let lines = ["}", "a", "}", "b", "}"];
        // Recorded at 2. Both 0 and 4 are the same distance, and the search
        // looks down first, so 4 is the answer -- what matters is that it is
        // one of the near ones and never 0 by accident of being first.
        assert_eq!(
            resolve_anchor(&thread("a", 2, "}").anchor, &lines),
            Resolution::Exact(2)
        );
        assert_eq!(
            resolve_anchor(&thread("a", 3, "}").anchor, &lines),
            Resolution::Moved(4)
        );
    }

    #[test]
    fn deleted_code_keeps_its_note_rather_than_losing_it() {
        let lines = ["fn main() {", "}"];
        let resolution = resolve_anchor(&thread("a", 1, "let x = 1;").anchor, &lines);
        assert!(resolution.has_drifted());
        // Clamped into the file, so jumping to it cannot land past the end.
        assert!(resolution.line() < lines.len() as u32);
    }

    #[test]
    fn re_indentation_does_not_orphan_a_note() {
        let lines = ["fn main() {", "        let x = 1;", "}"];
        assert_eq!(
            resolve_anchor(&thread("a", 1, "    let x = 1;").anchor, &lines),
            Resolution::Exact(1)
        );
    }

    #[test]
    fn the_file_round_trips_and_is_stable() {
        let threads = vec![thread("b", 1, "x"), thread("a", 2, "y")];
        let rendered = render(&threads).unwrap();

        // Sorted by id, so the same set always produces the same bytes.
        assert!(rendered.starts_with("{\"id\":\"a\""));
        assert_eq!(render(&parse(&rendered)).unwrap(), rendered);
        assert_eq!(parse(&rendered).len(), 2);
    }

    #[test]
    fn one_unreadable_line_does_not_discard_the_rest() {
        let good = serde_json::to_string(&thread("a", 0, "x")).unwrap();
        let contents = format!("{good}\n<<<<<<< HEAD\nnot json at all\n");
        assert_eq!(parse(&contents).len(), 1);
    }

    #[test]
    fn a_duplicated_id_keeps_one_copy() {
        let first = serde_json::to_string(&thread("a", 0, "x")).unwrap();
        let second = serde_json::to_string(&thread("a", 9, "y")).unwrap();
        let parsed = parse(&format!("{first}\n{second}\n"));
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].anchor.line, 9);
    }

    #[test]
    fn paths_are_recorded_with_forward_slashes() {
        assert_eq!(
            normalize_path(Path::new("src").join("nested").join("file.rs").as_path()),
            "src/nested/file.rs"
        );
    }
}
