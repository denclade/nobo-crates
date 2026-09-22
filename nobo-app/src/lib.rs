//! What nobo's commands do, without a host.
//!
//! Every function here was the body of a `#[tauri::command]` in
//! `src/commands/*.rs`. The bodies are unchanged; what went away is how
//! the host hands its state in -- `State<'_, Arc<IndexManager>>` became
//! `&IndexManager`, `State<'_, Arc<Mutex<Connection>>>` became
//! `&Mutex<Connection>`, and the one `AppHandle` became an
//! [`IndexListener`](nobo_core::core::indexer::IndexListener).
//!
//! That is the whole point of the crate: a second front end (harbour-nobo,
//! Qt/Silica on Sailfish OS) needs these bodies and cannot compile Tauri.
//! The Tauri commands become wrappers -- unpack `State`, call the function
//! here -- so there is one implementation and two callers rather than two
//! implementations.
//!
//! Three things stayed with the host on purpose, because they are the
//! host's and not nobo's: widening Tauri's asset/fs scopes
//! (`permission_utils`), writing an image to the clipboard (only its
//! decoder is here, see `commands_notes`), and resolving the mobile notes
//! directory from a platform path resolver.

pub mod commands_config;
pub mod commands_fs;
pub mod commands_headings;
pub mod commands_history;
pub mod commands_notes;
pub mod commands_org;
pub mod commands_search;
pub mod features;

/// What a host has to do before a new vault directory is read from.
///
/// Tauri's asset and fs scopes are per-directory and have to be widened
/// to the vault first (see `nobo_core::utils::permission_utils`, which
/// only the Tauri wrapper calls); a host with no scopes to widen passes
/// [`NoScope`] and nothing happens. A seam rather than an `AppHandle`
/// because it is the one thing `save_config` does that is the host's.
pub trait VaultScope {
    fn allow(&self, dir: &std::path::Path) -> Result<(), String>;
}

/// The scope for a host that has none.
pub struct NoScope;

impl VaultScope for NoScope {
    fn allow(&self, _dir: &std::path::Path) -> Result<(), String> {
        Ok(())
    }
}

/// Runs a long, blocking piece of work.
///
/// These bodies were written for Tauri's async commands, where a pass over
/// the whole vault had to be handed off (`tokio::task::block_in_place`) so
/// it would not sit on an async worker for its whole duration. Whether
/// that is needed is the host's question, not this crate's: the Tauri
/// wrapper puts `block_in_place` back around the four calls that had it,
/// and a host that already calls from a thread of its own
/// (harbour-nobo's `nobo_call`) just runs the work.
///
/// Kept as a named function rather than deleted, so the call sites still
/// say "this is the expensive part" where they always did.
pub fn blocking<T>(work: impl FnOnce() -> T) -> T {
    work()
}
