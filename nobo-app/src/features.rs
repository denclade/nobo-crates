//! Which features this build has, and what its expensive operations
//! have done -- the three commands that are about the app itself rather
//! than about the vault.
//!
//! From the app's `commands/mod.rs`; the module list that stood above
//! them is `lib.rs`'s now.

/// One feature, as the app has to talk about it.
///
/// Three states, not two: in the build and on, in the build and switched
/// off, or not in this build at all. The settings page shows a switch for
/// the first two and nothing for the third -- there is nothing to switch
/// (see core::features, and the iOS/org-babel case in nobo_babel).
#[derive(Debug, Clone, serde::Serialize, specta::Type)]
pub struct FeatureState {
    /// Its cargo feature name, which is also its id in the frontend
    /// manifest.
    pub id: String,
    /// Whether this build compiled it in. Fixed at build time.
    pub compiled_in: bool,
    /// Whether it is on right now: compiled in, not switched off, and
    /// everything it needs is on too.
    pub enabled: bool,
    /// What it is built on. A sync provider needs the sync, so switching
    /// the sync off takes its providers with it -- the UI says so rather
    /// than leaving a switch that appears to do nothing.
    pub needs: Vec<String>,
    /// The shared libraries it brings into the build (`nobo-http`,
    /// `nobo-webdav`, ...). Not features and not switchable -- shown
    /// because "what does this add to my binary" is a fair question, and
    /// the switch list is where somebody asks it.
    pub built_on: Vec<String>,
}

/// What this build has, and what is switched on right now.
///
/// The frontend needs this because the commands of a feature that is not
/// available are still there and still answer (see e.g.
/// `nobo_babel`); what must not happen is offering a button whose
/// only possible answer is "not here". Platform facts -- a phone too
/// narrow for the editor toolbar, an OS without process spawning -- are a
/// separate question and stay in store/capabilities.ts.
pub fn app_features() -> Vec<FeatureState> {
    nobo_core::core::features::all()
        .into_iter()
        .map(|spec| FeatureState {
            id: spec.id.to_string(),
            compiled_in: spec.compiled_in,
            enabled: nobo_core::core::features::is_on(spec.id),
            needs: spec.needs.iter().map(|needed| needed.to_string()).collect(),
            built_on: spec.built_on.iter().map(|library| library.to_string()).collect(),
        })
        .collect()
}

/// What the expensive operations of this build have done since the app
/// started (or since the last reset).
///
/// Every heavy operation declares itself where it happens -- what it does,
/// how it scales, and how often it is supposed to run (see
/// `core::cost`). This hands that list to the frontend together with the
/// counts, so the dev bar can show "the calendar refresh ran 50 times in
/// the last minute" next to the sentence that says it should run once.
///
/// Not a profiler: a counter and a stopwatch per operation. Cheap enough
/// to ask at any time, and it answers even when nothing has run -- a
/// declared operation with zero runs is an answer too.
pub fn heavy_operations() -> Vec<nobo_core::core::cost::HeavyReport> {
    nobo_core::core::cost::report()
}

/// Starts the counting over, so the next question can be "what did THIS
/// action cost" rather than "what has this session cost".
pub fn reset_heavy_operations() -> Vec<nobo_core::core::cost::HeavyReport> {
    nobo_core::core::cost::reset();
    nobo_core::core::cost::report()
}

/// Switches one feature on or off on this device.
///
/// Nothing is deleted either way: the feature keeps its settings section,
/// its files and its accounts, and switching it back on is not an import
/// (see core::features and core::migrations, which run for switched-off
/// features too).
pub fn set_feature_enabled(feature: String, enabled: bool) -> Result<Vec<FeatureState>, String> {
    nobo_core::core::features::set_enabled(&feature, enabled)?;
    // The whole list back, not just the one: switching off something
    // others are built on changes their answer too.
    Ok(app_features())
}
