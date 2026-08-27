//! Persistent command history and the ranking over it.
//!
//! Stored through the editor's existing sqlite layer rather than a new
//! dependency, in its own [`Domain`] so the migration is versioned
//! independently of anything else.

use std::path::{Path, PathBuf};

use anyhow::Result;
use sqlez::{
    domain::Domain, statement::Statement, thread_safe_connection::ThreadSafeConnection,
};

/// Weight applied per day of age when scoring.
///
/// Halves roughly every ten days: recent enough that this morning's work
/// dominates, slow enough that a command you run weekly does not vanish.
const DECAY_PER_DAY: f64 = 0.933;

/// Multiplier for a command recorded in this session.
const SESSION_BOOST: f64 = 2.0;

/// Multiplier for an exact working-directory match.
const EXACT_DIRECTORY_BOOST: f64 = 4.0;

/// Multiplier for a command from an ancestor of the current directory.
const ANCESTOR_DIRECTORY_BOOST: f64 = 2.0;

/// One recorded command.
#[derive(Clone, Debug, PartialEq)]
pub struct HistoryEntry {
    pub command: String,
    pub cwd: Option<PathBuf>,
    pub exit_code: Option<i32>,
    /// Unix seconds.
    pub started_at: i64,
    pub duration_ms: i64,
    pub session_id: String,
}

/// A ranked suggestion.
#[derive(Clone, Debug, PartialEq)]
pub struct Suggestion {
    pub command: String,
    pub score: f64,
}

impl Suggestion {
    /// The text that would be appended if this were accepted.
    pub fn completion_after(&self, typed: &str) -> Option<&str> {
        self.command
            .strip_prefix(typed)
            .filter(|rest| !rest.is_empty())
    }
}

pub struct TerminalHistoryDb(ThreadSafeConnection);

impl Domain for TerminalHistoryDb {
    const NAME: &str = stringify!(TerminalHistoryDb);

    const MIGRATIONS: &[&str] = &[indoc::indoc! {"
        CREATE TABLE terminal_history (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            command TEXT NOT NULL,
            cwd TEXT,
            exit_code INTEGER,
            started_at INTEGER NOT NULL,
            duration_ms INTEGER NOT NULL,
            session_id TEXT NOT NULL
        ) STRICT;

        CREATE INDEX terminal_history_command ON terminal_history (command);
        CREATE INDEX terminal_history_cwd ON terminal_history (cwd);
    "}];
}

db::static_connection!(TerminalHistoryDb, []);

/// In-memory history with a persistent backing store.
///
/// Ranking runs over the in-memory copy: a keystroke must never wait on a
/// query, and the whole history of one developer is small enough that holding
/// it costs less than the machinery required to avoid holding it.
pub struct HistoryStore {
    entries: Vec<HistoryEntry>,
    session_id: String,
    limit: usize,
}

impl HistoryStore {
    pub fn new(session_id: impl Into<String>, limit: usize) -> Self {
        Self {
            entries: Vec::new(),
            session_id: session_id.into(),
            limit,
        }
    }

    pub fn entries(&self) -> &[HistoryEntry] {
        &self.entries
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Records a finished command.
    ///
    /// Blank commands are dropped, and so are immediate repeats — a user
    /// hammering Enter at a prompt should not fill history with copies of the
    /// last thing they ran.
    ///
    /// A repeat with a *different* outcome is kept, because that is new
    /// information rather than noise: a command that failed and then succeeded
    /// has stopped being a mistake, and suppressing the success would leave it
    /// permanently unsuggestable.
    pub fn record(&mut self, entry: HistoryEntry) -> bool {
        let command = entry.command.trim();
        if command.is_empty() {
            return false;
        }

        if self.entries.last().is_some_and(|last| {
            last.command.trim() == command
                && last.cwd == entry.cwd
                && last.exit_code == entry.exit_code
        }) {
            return false;
        }

        self.entries.push(HistoryEntry {
            command: command.to_string(),
            ..entry
        });

        while self.entries.len() > self.limit {
            self.entries.remove(0);
        }
        true
    }

    /// The best suggestion for what the user has typed so far.
    ///
    /// `None` for an empty or whitespace-only prefix: with nothing typed every
    /// command matches, and suggesting one would be noise rather than help.
    pub fn suggest(&self, typed: &str, cwd: Option<&Path>, now: i64) -> Option<Suggestion> {
        if typed.trim().is_empty() {
            return None;
        }

        let mut best: Option<Suggestion> = None;

        for entry in &self.entries {
            if !entry.command.starts_with(typed) || entry.command == typed {
                continue;
            }

            // A command that has only ever failed is not a shortcut, it is a
            // mistake waiting to be repeated.
            if self.only_ever_failed(&entry.command) {
                continue;
            }

            let score = self.score(entry, cwd, now);
            if best.as_ref().is_none_or(|current| score > current.score) {
                best = Some(Suggestion {
                    command: entry.command.clone(),
                    score,
                });
            }
        }

        best
    }

    fn only_ever_failed(&self, command: &str) -> bool {
        let mut saw_outcome = false;
        for entry in self.entries.iter().filter(|entry| entry.command == command) {
            match entry.exit_code {
                Some(0) => return false,
                Some(_) => saw_outcome = true,
                // Unknown is not failure. Without shell integration reporting a
                // code there is nothing to hold against the command.
                None => return false,
            }
        }
        saw_outcome
    }

    fn score(&self, entry: &HistoryEntry, cwd: Option<&Path>, now: i64) -> f64 {
        let age_days = ((now - entry.started_at).max(0) as f64) / 86_400.0;
        let mut score = DECAY_PER_DAY.powf(age_days);

        score *= match (cwd, entry.cwd.as_deref()) {
            (Some(current), Some(recorded)) if current == recorded => EXACT_DIRECTORY_BOOST,
            (Some(current), Some(recorded)) if current.starts_with(recorded) => {
                ANCESTOR_DIRECTORY_BOOST
            }
            _ => 1.0,
        };

        if entry.session_id == self.session_id {
            score *= SESSION_BOOST;
        }

        score
    }
}

/// Reads and writes are async so the caller never blocks a keystroke on sqlite.
impl TerminalHistoryDb {
    pub async fn record(&self, entry: HistoryEntry) -> Result<()> {
        let cwd = entry
            .cwd
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned());

        self.write(move |connection| {
            let mut statement = Statement::prepare(
                connection,
                "INSERT INTO terminal_history
                 (command, cwd, exit_code, started_at, duration_ms, session_id)
                 VALUES (?, ?, ?, ?, ?, ?)",
            )?;
            statement.bind(&entry.command, 1)?;
            statement.bind(&cwd, 2)?;
            statement.bind(&entry.exit_code, 3)?;
            statement.bind(&entry.started_at, 4)?;
            statement.bind(&entry.duration_ms, 5)?;
            statement.bind(&entry.session_id, 6)?;
            statement.exec()
        })
        .await
    }

    /// Loads the most recent `limit` entries, oldest first.
    ///
    /// Ordered by id rather than timestamp: the clock can move backwards, and
    /// insertion order is what "most recent" actually means here.
    pub async fn recent(&self, limit: usize) -> Result<Vec<HistoryEntry>> {
        let rows: Vec<(String, Option<String>, Option<i32>, i64, i64, String)> = self
            .select_bound(
                "SELECT command, cwd, exit_code, started_at, duration_ms, session_id
                 FROM terminal_history ORDER BY id DESC LIMIT ?",
            )?(limit as i32)?;

        Ok(rows
            .into_iter()
            .rev()
            .map(
                |(command, cwd, exit_code, started_at, duration_ms, session_id)| HistoryEntry {
                    command,
                    cwd: cwd.map(PathBuf::from),
                    exit_code,
                    started_at,
                    duration_ms,
                    session_id,
                },
            )
            .collect())
    }

    /// Drops the oldest rows beyond `limit`, so a long-lived install does not
    /// grow without bound.
    pub async fn prune(&self, limit: usize) -> Result<()> {
        self.write(move |connection| {
            let mut statement = Statement::prepare(
                connection,
                "DELETE FROM terminal_history WHERE id NOT IN
                 (SELECT id FROM terminal_history ORDER BY id DESC LIMIT ?)",
            )?;
            statement.bind(&(limit as i32), 1)?;
            statement.exec()
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_700_000_000;
    const DAY: i64 = 86_400;

    fn entry(command: &str, cwd: Option<&str>, exit: Option<i32>, ago_days: i64) -> HistoryEntry {
        HistoryEntry {
            command: command.to_string(),
            cwd: cwd.map(PathBuf::from),
            exit_code: exit,
            started_at: NOW - ago_days * DAY,
            duration_ms: 10,
            session_id: "session-a".to_string(),
        }
    }

    fn store() -> HistoryStore {
        HistoryStore::new("session-a", 1000)
    }

    #[test]
    fn empty_prefix_suggests_nothing() {
        let mut store = store();
        store.record(entry("cargo build", None, Some(0), 0));
        assert_eq!(store.suggest("", None, NOW), None);
        assert_eq!(store.suggest("   ", None, NOW), None);
    }

    #[test]
    fn suggests_by_prefix_and_reports_the_remainder() {
        let mut store = store();
        store.record(entry("cargo build --release", None, Some(0), 0));

        let suggestion = store.suggest("car", None, NOW).expect("matched");
        assert_eq!(suggestion.command, "cargo build --release");
        assert_eq!(suggestion.completion_after("car"), Some("go build --release"));
    }

    #[test]
    fn an_exact_match_is_not_a_suggestion() {
        let mut store = store();
        store.record(entry("ls", None, Some(0), 0));
        assert_eq!(
            store.suggest("ls", None, NOW),
            None,
            "there is nothing left to accept"
        );
    }

    #[test]
    fn the_current_directory_wins() {
        let mut store = store();
        store.record(entry("cargo run --bin server", Some("/work/other"), Some(0), 0));
        store.record(entry("cargo run --bin client", Some("/work/here"), Some(0), 0));

        let suggestion = store
            .suggest("cargo run", Some(Path::new("/work/here")), NOW)
            .expect("matched");
        assert_eq!(
            suggestion.command, "cargo run --bin client",
            "the same command means different things in different repos"
        );
    }

    #[test]
    fn an_ancestor_directory_beats_an_unrelated_one() {
        let mut store = store();
        store.record(entry("make test", Some("/somewhere/else"), Some(0), 0));
        store.record(entry("make build", Some("/work"), Some(0), 0));

        let suggestion = store
            .suggest("make", Some(Path::new("/work/crates/inner")), NOW)
            .expect("matched");
        assert_eq!(suggestion.command, "make build");
    }

    #[test]
    fn recent_beats_old() {
        let mut store = store();
        store.record(entry("git push origin old", None, Some(0), 60));
        store.record(entry("git push origin new", None, Some(0), 0));

        let suggestion = store.suggest("git push", None, NOW).expect("matched");
        assert_eq!(suggestion.command, "git push origin new");
    }

    #[test]
    fn a_command_that_only_ever_failed_is_never_suggested() {
        let mut store = store();
        store.record(entry("npm run broken", None, Some(1), 0));

        assert_eq!(
            store.suggest("npm", None, NOW),
            None,
            "suggesting it would be repeating a mistake"
        );
    }

    #[test]
    fn one_success_redeems_a_command() {
        let mut store = store();
        store.record(entry("npm run flaky", None, Some(1), 2));
        store.record(entry("npm run flaky", None, Some(0), 1));

        let suggestion = store.suggest("npm", None, NOW).expect("matched");
        assert_eq!(suggestion.command, "npm run flaky");
    }

    #[test]
    fn an_unknown_exit_code_is_not_failure() {
        let mut store = store();
        // No shell integration reporting a code: there is nothing to hold
        // against the command.
        store.record(entry("some-command", None, None, 0));
        assert!(store.suggest("some", None, NOW).is_some());
    }

    #[test]
    fn this_session_outranks_previous_ones() {
        let mut store = store();
        let mut older = entry("deploy staging", None, Some(0), 0);
        older.session_id = "session-old".to_string();
        store.record(older);
        store.record(entry("deploy production", None, Some(0), 0));

        let suggestion = store.suggest("deploy", None, NOW).expect("matched");
        assert_eq!(suggestion.command, "deploy production");
    }

    #[test]
    fn blank_and_repeated_commands_are_not_recorded() {
        let mut store = store();
        assert!(!store.record(entry("", None, Some(0), 0)));
        assert!(!store.record(entry("   ", None, Some(0), 0)));

        assert!(store.record(entry("ls -la", None, Some(0), 0)));
        assert!(
            !store.record(entry("ls -la", None, Some(0), 0)),
            "hammering Enter should not fill history with duplicates"
        );
        assert_eq!(store.entries().len(), 1);
    }

    #[test]
    fn the_limit_evicts_oldest_first() {
        let mut store = HistoryStore::new("session-a", 2);
        store.record(entry("one", None, Some(0), 0));
        store.record(entry("two", None, Some(0), 0));
        store.record(entry("three", None, Some(0), 0));

        let commands: Vec<&str> = store
            .entries()
            .iter()
            .map(|entry| entry.command.as_str())
            .collect();
        assert_eq!(commands, vec!["two", "three"]);
    }
}
