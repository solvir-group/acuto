//! One agent at a time per file.
//!
//! Several agent threads can run at once -- the eight-pane layout exists
//! precisely so they do -- and nothing stopped two of them opening an edit
//! session on the same file. When that happens neither agent is wrong and both
//! lose: they read the same starting text, compute edits against it, and the
//! second one's edits land on a buffer that no longer matches what it read.
//! The result is a file that compiles by luck, and two transcripts that each
//! claim to have made a change.
//!
//! A claim is held for one tool call. It is not a lock on the file between
//! calls: an agent that finishes editing releases immediately, so two agents
//! can still take turns on the same file within one turn each. What it stops is
//! the overlap, which is the part that corrupts.
//!
//! Deliberately coarse. Line-level claims sound better and are not available
//! here: an edit session knows its path before it knows which lines the model
//! will touch, because the edits stream in afterwards. A file-level claim can
//! be taken at the only moment when refusing is still cheap.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

/// How long a claim survives without being released.
///
/// A claim is released when its guard drops, which covers every ordinary path
/// including errors and cancellation. This is for the paths that are not
/// ordinary -- a panic between claiming and dropping -- where the alternative
/// is a file no agent can ever edit again until the editor restarts.
const CLAIM_EXPIRY: Duration = Duration::from_secs(120);

struct Claim {
    /// Which thread holds it. Compared rather than displayed, so it only has to
    /// be unique within the process.
    holder: u64,
    /// What to call the holder when telling another agent why it cannot edit.
    holder_label: String,
    taken_at: Instant,
}

fn claims() -> &'static Mutex<HashMap<PathBuf, Claim>> {
    static CLAIMS: OnceLock<Mutex<HashMap<PathBuf, Claim>>> = OnceLock::new();
    CLAIMS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Holds a claim for as long as it is alive.
///
/// Dropping releases, so an edit session that ends any way at all -- returned,
/// errored, cancelled mid-stream -- gives the file back without the caller
/// having to remember.
pub(crate) struct EditClaim {
    path: PathBuf,
    holder: u64,
}

impl Drop for EditClaim {
    fn drop(&mut self) {
        let Ok(mut claims) = claims().lock() else {
            return;
        };
        // Only if it is still ours: an expired claim may have been taken over
        // by another thread, and releasing then would hand that thread's file
        // to a third.
        if claims.get(&self.path).is_some_and(|claim| claim.holder == self.holder) {
            claims.remove(&self.path);
        }
    }
}

/// Why a file could not be claimed.
pub(crate) struct Blocked {
    pub holder_label: String,
}

/// Takes the claim on `path` for `holder`, or reports who has it.
///
/// Re-entrant for the same holder: an agent that opens a second session on a
/// file it is already editing is sequencing its own work, which is fine, and
/// refusing would break a legitimate two-step edit.
pub(crate) fn claim(path: &Path, holder: u64, holder_label: &str) -> Result<EditClaim, Blocked> {
    let Ok(mut claims) = claims().lock() else {
        // A poisoned registry means some other thread panicked while holding
        // the lock. Coordination is an improvement on a best-effort basis, not
        // a correctness guarantee: refusing every edit from here on would be a
        // worse failure than the one being guarded against.
        return Ok(EditClaim {
            path: path.to_path_buf(),
            holder,
        });
    };

    if let Some(existing) = claims.get(path)
        && existing.holder != holder
        && existing.taken_at.elapsed() < CLAIM_EXPIRY
    {
        return Err(Blocked {
            holder_label: existing.holder_label.clone(),
        });
    }

    claims.insert(
        path.to_path_buf(),
        Claim {
            holder,
            holder_label: holder_label.to_string(),
            taken_at: Instant::now(),
        },
    );

    Ok(EditClaim {
        path: path.to_path_buf(),
        holder,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each test uses paths of its own, because the registry is process-wide
    /// and the test harness runs them in parallel.
    fn unique(name: &str) -> PathBuf {
        PathBuf::from(format!("/test/{name}/{:?}", std::thread::current().id()))
    }

    #[test]
    fn a_second_agent_is_refused_and_told_who_has_it() {
        let path = unique("refused");
        let _first = claim(&path, 1, "Fix the parser").expect("first claim");

        let blocked = claim(&path, 2, "Update the docs").err().expect("refused");
        assert_eq!(blocked.holder_label, "Fix the parser");
    }

    #[test]
    fn the_same_agent_may_claim_again() {
        let path = unique("reentrant");
        let _first = claim(&path, 7, "Thread").expect("first claim");
        assert!(claim(&path, 7, "Thread").is_ok());
    }

    #[test]
    fn releasing_lets_the_next_agent_in() {
        let path = unique("released");
        {
            let _held = claim(&path, 1, "First").expect("claim");
            assert!(claim(&path, 2, "Second").is_err());
        }
        assert!(claim(&path, 2, "Second").is_ok());
    }

    #[test]
    fn a_stale_guard_does_not_release_someone_elses_claim() {
        let path = unique("stolen");
        let first = claim(&path, 1, "First").expect("claim");

        // Simulate expiry, then a takeover.
        claims()
            .lock()
            .unwrap()
            .get_mut(&path)
            .unwrap()
            .taken_at = Instant::now() - CLAIM_EXPIRY - Duration::from_secs(1);
        let _second = claim(&path, 2, "Second").expect("takes over an expired claim");

        drop(first);

        // The second holder still has it.
        assert!(claim(&path, 3, "Third").is_err());
    }

    #[test]
    fn different_files_do_not_block_each_other() {
        let one = unique("one");
        let two = unique("two");
        let _a = claim(&one, 1, "First").expect("claim");
        assert!(claim(&two, 2, "Second").is_ok());
    }
}
