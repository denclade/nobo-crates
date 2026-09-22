//! Which features this build has, and which of them are switched on.
//!
//! Two different questions, and the difference is the whole point:
//!
//! - **Compiled in.** Decided when the binary was built and unchangeable
//!   afterwards. An iOS build has no org-babel at all -- not a hidden
//!   button, no interpreter, nothing to switch on (App Store rule 2.5.2,
//!   see `nobo_babel`).
//! - **Switched on.** A choice on this device, stored in `settings.json`
//!   and changeable at runtime. What is off keeps all its data: its
//!   settings section, its files, its accounts. Switching it back on is
//!   not an import.
//!
//! A feature nobody switched off is on. That matters for an update that
//! brings a new feature -- it arrives on, not silently missing -- and for
//! a settings file written by a build that had features this one has
//! never heard of.
//!
//! # Registering
//!
//! Like every other thing a feature declares (see `core::file_kind`,
//! `core::search`), it registers itself:
//!
//! ```ignore
//! inventory::submit! {
//!     FeatureSpec {
//!         id: "caldav",
//!         compiled_in: cfg!(feature = "caldav"),
//!         needs: &[],
//!         built_on: &["nobo-http", "nobo-webdav", "nobo-ical", "nobo-calendar-mirror"],
//!     }
//! }
//! ```
//!
//! The `cfg!` sits at the registration site because only the feature can
//! name its own cargo flag -- the core would have to keep a list, which
//! is the thing this whole layout is against.
//!
//! # Reading it cheaply
//!
//! `is_on` is asked on every file write (see `fs_utils::write_lines` and
//! `file_kind::capabilities`), so it must not touch the disk. The
//! switched-off set is read once and kept; `set_enabled` is the only
//! thing that changes it, and it writes the file and the cache together.

use std::collections::{HashMap, HashSet};
use std::sync::{OnceLock, RwLock};

use serde::{Deserialize, Serialize};

use crate::config::section::Section;

/// One feature, registered by itself.
pub struct FeatureSpec {
    /// Its cargo feature name, which is also its id in the frontend
    /// manifest and the name of its settings section.
    pub id: &'static str,
    /// Whether this build compiled it in.
    pub compiled_in: bool,
    /// What it is nothing without. Switching off `calendar` switches off
    /// `caldav` with it -- CalDAV writes the calendar mirrors.
    pub needs: &'static [&'static str],
    /// The shared libraries it is built on (`nobo-http`, `nobo-webdav`,
    /// `nobo-ical`, `nobo-calendar-mirror`).
    ///
    /// Not features: they have no switch, no settings and no screen --
    /// they are what two features have in common, kept out of both (see
    /// docs/architecture.md). Named here because the question "what does
    /// this thing bring into my build" is a fair one, and the switch list
    /// is where it gets asked. `tools/check-feature-libraries.mjs`
    /// compares this against the crate's actual Cargo.toml, so it cannot
    /// quietly go out of date.
    pub built_on: &'static [&'static str],
}

inventory::collect!(FeatureSpec);

/// One registered feature, with the walk through its `needs` already
/// done.
struct Registered {
    spec: &'static FeatureSpec,
    /// Everything it stands on, transitively and flattened -- so the
    /// question on the write path is a lookup and a handful of
    /// comparisons instead of a recursive walk each time.
    ///
    /// `None` when something in that chain is not registered at all. Such
    /// a feature can never be on (see `is_on`), and the walk has nothing
    /// to follow. `every_feature_needs_something_registered` makes sure
    /// this build has no such case.
    stands_on: Option<Vec<&'static FeatureSpec>>,
}

/// Who is registered, resolved once.
///
/// The registrations arrive from the linker (`inventory`) and cannot
/// change afterwards, so both the lookup and the `needs` walk are done
/// once for the life of the process rather than on every question. The
/// walk carries a `seen` set: two features that name each other would
/// otherwise recurse until the stack ends -- on a path that runs at every
/// file write. It cannot happen in a build that passes
/// `no_feature_stands_on_itself`, and it does not crash even if it did.
fn registry() -> &'static HashMap<&'static str, Registered> {
    static REGISTRY: OnceLock<HashMap<&'static str, Registered>> = OnceLock::new();
    REGISTRY.get_or_init(|| {
        let by_id: HashMap<&'static str, &'static FeatureSpec> =
            inventory::iter::<FeatureSpec>.into_iter().map(|spec| (spec.id, spec)).collect();

        by_id
            .values()
            .map(|spec| (spec.id, Registered { spec, stands_on: flatten_needs(spec, &by_id) }))
            .collect()
    })
}

/// Every feature SPEC stands on, directly or through another.
///
/// `None` if any of them is not registered.
fn flatten_needs(
    spec: &'static FeatureSpec,
    by_id: &HashMap<&'static str, &'static FeatureSpec>,
) -> Option<Vec<&'static FeatureSpec>> {
    let mut stands_on = Vec::new();
    let mut seen: HashSet<&'static str> = HashSet::from([spec.id]);
    let mut todo: Vec<&'static str> = spec.needs.to_vec();

    while let Some(id) = todo.pop() {
        if !seen.insert(id) {
            continue;
        }
        let needed = by_id.get(id)?;
        stands_on.push(*needed);
        todo.extend(needed.needs);
    }
    Some(stands_on)
}

/// Every feature this build knows of, compiled in or not, in a stable
/// order.
pub fn all() -> Vec<&'static FeatureSpec> {
    let mut specs: Vec<&'static FeatureSpec> = registry().values().map(|entry| entry.spec).collect();
    specs.sort_by_key(|spec| spec.id);
    specs
}

fn spec(id: &str) -> Option<&'static FeatureSpec> {
    registry().get(id).map(|entry| entry.spec)
}

/// What is stored: the exceptions, not the state of everything.
///
/// A list of what is OFF rather than a map of on/off, so a feature this
/// build has never heard of -- written by a newer build, or by one with
/// more compiled in -- keeps its answer instead of being read as absent.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct Switches {
    #[serde(default)]
    disabled: Vec<String>,
}

/// Where the switches live. Not a feature's own section: which features
/// are on is the app's business, not any one feature's.
const SWITCHES: Section<Switches> = Section::new("features");

/// The switched-off set, read once. `None` until the first question.
static DISABLED: OnceLock<RwLock<HashSet<String>>> = OnceLock::new();

fn disabled() -> &'static RwLock<HashSet<String>> {
    DISABLED.get_or_init(|| RwLock::new(SWITCHES.load().disabled.into_iter().collect()))
}

/// Whether FEATURE is compiled in AND switched on -- and so are the
/// features it needs.
///
/// The one question the rest of the app asks. A feature that is not
/// registered at all answers `false`: nothing can be served by something
/// that does not exist, and answering `true` would make a typo look like
/// a working feature.
pub fn is_on(feature: &str) -> bool {
    let Some(entry) = registry().get(feature) else { return false };
    // A feature is only as on as what it is built on, so what it stands
    // on is asked exactly like itself -- and something it needs that
    // nobody registered leaves it off for good.
    let Some(stands_on) = &entry.stands_on else { return false };
    if !entry.spec.compiled_in || stands_on.iter().any(|needed| !needed.compiled_in) {
        return false;
    }
    let Ok(off) = disabled().read() else { return true };
    !off.contains(feature) && !stands_on.iter().any(|needed| off.contains(needed.id))
}

/// Switches FEATURE on or off, on this device.
///
/// Refuses for something this build does not have: there is nothing to
/// switch, and pretending otherwise would leave a setting that does
/// nothing and cannot be undone from the UI (which shows no switch for
/// it).
pub fn set_enabled(feature: &str, enabled: bool) -> Result<(), String> {
    let Some(spec) = spec(feature) else {
        return Err(format!("this build has no feature called {feature}"));
    };
    if !spec.compiled_in {
        return Err(format!("{feature} is not part of this build"));
    }

    let mut switches = SWITCHES.load();
    switches.disabled.retain(|id| id != feature);
    if !enabled {
        switches.disabled.push(feature.to_string());
    }
    switches.disabled.sort();
    SWITCHES.save(&switches)?;

    // Only after the write: a cache that says "on" over a file that says
    // "off" is the worse of the two failures.
    if let Ok(mut off) = disabled().write() {
        if enabled {
            off.remove(feature);
        } else {
            off.insert(feature.to_string());
        }
    }
    Ok(())
}

/// Re-reads the switches from disk. For a process that did not write
/// them itself -- the Android background worker runs without the app (see
/// `features::background`).
pub fn reload() {
    let stored: HashSet<String> = SWITCHES.load().disabled.into_iter().collect();
    match DISABLED.get() {
        Some(cache) => {
            if let Ok(mut off) = cache.write() {
                *off = stored;
            }
        }
        None => {
            let _ = DISABLED.set(RwLock::new(stored));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Registered the way a feature registers its own -- this module knows
    // no feature, and the mechanism has to work for whoever shows up. (A
    // comment, not a doc comment: `inventory::submit!` takes no docs.)
    inventory::submit! {
        FeatureSpec { id: "test-present", compiled_in: true, needs: &[], built_on: &[] }
    }
    inventory::submit! {
        FeatureSpec { id: "test-absent", compiled_in: false, needs: &[], built_on: &[] }
    }
    inventory::submit! {
        FeatureSpec { id: "test-dependent", compiled_in: true, needs: &["test-absent"], built_on: &[] }
    }

    #[test]
    fn a_feature_nobody_switched_off_is_on() {
        assert!(is_on("test-present"));
    }

    #[test]
    fn a_feature_this_build_does_not_have_is_off_whatever_the_file_says() {
        assert!(!is_on("test-absent"));
    }

    /// Switching off what something is built on switches it off too --
    /// otherwise CalDAV would keep writing calendar mirrors after the
    /// calendars were switched off.
    #[test]
    fn a_feature_is_only_as_on_as_what_it_needs() {
        assert!(!is_on("test-dependent"));
    }

    #[test]
    fn a_name_nobody_registered_is_not_on() {
        assert!(!is_on("gibt-es-nicht"));
    }

    #[test]
    fn switching_something_this_build_lacks_is_refused_rather_than_stored() {
        let error = set_enabled("test-absent", true).unwrap_err();
        assert!(error.contains("not part of this build"), "{error}");
    }

    #[test]
    fn every_registered_feature_has_an_id_of_its_own() {
        let ids: Vec<&str> = all().iter().map(|spec| spec.id).collect();
        let unique: HashSet<&&str> = ids.iter().collect();
        assert_eq!(unique.len(), ids.len(), "{ids:?}");
    }

    /// What it needs has to exist, or `is_on` answers false forever and
    /// the feature is quietly dead.
    #[test]
    fn everything_a_feature_needs_is_registered_too() {
        let known: HashSet<&str> = all().iter().map(|spec| spec.id).collect();
        for spec in all() {
            for needed in spec.needs {
                assert!(known.contains(needed), "{} needs {needed}, which nothing registers", spec.id);
            }
        }
    }

    /// And nothing stands on itself, however far around.
    ///
    /// Two features naming each other in `needs` is a plausible slip --
    /// "CalDAV needs the calendars, and the calendars need CalDAV to
    /// write them". Every registration is a `submit!` somewhere else, so
    /// nobody sees the pair; the graph only exists here. The test walks
    /// it rather than trusting the walk in `flatten_needs` to survive
    /// one.
    #[test]
    fn no_feature_stands_on_itself() {
        for spec in all() {
            let mut seen: HashSet<&str> = HashSet::new();
            let mut todo: Vec<&str> = spec.needs.to_vec();
            while let Some(id) = todo.pop() {
                assert_ne!(id, spec.id, "{} needs itself, in the end -- see its needs", spec.id);
                if !seen.insert(id) {
                    continue;
                }
                if let Some(needed) = all().into_iter().find(|other| other.id == id) {
                    todo.extend(needed.needs);
                }
            }
        }
    }

    /// The cheap part of the same story: what a feature stands on is
    /// worked out once, not on every question -- and `is_on` runs on
    /// every file write.
    #[test]
    fn what_a_feature_stands_on_is_resolved_once() {
        let dependent = registry().get("test-dependent").expect("registered above");

        assert_eq!(
            dependent.stands_on.as_ref().map(|specs| specs.iter().map(|spec| spec.id).collect::<Vec<_>>()),
            Some(vec!["test-absent"]),
        );
    }
}
