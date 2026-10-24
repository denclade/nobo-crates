// SPDX-FileCopyrightText: 2026 Dennis Burgermeister
// SPDX-License-Identifier: GPL-3.0-or-later

//! What the expensive things are, when they are supposed to run, and how
//! often they actually did.
//!
//! Every performance problem this app has had was the same shape: not one
//! slow operation, but a cheap one running far more often than anybody
//! thought. A watcher event per write instead of per change. A vault
//! rescan for one rewritten mirror file. A settings read twice per
//! command. None of that is visible in the code you are reading -- it is
//! visible in how many times somebody else calls it.
//!
//! So the heavy operations say so themselves, next to the code that does
//! them:
//!
//! ```ignore
//! inventory::submit! {
//!     Heavy {
//!         id: "vault-scan",
//!         what: "walks the vault, reads and parses every org file",
//!         scale: Scale::Vault,
//!         expected: "once at startup, and when the vault path changes",
//!     }
//! }
//!
//! pub fn scan_directory(&self, dir: &Path) {
//!     let _cost = cost::timed("vault-scan");
//!     // ...
//! }
//! ```
//!
//! `expected` is the part that makes this worth having. A count on its own
//! says nothing -- 40 rescans is fine for 40 vault changes and a disaster
//! for one sync. The sentence says what the number should look like, so
//! anybody (or the dev bar, see `heavy_operations`) can compare the two
//! without knowing the code.
//!
//! # What this is not
//!
//! Not a profiler and not a budget: nothing is refused, nothing is
//! sampled, nothing is written to disk. It is a counter and a stopwatch
//! per operation, plus who asked -- so that "why is this slow" starts with
//! a list instead of a guess.
//!
//! Cheap enough for the paths it sits on: one `Instant::now()` pair and a
//! short mutex, against operations that are milliseconds by definition (a
//! parse, a rescan, a sync). Deliberately NOT on the truly hot ones --
//! `file_kind::capabilities` and `features::is_on` run per write and are
//! measured in nanoseconds; instrumenting those would cost more than they
//! do.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// How an operation grows with what it is given.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, specta::Type)]
#[serde(rename_all = "kebab-case")]
pub enum Scale {
    /// With the whole vault: every file read and parsed.
    Vault,
    /// With one file's size.
    File,
    /// With what is on the network, not on disk.
    Remote,
    /// Neither -- it costs what it costs.
    Constant,
}

/// One expensive thing, declared where it happens.
pub struct Heavy {
    /// Stable id, also what the report shows.
    pub id: &'static str,
    /// What it does, in one line.
    pub what: &'static str,
    pub scale: Scale,
    /// When it is supposed to run. The sentence a count is read against.
    pub expected: &'static str,
}

inventory::collect!(Heavy);

/// Every declared operation, in a stable order.
pub fn declared() -> Vec<&'static Heavy> {
    let mut all: Vec<&'static Heavy> = inventory::iter::<Heavy>.into_iter().collect();
    all.sort_by_key(|heavy| heavy.id);
    all
}

#[derive(Debug, Default, Clone, Copy)]
struct Tally {
    runs: u64,
    total: Duration,
    longest: Duration,
}

/// Counted per operation AND per caller: "the rescan ran 40 times" is a
/// different problem from "CalDAV asked for the rescan 40 times", and only
/// the second one tells you where to look.
type Ledger = HashMap<(&'static str, &'static str), Tally>;

fn ledger() -> &'static Mutex<Ledger> {
    static LEDGER: OnceLock<Mutex<Ledger>> = OnceLock::new();
    LEDGER.get_or_init(Default::default)
}

/// Times the rest of the current scope, on the core's own account.
pub fn timed(id: &'static str) -> Timing {
    timed_by(id, "core")
}

/// The same, for a feature running one of these on its own behalf.
///
/// `who` is the feature's id (see `core::features`), so a report can say
/// which one is asking -- the calendar refresh and the CalDAV sync both
/// write mirror files, and a count that lumps them together names neither.
pub fn timed_by(id: &'static str, who: &'static str) -> Timing {
    Timing { id, who, started: Instant::now() }
}

/// Records on drop -- an early return or a `?` is still a run that
/// happened.
pub struct Timing {
    id: &'static str,
    who: &'static str,
    started: Instant,
}

impl Drop for Timing {
    fn drop(&mut self) {
        let elapsed = self.started.elapsed();
        if let Ok(mut ledger) = ledger().lock() {
            let tally = ledger.entry((self.id, self.who)).or_default();
            tally.runs += 1;
            tally.total += elapsed;
            tally.longest = tally.longest.max(elapsed);
        }
    }
}

/// One row of the report: what was declared, plus what happened.
#[derive(Debug, Clone, serde::Serialize, specta::Type)]
pub struct HeavyReport {
    pub id: String,
    pub what: String,
    pub scale: Scale,
    pub expected: String,
    /// Who ran it, with their own counts. Empty when it has not run.
    pub callers: Vec<CallerReport>,
    /// A count, as a JS number: nothing here reaches the range where that
    /// would lose a digit (see specta's BigInt rule).
    #[specta(type = specta_typescript::Number)]
    pub runs: u64,
    #[specta(type = specta_typescript::Number)]
    pub total_ms: f64,
    #[specta(type = specta_typescript::Number)]
    pub longest_ms: f64,
}

#[derive(Debug, Clone, serde::Serialize, specta::Type)]
pub struct CallerReport {
    /// A feature's id, or "core".
    pub who: String,
    #[specta(type = specta_typescript::Number)]
    pub runs: u64,
    #[specta(type = specta_typescript::Number)]
    pub total_ms: f64,
}

/// What every declared operation has done since the last `reset`.
///
/// Declared-but-never-run operations are in here too, with zero: "this
/// never happens" is as much an answer as a number.
pub fn report() -> Vec<HeavyReport> {
    let ledger = ledger().lock().map(|l| l.clone()).unwrap_or_default();

    declared()
        .into_iter()
        .map(|heavy| {
            let mut callers: Vec<CallerReport> = ledger
                .iter()
                .filter(|((id, _), _)| *id == heavy.id)
                .map(|((_, who), tally)| CallerReport {
                    who: who.to_string(),
                    runs: tally.runs,
                    total_ms: tally.total.as_secs_f64() * 1000.0,
                })
                .collect();
            callers.sort_by(|a, b| b.runs.cmp(&a.runs).then_with(|| a.who.cmp(&b.who)));

            let mine = ledger.iter().filter(|((id, _), _)| *id == heavy.id);
            let runs = mine.clone().map(|(_, tally)| tally.runs).sum();
            let total: Duration = mine.clone().map(|(_, tally)| tally.total).sum();
            let longest = mine.map(|(_, tally)| tally.longest).max().unwrap_or_default();

            HeavyReport {
                id: heavy.id.to_string(),
                what: heavy.what.to_string(),
                scale: heavy.scale,
                expected: heavy.expected.to_string(),
                callers,
                runs,
                total_ms: total.as_secs_f64() * 1000.0,
                longest_ms: longest.as_secs_f64() * 1000.0,
            }
        })
        .collect()
}

/// Starts counting again -- for "what does THIS action cost", which is the
/// question worth asking.
pub fn reset() {
    if let Ok(mut ledger) = ledger().lock() {
        ledger.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The ledger is one per process and `reset` clears all of it, so
    /// these tests cannot run beside each other -- cargo runs them on
    /// threads by default. One at a time, rather than each of them
    /// counting its neighbours' runs (the mistake this module is here to
    /// make visible, made in the tests for it).
    static SERIALIZED: Mutex<()> = Mutex::new(());

    fn alone() -> std::sync::MutexGuard<'static, ()> {
        SERIALIZED.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    inventory::submit! {
        Heavy {
            id: "test-heavy",
            what: "stands in for something expensive",
            scale: Scale::File,
            expected: "only from the tests in this module",
        }
    }

    fn row(id: &str) -> HeavyReport {
        report().into_iter().find(|row| row.id == id).expect("declared above")
    }

    #[test]
    fn a_declared_operation_is_in_the_report_even_before_it_runs() {
        let _alone = alone();
        reset();
        let row = row("test-heavy");

        assert_eq!(row.runs, 0);
        assert!(row.callers.is_empty());
        assert_eq!(row.expected, "only from the tests in this module");
    }

    #[test]
    fn what_ran_is_counted_per_caller() {
        let _alone = alone();
        reset();
        {
            let _one = timed("test-heavy");
            let _two = timed_by("test-heavy", "caldav");
            let _three = timed_by("test-heavy", "caldav");
        }

        let row = row("test-heavy");
        assert_eq!(row.runs, 3);
        assert_eq!(row.callers[0].who, "caldav", "the busiest caller first");
        assert_eq!(row.callers[0].runs, 2);
        assert_eq!(row.callers[1].who, "core");
    }

    /// The counter has to survive the `?` that ends the operation early --
    /// a run that failed is still a run that cost something.
    #[test]
    fn an_early_return_still_counts() {
        let _alone = alone();
        reset();

        fn gave_up() -> Result<(), ()> {
            let _cost = timed("test-heavy");
            Err(())
        }
        let _ = gave_up();

        assert_eq!(row("test-heavy").runs, 1);
    }
}
