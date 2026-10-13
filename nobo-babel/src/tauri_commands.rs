//! The same commands, as Tauri sees them.
//!
//! One line each where nothing blocks, and `spawn_blocking` around the
//! three that do: everything that decides anything lives in `commands`,
//! so the Tauri build and a non-Tauri host cannot answer the same
//! question differently. The names are what the frontend's generated
//! bindings call; so are the doc comments, which tauri-specta reads off
//! the command to write `bindings.ts` -- which is why they are the same
//! text as in `commands`: they describe the contract, and the frontend is
//! who reads them.

use std::sync::{Arc, Mutex};

use rusqlite::Connection;
use tauri::State;

use crate::commands;
use crate::model::{ExecuteBabelBlockPayload, ExecutionResult, InterpreterInfo, Settings};
use crate::SessionRegistry;

/// Runs `work` on a thread that may block, and waits for it there.
///
/// Everything in this plugin that starts a process goes through here. A
/// panic inside becomes an error rather than a lost answer: the caller is
/// a button that would otherwise spin forever.
async fn off_the_main_thread<T, F>(work: F) -> Result<T, String>
where
    F: FnOnce() -> Result<T, String> + Send + 'static,
    T: Send + 'static,
{
    tauri::async_runtime::spawn_blocking(work)
        .await
        .map_err(|e| format!("the interpreter could not be run: {e}"))?
}

#[tauri::command]
#[specta::specta]
pub fn babel_is_trusted(file_path: String, content: String) -> bool {
    commands::babel_is_trusted(file_path, content)
}

#[tauri::command]
#[specta::specta]
pub fn babel_trust_file(file_path: String, content: String) -> Result<(), String> {
    commands::babel_trust_file(file_path, content)
}

/// Runs one SRC block and answers with what it printed.
///
/// `async`, and that is not decoration: a plain `#[tauri::command]` runs
/// on the main thread, so a block that takes a second would freeze the
/// window for a second and one that loops forever would freeze it for
/// good -- including the part of the UI that would have to offer a way
/// out. The work itself is blocking (a process, two pipes, a wait), so
/// it goes to `spawn_blocking` rather than onto an async worker.
///
/// The second half of the same problem is in `exec`: the block is stopped
/// after `exec::TIMEOUT` and its process killed. Without that, moving off
/// the main thread would only trade a frozen window for an invisible
/// runaway interpreter.
#[tauri::command]
#[specta::specta]
pub async fn babel_execute_block(
    payload: ExecuteBabelBlockPayload,
    session_registry: State<'_, Arc<SessionRegistry>>,
) -> Result<ExecutionResult, String> {
    let registry = Arc::clone(&session_registry);
    off_the_main_thread(move || commands::babel_execute_block(payload, &registry)).await
}

#[tauri::command]
#[specta::specta]
pub fn babel_kill_session(
    file_path: String,
    session_name: String,
    session_registry: State<'_, Arc<SessionRegistry>>,
) {
    commands::babel_kill_session(file_path, session_name, &session_registry)
}

#[tauri::command]
#[specta::specta]
pub fn babel_discover_bash() -> Option<String> {
    commands::babel_discover_bash()
}

/// Every interpreter it can find, each asked for its `--version` -- a
/// dozen processes on a machine with several Pythons, so not on the main
/// thread either.
#[tauri::command]
#[specta::specta]
pub async fn babel_discover_python_interpreters() -> Result<Vec<InterpreterInfo>, String> {
    off_the_main_thread(commands::babel_discover_python_interpreters).await
}

/// Builds a virtualenv, which means `python -m venv` and a wait of
/// seconds to a minute -- off the main thread for the same reason as
/// running a block (see `babel_execute_block`).
#[tauri::command]
#[specta::specta]
pub async fn babel_create_venv(target_dir: String, python_path: Option<String>) -> Result<String, String> {
    off_the_main_thread(move || commands::babel_create_venv(target_dir, python_path)).await
}

/// `:tangle <file>` -- writes a single block's code out as a real source
/// file. Always overwrites the target (the .org file is the source of
/// truth; the tangled file is a regenerated artifact), so re-tangling
/// after editing the block produces the same result every time.
#[tauri::command]
#[specta::specta]
pub fn babel_tangle_write(
    target_path: String,
    code: String,
    // The note the block lives in, and its text -- what `trust::is_trusted`
    // is asked about. See the gate below.
    file_path: String,
    content: String,
    db: State<'_, Arc<Mutex<Connection>>>,
) -> Result<(), String> {
    commands::babel_tangle_write(target_path, code, file_path, content, db.inner())
}

/// Remembers DIRECTORY as a place `:tangle` may write, after the user
/// picked it in the operating system's own directory chooser.
///
/// The picker is the grant, not this command -- see `tangle_roots` for why
/// a prompt nobo draws itself would grant nothing under Flatpak and
/// everything on an unpackaged build. What this adds is the part the
/// picker cannot check on its own:
///
/// **TARGET_PATH must actually be inside DIRECTORY.** Otherwise a block
/// reading `:tangle ~/.bashrc` could open a chooser, the user could pick
/// `~/projekte` believing that is where the code goes, and the write would
/// land on `~/.bashrc` under a grant that never mentioned it. The chooser
/// answers "you may write here"; only this answers "and here is where it
/// was going to write".
///
/// Tauri's own asset and filesystem scopes are deliberately NOT widened
/// here, unlike for the vault (`nobo_tauri::permission_utils` at startup).
/// Those two scopes govern what the WEBVIEW may reach -- `readTextFile`,
/// the asset protocol -- and tangling needs neither: the write happens in
/// Rust through `std::fs`. Granting a folder so one code block may be
/// written there must not also hand every script in the renderer read and
/// write access to it.
#[tauri::command]
#[specta::specta]
pub fn babel_grant_tangle_root(directory: String, target_path: String) -> Result<(), String> {
    commands::babel_grant_tangle_root(directory, target_path)
}

/// Every folder outside the vault that tangling may write to -- for the
/// settings screen, and so a grant is reviewable rather than invisible.
#[tauri::command]
#[specta::specta]
pub fn babel_tangle_roots() -> Vec<String> {
    commands::babel_tangle_roots()
}

/// Takes a folder grant back.
///
/// Only nobo's own record of it: on an unsandboxed build the process
/// could always have written there, and under Flatpak the portal's
/// hand-over is the portal's to withdraw (Settings -> Applications, or
/// `flatpak permission-remove`). What this guarantees is that nobo will
/// not write there again without asking -- which is the decision the
/// user made here, and the reason the list is shown at all.
#[tauri::command]
#[specta::specta]
pub fn babel_revoke_tangle_root(directory: String) -> Result<(), String> {
    commands::babel_revoke_tangle_root(directory)
}

/// This feature's own settings, read and written on their own (see
/// nobo_core::config::section). Not part of `Config`: the general
/// settings have no business carrying an interpreter path, and a build
/// without org-babel has no business having one at all.
#[tauri::command]
#[specta::specta]
pub fn babel_settings() -> Settings {
    commands::babel_settings()
}

#[tauri::command]
#[specta::specta]
pub fn babel_save_settings(settings: Settings) -> Result<(), String> {
    commands::babel_save_settings(settings)
}
