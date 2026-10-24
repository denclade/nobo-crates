// SPDX-FileCopyrightText: 2026 Dennis Burgermeister
// SPDX-License-Identifier: GPL-3.0-or-later

//! Every migration in the app, registered by whoever owns the data.
//!
//! A migration brings one device's own stored data from the shape an
//! older build wrote to the shape this one expects. It runs once, at
//! startup, before anything reads that data.
//!
//! # Registering one
//!
//! Migrations are not listed centrally -- the feature that owns the data
//! owns its migrations, and registers them the same way it registers a
//! file kind:
//!
//! ```ignore
//! inventory::submit! {
//!     Migration {
//!         owner: "caldav",
//!         id: 1,
//!         description: "dropped the sort cache from the list state",
//!         run: |context| { /* ... */ Ok(()) },
//!     }
//! }
//! ```
//!
//! `owner` is a namespace and its own version counter: ids only have to
//! be unique and increasing *within* one owner, two features cannot
//! collide, and deleting a feature deletes its history with it. The
//! reached version per owner lives in `state.json` -- per device, because
//! that is what a migration is about.
//!
//! Migrations of different owners are independent and run in no
//! particular order. One that needs another owner's migration to have run
//! first is not two migrations; it is one, and it belongs to whoever owns
//! the result.
//!
//! # Deleting one
//!
//! Delete it. The floor is derived: once the lowest id an owner still
//! registers is 3, a device sitting at 1 or 2 is refused with a message
//! saying to open it with an older release first -- a fine thing to ask
//! of somebody who skipped two years of updates, and much better than
//! carrying code nobody can test.
//!
//! A device at 0 is a fresh install, never refused: it has nothing to
//! bring forward and simply starts at the current version.
//!
//! # What is NOT migrated
//!
//! **The vault.** It is org files, read by a parser that ignores what it
//! does not recognise and writes back what it did not touch. A new
//! property needs no migration; a renamed one is read under both names
//! for a release. And it could not be migrated anyway: the vault is
//! synced, so the same files are open on devices running different
//! versions, and converting them on one device means the other writes the
//! old shape straight back. A tolerant reader is the only thing that
//! works under mixed versions. The rule that keeps this true: **never
//! repurpose a name.**
//!
//! **Secrets.** They can be renamed, but not from here: a keychain can be
//! locked, slow or absent, and a startup migration would have to block on
//! it and then cope with failing. `core::secrets` renames read-through
//! instead (see `Secrets::renamed_from`), which is self-healing and costs
//! nothing when there is nothing to move.
//!
//! **The sqlite database.** A cache of what servers said, rebuilt when
//! missing, and it grows columns by itself (see `db::ensure_columns`).
//!
//! # Deliberately synchronous
//!
//! `run` cannot await. Everything a migration should touch is a local
//! file this device owns; something that needs the network, a server or a
//! keychain is not a migration of stored data, and making the interface
//! async would invite exactly that -- at startup, where a hung call is a
//! frozen app.

use std::collections::BTreeMap;
use std::path::Path;

/// What a migration is allowed to touch.
///
/// The vault is in here to be LOOKED at -- a migration that has to
/// translate a path it once recorded needs to see what is there. Writing
/// to it is not what migrations are for (see the module doc).
pub struct Context<'a> {
    pub vault_root: Option<&'a Path>,
}

pub struct Migration {
    /// The namespace and version counter: "config", "state", or a feature
    /// name. Its own counter, so two features never collide.
    pub owner: &'static str,
    /// Unique and increasing within the owner. Never renumber, never
    /// reuse: a device remembers the number it reached.
    pub id: u32,
    /// One line, past tense, saying what it did to the data. This is what
    /// somebody reads when a migration is suspected of something years
    /// later.
    pub description: &'static str,
    /// Must be idempotent: a run that failed halfway is tried again on
    /// the next start, and it will meet data it has already converted.
    pub run: fn(&Context) -> Result<(), String>,
}

inventory::collect!(Migration);

fn registered() -> impl Iterator<Item = &'static Migration> {
    inventory::iter::<Migration>.into_iter()
}

/// Every owner that has registered anything, sorted so a log reads the
/// same way twice.
pub fn owners() -> Vec<&'static str> {
    let mut owners: Vec<&'static str> = registered().map(|migration| migration.owner).collect();
    owners.sort_unstable();
    owners.dedup();
    owners
}

/// The version an owner's data reaches with this build: its highest
/// registered id, or 0 when it has none.
pub fn current_version(owner: &str) -> u32 {
    registered().filter(|m| m.owner == owner).map(|m| m.id).max().unwrap_or(0)
}

/// The oldest version this build can still bring forward. Derived from
/// what is left after deletions -- see the module doc.
fn floor(owner: &str) -> u32 {
    registered().filter(|m| m.owner == owner).map(|m| m.id).min().unwrap_or(0)
}

/// Whether this build can work with OWNER's data at version FROM.
///
/// Both directions matter, for different reasons: too old means the
/// migration that would fix it has been deleted, too new means another
/// build has already converted the data to a shape this one does not know
/// -- and writing to it would undo that.
pub fn check_supported(owner: &str, from: u32) -> Result<(), String> {
    // A fresh device has nothing to bring forward.
    if from == 0 {
        return Ok(());
    }
    let floor = floor(owner);
    if from < floor {
        return Err(format!(
            "'{owner}' data is from an app version that is no longer supported (format {from}, this build needs at \
least {floor}). Open it once with an older nobo release to bring it forward."
        ));
    }
    let current = current_version(owner);
    if from > current {
        return Err(format!(
            "'{owner}' data was written by a newer version of nobo (format {from}, this build knows {current}). \
Update nobo before using it here -- writing to it now would undo what the newer version did."
        ));
    }
    Ok(())
}

/// Runs what OWNER still needs, from FROM upwards, and returns the
/// version actually reached.
///
/// Stops at the first failure and reports how far it got, so the caller
/// records exactly that: a run that got through two of three must not be
/// recorded as complete, and must not repeat the two on the next start
/// either.
pub fn run(owner: &str, from: u32, context: &Context) -> (u32, Result<(), String>) {
    if let Err(e) = check_supported(owner, from) {
        return (from, Err(e));
    }
    run_list(&pending(owner, from), from, context)
}

/// OWNER's migrations above FROM, in id order.
fn pending(owner: &str, from: u32) -> Vec<&'static Migration> {
    let mut pending: Vec<&'static Migration> = registered().filter(|m| m.owner == owner && m.id > from).collect();
    pending.sort_by_key(|migration| migration.id);
    pending
}

/// The loop itself, over a list the caller names -- which is what the
/// tests drive, so what they exercise is the code the app runs.
fn run_list(list: &[&Migration], from: u32, context: &Context) -> (u32, Result<(), String>) {
    let mut reached = from;
    for migration in list {
        log::info!("migration {}/{}: {}", migration.owner, migration.id, migration.description);
        if let Err(e) = (migration.run)(context) {
            return (reached, Err(format!("migration {}/{} ({}) failed: {e}", migration.owner, migration.id, migration.description)));
        }
        reached = migration.id;
    }
    (reached, Ok(()))
}

/// Brings every owner's data up to date, once, at startup.
///
/// A failure does not stop the app: that owner stays at the version it
/// reached and the next start tries again. `versions` is what the device
/// remembers (see `PersistedState::migrations`); what comes back is what
/// it should remember from now on.
/// Runs everything that is pending, for every owner.
///
/// Including owners whose feature is switched off (see `core::features`):
/// "off for six months, then on again" must not be a data loss, and a
/// migration that never ran is exactly that. Registration is link-time
/// and each owner counts for itself, so this costs a no-op per switched-
/// off feature and nothing else.
pub fn run_all(vault_root: Option<&Path>, versions: &BTreeMap<String, u32>) -> MigrationReport {
    let context = Context { vault_root };
    let mut report = MigrationReport { versions: versions.clone(), errors: Vec::new() };

    for owner in owners() {
        let from = versions.get(owner).copied().unwrap_or(0);
        let (reached, result) = run(owner, from, &context);
        // Only what actually ran. There is deliberately no "fresh device,
        // nothing ran, so it is current by definition" case:
        //
        // `owners()` is built from the registrations, so an owner with no
        // migrations cannot appear in this loop at all -- and `run(owner,
        // 0, ..)` hands back every registered migration, so a fresh device
        // that SUCCEEDS comes back with `reached == max id` and is
        // recorded by the line above.
        //
        // `from == 0 && reached == 0` therefore means the opposite of what
        // it looked like: the first migration returned `Err` (`run_list`
        // only advances `reached` after a success) or `check_supported`
        // refused. Stamping the current version there marked a FAILED
        // migration as done -- on exactly the population that matters,
        // every existing install on its first build with migrations -- and
        // its legacy keys were then never read again. An unparsable
        // settings.json or a full disk is enough to trigger it.
        if reached != from {
            report.versions.insert(owner.to_string(), reached);
        }
        if let Err(e) = result {
            report.errors.push(e);
        }
    }
    report
}

/// What `run_all` did, so the caller can record it.
#[derive(Debug, PartialEq)]
pub struct MigrationReport {
    /// The version per owner as it stands now. Save this.
    pub versions: BTreeMap<String, u32>,
    pub errors: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static RAN: AtomicU32 = AtomicU32::new(0);

    fn record(_: &Context) -> Result<(), String> {
        RAN.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    fn fail(_: &Context) -> Result<(), String> {
        Err("nope".into())
    }

    const THREE: &[Migration] = &[
        Migration { owner: "test", id: 1, description: "one", run: record },
        Migration { owner: "test", id: 2, description: "two", run: record },
        Migration { owner: "test", id: 3, description: "three", run: fail },
    ];

    fn apply(list: &[&Migration], from: u32) -> (u32, Result<(), String>) {
        run_list(list, from, &Context { vault_root: None })
    }

    #[test]
    fn only_what_is_missing_runs() {
        RAN.store(0, Ordering::SeqCst);

        let (reached, result) = apply(&[&THREE[1]], 1);

        assert!(result.is_ok());
        assert_eq!(reached, 2);
        assert_eq!(RAN.load(Ordering::SeqCst), 1, "the one already applied must not run again");
    }

    /// A run that got halfway must be recorded as halfway: recording it as
    /// complete would skip the rest forever, recording it as untouched
    /// would repeat what already succeeded.
    #[test]
    fn a_failure_reports_how_far_it_got() {
        RAN.store(0, Ordering::SeqCst);

        let (reached, result) = apply(&[&THREE[0], &THREE[1], &THREE[2]], 0);

        assert!(result.is_err());
        assert_eq!(reached, 2);
    }

    #[test]
    fn an_owner_with_nothing_registered_is_already_current() {
        assert_eq!(current_version("nobody-registered-this"), 0);
        assert!(check_supported("nobody-registered-this", 0).is_ok());
    }

    /// Data another build has already converted must not be written back
    /// into its old shape.
    #[test]
    fn a_store_from_the_future_is_refused() {
        let error = check_supported("nobody-registered-this", 99).unwrap_err();

        assert!(error.contains("newer version"), "{error}");
    }

    #[test]
    fn run_refuses_it_too_rather_than_only_its_caller() {
        let (reached, result) = run("nobody-registered-this", 99, &Context { vault_root: None });

        assert_eq!(reached, 99, "nothing was done to it");
        assert!(result.unwrap_err().contains("newer version"));
    }
}
