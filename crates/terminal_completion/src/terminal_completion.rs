//! Command history, and the suggestion drawn from it.
//!
//! The terminal already knows which commands you run and where you ran them.
//! Recording that and offering the best match back as ghost text is the highest
//! value-per-line in terminal completion: it needs no command specs, no
//! subprocess, and no network, and it is right most of the time because people
//! rerun their own commands constantly.
//!
//! # What "best" means
//!
//! Ranked in this order, because that is the order in which the signals are
//! actually trustworthy:
//!
//! 1. **Directory.** A command run in *this* directory beats the same command
//!    run elsewhere, and a command from an ancestor beats one from an unrelated
//!    tree. `cargo run` means something different in two different repos.
//! 2. **Frecency.** Recency-decayed frequency rather than raw count, so the
//!    thing you ran fifty times last year stops outranking what you ran twice
//!    this morning.
//! 3. **Success.** A command that has only ever failed is never suggested. It
//!    is not a shortcut, it is a mistake waiting to be repeated.
//! 4. **Session.** This session's commands outrank previous ones, because
//!    whatever you are doing now is the best predictor of what you will do next.

pub mod history;
pub mod path_commands;
pub mod shell_history;

/// Commands kept per terminal. Large enough to cover a long working session,
/// small enough that ranking over it stays instant.
pub const DEFAULT_HISTORY_LIMIT: usize = 10_000;

pub use history::{HistoryEntry, HistoryStore, Suggestion, TerminalHistoryDb};
pub use shell_history::ShellHistoryKind;
