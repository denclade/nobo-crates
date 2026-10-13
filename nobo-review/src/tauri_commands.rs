//! The same commands, as Tauri sees them.
//!
//! One line each on purpose: everything that decides anything lives in
//! `commands`, so the Tauri build and a non-Tauri host cannot answer the
//! same question differently. The names are what the frontend's generated
//! bindings call; so are the doc comments, which tauri-specta reads off
//! the command to write `bindings.ts` -- which is why they are the same
//! text as in `commands`: they describe the contract, and the frontend is
//! who reads them.

use crate::commands;
use crate::state::ReviewState;

/// No vault configured yet (fresh install, still on Setup) reads as "never
/// reviewed" rather than an error -- same reasoning as `state::load`'s
/// own missing-file case, just one layer further out.
#[tauri::command]
#[specta::specta]
pub fn review_state() -> ReviewState {
    commands::review_state()
}

#[tauri::command]
#[specta::specta]
pub fn review_save_state(state: ReviewState) -> Result<(), String> {
    commands::review_save_state(state)
}

/// How often the review is due on this device (see `crate::Settings`).
#[tauri::command]
#[specta::specta]
pub fn review_settings() -> crate::Settings {
    commands::review_settings()
}

#[tauri::command]
#[specta::specta]
pub fn review_save_settings(settings: crate::Settings) -> Result<(), String> {
    commands::review_save_settings(settings)
}
