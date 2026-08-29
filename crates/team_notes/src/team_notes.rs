//! Team communication that lives in the repository.
//!
//! This replaces the collaboration panel, which talked to a server this fork
//! does not have and so did nothing at all. It answers two questions a team
//! actually has inside an editor, and deliberately not a third:
//!
//! * **"Why is this line like this?"** -- a note, anchored to the line, that
//!   your teammate sees when they open the file.
//! * **"What should I pick up next?"** -- a ticket, with a status and an
//!   assignee, optionally anchored to the code it concerns.
//!
//! It does not do chat. Chat is a solved problem that Slack is better at, and
//! an editor competing with it would lose while making the editor worse.
//! Everything here is attached to work, which is the part a chat window cannot
//! do and an editor can.
//!
//! # Why the repository is the transport
//!
//! A fork with no server has exactly one channel that every teammate is already
//! connected to, already authenticated against, and already synchronises
//! deliberately rather than in the background: the git repository. Records
//! stored in it arrive with `git pull`, survive going offline, review as part
//! of a diff, and need no account. Nothing here can break because a service is
//! down, because there is no service.
//!
//! The cost is that they move at the speed of pushes rather than keystrokes.
//! That is the right trade for work items and code questions, and the wrong one
//! for chat -- which is the other reason this does not try to do chat.
//!
//! # Why JSONL
//!
//! One record per line, sorted by id. A pretty-printed JSON array conflicts in
//! git the moment two people add anything, because both edits land on the same
//! closing bracket. Line-delimited records with stable ordering merge cleanly:
//! two people adding records touch different lines, and git resolves it without
//! anyone being asked. A format that generates merge conflicts is a format
//! nobody will keep using.

pub mod panel;

pub use panel::{AddNote, NewTicket, TeamNotesPanel, ToggleFocus};

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

/// What a record is for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// A question or remark about a specific line. Always anchored.
    #[default]
    Note,
    /// A piece of work. Anchored only if it is about a specific place.
    Ticket,
    /// Something said to the team that is not about a line and is not work.
    ///
    /// Deliberately the same record as the other two, with the same replies and
    /// the same `@mentions`. A separate chat system would need its own storage,
    /// its own sync and its own inbox, and would still be worse at chat than
    /// the tool everyone already has open. What this adds is a message that can
    /// become a ticket without being retyped.
    Message,
}

/// Where a ticket has got to.
///
/// Three states, not seven. A tracker with a rich workflow is a tracker
/// somebody has to administer, and this one has to survive being edited by hand
/// in a merge conflict.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    #[default]
    Open,
    InProgress,
    Done,
}

impl Status {
    pub fn label(self) -> &'static str {
        match self {
            Self::Open => "Open",
            Self::InProgress => "In Progress",
            Self::Done => "Done",
        }
    }

    /// The next state when the status is clicked. Cycles, so one control moves
    /// a ticket all the way through and back.
    pub fn next(self) -> Self {
        match self {
            Self::Open => Self::InProgress,
            Self::InProgress => Self::Done,
            Self::Done => Self::Open,
        }
    }

    pub fn is_closed(self) -> bool {
        matches!(self, Self::Done)
    }
}

/// A note or a ticket, with its replies.
///
/// One type for both, because the difference between them is two optional
/// fields and everything else -- storage, merging, anchoring, replying,
/// mentions -- is identical. Two types would be the same code twice.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NoteThread {
    /// Sortable and unique. Sortable matters: the file is kept in id order so
    /// two people writing records produce a diff git can merge.
    pub id: String,
    #[serde(default)]
    pub kind: Kind,
    /// Present for a note, and for a ticket that is about a particular place.
    /// Worktree-relative with forward slashes on every platform, so a record
    /// written on Windows resolves on Linux.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor: Option<Anchor>,
    /// A ticket's own headline. Notes take their title from the first message,
    /// because a note is a sentence and giving it a separate title would mean
    /// writing the same thing twice.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default)]
    pub status: Status,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assignee: Option<String>,
    #[serde(default)]
    pub resolved: bool,
    pub messages: Vec<Message>,
}

impl NoteThread {
    /// What to show as the record's one-line summary.
    pub fn headline(&self) -> &str {
        if let Some(title) = self.title.as_deref()
            && !title.is_empty()
        {
            return title;
        }
        self.messages
            .first()
            .map(|message| message.body.as_str())
            .unwrap_or_default()
    }

    /// Whether this record is finished, by whichever of its two mechanisms
    /// applies.
    pub fn is_closed(&self) -> bool {
        match self.kind {
            Kind::Note | Kind::Message => self.resolved,
            Kind::Ticket => self.status.is_closed(),
        }
    }

    /// Everyone named with an `@` anywhere in the record, plus its assignee.
    ///
    /// Used to decide what belongs in someone's inbox. Names are compared
    /// without case because a mention is typed by a person, and `@Drew` and
    /// `@drew` are the same person.
    pub fn mentions(&self) -> Vec<String> {
        let mut names: Vec<String> = self.assignee.iter().cloned().collect();
        for message in &self.messages {
            names.extend(extract_mentions(&message.body));
        }
        if let Some(title) = &self.title {
            names.extend(extract_mentions(title));
        }
        names
    }

    /// Whether `name` should see this in their inbox.
    pub fn concerns(&self, name: &str) -> bool {
        self.mentions()
            .iter()
            .any(|mentioned| mentioned.eq_ignore_ascii_case(name))
    }
}

/// Pulls `@name` out of a body.
///
/// A name runs until whitespace or punctuation that cannot be part of one, so
/// `@drew,` and `@drew.` both mention drew. An `@` with nothing after it, or
/// one inside an email address, is not a mention.
pub fn extract_mentions(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    let bytes = text.as_bytes();
    let mut index = 0;

    while let Some(offset) = text[index..].find('@') {
        let at = index + offset;
        index = at + 1;

        // An `@` preceded by a word character is part of an address, not a
        // mention.
        if at > 0
            && bytes
                .get(at - 1)
                .is_some_and(|byte| byte.is_ascii_alphanumeric())
        {
            continue;
        }

        let rest = &text[at + 1..];
        let end = rest
            .find(|character: char| {
                !(character.is_alphanumeric() || character == '-' || character == '_')
            })
            .unwrap_or(rest.len());
        if end > 0 {
            found.push(rest[..end].to_string());
        }
    }

    found
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
            kind: Kind::Note,
            file: Some("src/main.rs".to_string()),
            anchor: Some(Anchor {
                line,
                text: text.to_string(),
            }),
            title: None,
            status: Status::Open,
            assignee: None,
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
            resolve_anchor(thread("a", 1, "let x = 1;").anchor.as_ref().unwrap(), &lines),
            Resolution::Exact(1)
        );
    }

    #[test]
    fn a_note_follows_its_line() {
        let lines = ["// added", "// added", "fn main() {", "    let x = 1;", "}"];
        // Recorded at line 1, the text is now at line 3.
        assert_eq!(
            resolve_anchor(thread("a", 1, "let x = 1;").anchor.as_ref().unwrap(), &lines),
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
            resolve_anchor(thread("a", 2, "}").anchor.as_ref().unwrap(), &lines),
            Resolution::Exact(2)
        );
        assert_eq!(
            resolve_anchor(thread("a", 3, "}").anchor.as_ref().unwrap(), &lines),
            Resolution::Moved(4)
        );
    }

    #[test]
    fn deleted_code_keeps_its_note_rather_than_losing_it() {
        let lines = ["fn main() {", "}"];
        let resolution = resolve_anchor(thread("a", 1, "let x = 1;").anchor.as_ref().unwrap(), &lines);
        assert!(resolution.has_drifted());
        // Clamped into the file, so jumping to it cannot land past the end.
        assert!(resolution.line() < lines.len() as u32);
    }

    #[test]
    fn re_indentation_does_not_orphan_a_note() {
        let lines = ["fn main() {", "        let x = 1;", "}"];
        assert_eq!(
            resolve_anchor(thread("a", 1, "    let x = 1;").anchor.as_ref().unwrap(), &lines),
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
        assert_eq!(parsed[0].anchor.as_ref().unwrap().line, 9);
    }

    #[test]
    fn a_note_written_before_tickets_existed_still_loads() {
        // The fields tickets added are all defaulted, so a file written by an
        // earlier build parses as an open note rather than failing the line --
        // which would silently drop every note somebody already had.
        let legacy = concat!(
            r#"{"id":"a","file":"src/main.rs","#,
            r#""anchor":{"line":3,"text":"let x = 1;"},"#,
            r#""resolved":false,"#,
            r#""messages":[{"author":"drew","at":"2026-01-01T00:00:00Z","body":"why?"}]}"#,
        );
        let parsed = parse(legacy);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].kind, Kind::Note);
        assert_eq!(parsed[0].status, Status::Open);
        assert!(!parsed[0].is_closed());
    }

    #[test]
    fn a_ticket_needs_no_file() {
        let ticket = NoteThread {
            id: "t".into(),
            kind: Kind::Ticket,
            file: None,
            anchor: None,
            title: Some("Ship the installer".into()),
            status: Status::InProgress,
            assignee: Some("drew".into()),
            resolved: false,
            messages: vec![],
        };
        let rendered = render(&[ticket.clone()]).unwrap();
        assert_eq!(parse(&rendered), vec![ticket.clone()]);
        assert_eq!(ticket.headline(), "Ship the installer");
        assert!(!ticket.is_closed());
    }

    #[test]
    fn a_ticket_is_closed_by_status_and_a_note_by_resolution() {
        let mut ticket = NoteThread {
            kind: Kind::Ticket,
            status: Status::Done,
            ..thread("t", 0, "x")
        };
        assert!(ticket.is_closed());
        ticket.status = Status::Open;
        assert!(!ticket.is_closed());

        let mut note = thread("n", 0, "x");
        assert!(!note.is_closed());
        note.resolved = true;
        assert!(note.is_closed());
    }

    #[test]
    fn status_cycles_all_the_way_round() {
        assert_eq!(Status::Open.next(), Status::InProgress);
        assert_eq!(Status::InProgress.next(), Status::Done);
        assert_eq!(Status::Done.next(), Status::Open);
    }

    #[test]
    fn mentions_are_found_but_email_addresses_are_not() {
        assert_eq!(extract_mentions("ping @drew and @sam-b"), vec!["drew", "sam-b"]);
        assert!(extract_mentions("mail me at drew@example.com").is_empty());
        assert!(extract_mentions("an @ on its own").is_empty());
        assert_eq!(extract_mentions("@drew, thoughts?"), vec!["drew"]);
    }

    #[test]
    fn a_record_concerns_its_assignee_and_anyone_mentioned() {
        let mut record = thread("a", 0, "x");
        record.assignee = Some("sam".into());
        record.messages[0].body = "@Drew can you look".into();

        assert!(record.concerns("sam"));
        // Case does not matter: a mention is typed by a person.
        assert!(record.concerns("drew"));
        assert!(!record.concerns("alex"));
    }

    #[test]
    fn a_message_is_closed_by_resolution_like_a_note() {
        let mut message = NoteThread {
            kind: Kind::Message,
            ..thread("m", 0, "x")
        };
        assert!(!message.is_closed());
        message.resolved = true;
        assert!(message.is_closed());
    }

    #[test]
    fn paths_are_recorded_with_forward_slashes() {
        assert_eq!(
            normalize_path(Path::new("src").join("nested").join("file.rs").as_path()),
            "src/nested/file.rs"
        );
    }
}
