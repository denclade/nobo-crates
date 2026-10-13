//! A job that must not run twice at the same time.
//!
//! Refreshing calendars, syncing task lists, syncing the vault: each of
//! them reads the vault, talks to a server and writes files back. Two of
//! them at once is never useful and sometimes harmful -- they race each
//! other's writes, they multiply the requests a server sees, and each one
//! ends by rescanning the vault.
//!
//! And they are easy to trigger twice. A resume event arriving with a
//! visibility change, a button pressed twice, a dev server reloading the
//! frontend while the last pass is still in flight: all of that is
//! ordinary, and none of it should turn into fifty concurrent passes.
//!
//! ```ignore
//! static REFRESHING: OnceAtATime = OnceAtATime::new();
//!
//! let Some(_busy) = REFRESHING.try_enter() else {
//!     return Err("already running".to_string());
//! };
//! // ... the work; the guard releases when it drops, panic included.
//! ```
//!
//! Deliberately not a queue: the second caller wanted the same thing the
//! first one is already doing, so the honest answer is "it is running",
//! not "here it is again in a moment".

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

pub struct OnceAtATime {
    running: AtomicBool,
}

/// Held for the length of the run. Releasing on drop is the point: an
/// early return or a panic must not leave the job marked as running
/// forever, which would be worse than the double run it prevents.
pub struct Busy<'a>(&'a AtomicBool);

impl Drop for Busy<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

impl OnceAtATime {
    pub const fn new() -> Self {
        OnceAtATime { running: AtomicBool::new(false) }
    }

    /// `Some` for the one caller that gets to run, `None` for everyone
    /// arriving while it does.
    pub fn try_enter(&self) -> Option<Busy<'_>> {
        self.running.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).ok().map(|_| Busy(&self.running))
    }

    /// Whether a run is in flight right now. For a caller that wants to
    /// say so rather than start one.
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }
}

impl Default for OnceAtATime {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_second_caller_is_turned_away_while_the_first_runs() {
        let job = OnceAtATime::new();

        let first = job.try_enter();

        assert!(first.is_some());
        assert!(job.try_enter().is_none());
        assert!(job.is_running());
    }

    #[test]
    fn the_next_one_gets_in_once_the_run_is_over() {
        let job = OnceAtATime::new();

        drop(job.try_enter());

        assert!(job.try_enter().is_some());
    }

    /// The failure that would be worse than a double run: a job that
    /// panicked and can never be started again.
    #[test]
    fn a_run_that_panics_still_releases_it() {
        let job = OnceAtATime::new();

        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _busy = job.try_enter().expect("free");
            panic!("mid-run");
        }));

        assert!(panicked.is_err());
        assert!(!job.is_running());
        assert!(job.try_enter().is_some());
    }
}

/// The same, but one run at a time *per key* rather than globally.
///
/// For work that is about one thing: syncing one CalDAV list is not the
/// same job as syncing another, and a global guard would make the open
/// list's own poll swallow a manual sync of a different one. Two runs
/// against the SAME list are the problem -- they double every request and
/// race each other's writes into one mirror file, which is how a heading
/// typed once became two tasks on the server.
///
/// Anyone who touches one subscription takes this: the single-list
/// command, the loop inside the all-lists command, and the background
/// worker. A guard that covers one of several entrances covers nothing.
pub struct OncePerKey {
    running: OnceLock<Mutex<HashSet<String>>>,
}

/// Held for the length of the run, released on drop -- see `Busy`.
pub struct BusyKey<'a> {
    owner: &'a OncePerKey,
    key: String,
}

impl Drop for BusyKey<'_> {
    fn drop(&mut self) {
        if let Some(running) = self.owner.running.get() {
            if let Ok(mut set) = running.lock() {
                set.remove(&self.key);
            }
        }
    }
}

impl OncePerKey {
    pub const fn new() -> Self {
        OncePerKey { running: OnceLock::new() }
    }

    /// `Some` for the one caller that gets to run this key, `None` for
    /// everyone arriving while it does.
    pub fn try_enter(&self, key: &str) -> Option<BusyKey<'_>> {
        let running = self.running.get_or_init(Default::default);
        // A poisoned lock means a panic while holding it, not a state
        // worth refusing over -- the set is a set of strings.
        let mut set = running.lock().unwrap_or_else(|e| e.into_inner());
        set.insert(key.to_string()).then(|| BusyKey { owner: self, key: key.to_string() })
    }
}

impl Default for OncePerKey {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod per_key_tests {
    use super::*;

    #[test]
    fn two_runs_of_the_same_key_do_not_overlap() {
        let guard = OncePerKey::new();

        let first = guard.try_enter("liste-a").expect("the first caller runs");
        assert!(guard.try_enter("liste-a").is_none(), "the second is turned away");

        drop(first);
        assert!(guard.try_enter("liste-a").is_some(), "and can go once the first is done");
    }

    /// The whole reason this is not the global guard: the open list's
    /// 20-second poll must not swallow a manual sync of another list.
    #[test]
    fn different_keys_run_at_the_same_time() {
        let guard = OncePerKey::new();

        let _a = guard.try_enter("liste-a").unwrap();

        assert!(guard.try_enter("liste-b").is_some());
    }
}
