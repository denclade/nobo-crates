// SPDX-FileCopyrightText: 2026 Dennis Burgermeister
// SPDX-License-Identifier: GPL-3.0-or-later

//! What this plugin can be asked to do, without a host.
//!
//! Plain functions: no `#[tauri::command]`, no `State<...>`, no
//! `AppHandle`. The injected state arrives as a borrow instead
//! (`&SessionRegistry`, `&Mutex<Connection>`), and whatever runs the
//! blocking parts off the main thread is the caller's business -- see
//! `tauri_commands`, which is the Tauri half, and `exec` for the timeout
//! that keeps a runaway interpreter from outliving it.
//!
//! One body each, now that this is a crate of its own: a build without
//! org-babel does not compile it, so there is no "not in this build" half
//! to write. What used to be that answer is the absence of the plugin --
//! and the frontend never asks, because `app_features()` says it is not
//! here (see nobo_core::core::features).

use rusqlite::Connection;
use std::path::Path;
use std::sync::Mutex;

use crate::model::BabelLanguage;
use crate::model::{ExecuteBabelBlockPayload, ExecutionResult, InterpreterInfo, Settings};
use crate::SessionRegistry;
use crate::{exec, interpreters, trust};
use nobo_core::core::history::record_best_effort;
use nobo_core::core::models::{FileSnapshot, HistoryEntryKind};
use nobo_core::utils::fs_utils::filename_of;
use nobo_core::utils::ToCmdResult;

pub fn babel_is_trusted(file_path: String, content: String) -> bool {
    trust::is_trusted(&file_path, &content)
}

pub fn babel_trust_file(file_path: String, content: String) -> Result<(), String> {
    trust::trust(&file_path, &content).to_cmd()
}

/// Runs one SRC block and answers with what it printed.
///
/// **This blocks** -- a process, two pipes and a wait -- and it is the
/// caller's job to be somewhere that may. Under Tauri that is
/// `spawn_blocking` in `tauri_commands`, because a plain command runs on
/// the main thread: a block that takes a second would freeze the window
/// for a second, and one that loops forever would freeze it for good,
/// including the part of the UI that would have to offer a way out. A
/// host that already calls from a thread of its own just calls this.
///
/// The second half of the same problem is in `exec`: the block is stopped
/// after `exec::TIMEOUT` and its process killed. Without that, moving off
/// the main thread would only trade a frozen window for an invisible
/// runaway interpreter.
pub fn babel_execute_block(payload: ExecuteBabelBlockPayload, session_registry: &SessionRegistry) -> Result<ExecutionResult, String> {
    let settings = crate::SETTINGS.load();
    let interpreter_path = match payload.language {
        BabelLanguage::Python => settings.interpreter_path,
        BabelLanguage::Bash => settings.bash_path,
    };

    let vars: Vec<(String, String)> = payload.vars.into_iter().map(|v| (v.key, v.value)).collect();
    let code = exec::build_code(payload.language, &vars, &payload.code);

    // Where the note is. A session takes the same directory from its own
    // key (see SessionRegistry::send), so both paths resolve a relative
    // path the same way -- which is the whole point.
    let working_dir = Path::new(&payload.file_path).parent().filter(|p| !p.as_os_str().is_empty()).map(|p| p.to_path_buf());

    match payload.session {
        Some(session_name) => {
            let key = (payload.file_path, session_name);
            session_registry.send(key, payload.language, interpreter_path.as_deref(), &code).to_cmd()
        }
        None => exec::run(payload.language, interpreter_path.as_deref(), &code, working_dir.as_deref(), payload.use_cache).to_cmd(),
    }
}

pub fn babel_kill_session(file_path: String, session_name: String, session_registry: &SessionRegistry) {
    session_registry.kill(&(file_path, session_name));
}

pub fn babel_discover_bash() -> Option<String> {
    interpreters::discover_shell().map(|p| p.to_string_lossy().into_owned())
}

/// Every interpreter it can find, each asked for its `--version` -- a
/// dozen processes on a machine with several Pythons, so this blocks for
/// the same reason `babel_execute_block` does.
pub fn babel_discover_python_interpreters() -> Result<Vec<InterpreterInfo>, String> {
    Ok(interpreters::discover_pythons())
}

/// Builds a virtualenv, which means `python -m venv` and a wait of
/// seconds to a minute -- blocking for the same reason as running a
/// block (see `babel_execute_block`).
pub fn babel_create_venv(target_dir: String, python_path: Option<String>) -> Result<String, String> {
    let path = interpreters::create_venv(Path::new(&target_dir), python_path.as_deref()).to_cmd()?;
    Ok(path.to_string_lossy().into_owned())
}

/// `:tangle <file>` -- writes a single block's code out as a real source
/// file. Always overwrites the target (the .org file is the source of
/// truth; the tangled file is a regenerated artifact), so re-tangling
/// after editing the block produces the same result every time.
pub fn babel_tangle_write(
    target_path: String,
    code: String,
    // The note the block lives in, and its text -- what `trust::is_trusted`
    // is asked about. See the gate below.
    file_path: String,
    content: String,
    db: &Mutex<Connection>,
) -> Result<(), String> {
    let path = Path::new(&target_path);

    // The same gate running a block has to pass.
    //
    // It had none: the Tangle button was built from the block's own
    // `:tangle` header and wrote wherever that pointed, while `run` next
    // to it asked first. A shared vault could carry
    // `#+begin_src bash :tangle /home/<user>/.bashrc`, and one click on a
    // button labelled only "Tangle" destroyed that file and left code that
    // runs at the next shell start.
    //
    // Checked in the backend, not in the caller: `target_path` comes from
    // the frontend, so a check up there is decoration.
    if !crate::trust::is_trusted(&file_path, &content) {
        return Err(nobo_core::t!(
            "Diese Notiz ist nicht als vertrauenswürdig markiert -- Tangle schreibt keine Datei, bevor der Code nicht freigegeben wurde.",
            "This note is not marked as trusted -- Tangle writes no file until its code has been approved."
        ));
    }

    // And it goes somewhere the user has chosen.
    //
    // `resolveTanglePath` accepts an absolute path on purpose (org-babel
    // compatibility, pinned by its own tests), so this is where the line
    // is drawn instead. Trust means "this code may run", which is already
    // a lot; it should not silently also mean "and may replace any file on
    // this machine".
    //
    // Two places qualify: the vault, chosen during setup, and a folder
    // granted since through the OS picker (see `tangle_roots`, which
    // explains why the picker and not a modal of our own is what grants
    // it). Enforced HERE and not in the caller for the same reason as the
    // trust gate above -- `target_path` comes from the frontend.
    let vault_root = nobo_core::config::settings::Config::require_vault_root()?;
    let target = nobo_core::utils::fs_utils::canonicalize_or_self(path);
    let in_vault = crate::tangle_roots::is_inside(&target, &vault_root);
    if !in_vault && !crate::tangle_roots::is_granted(&target) {
        return Err(needs_folder_grant(&target));
    }

    // From here the write can still be refused by the OPERATING SYSTEM,
    // which outside the vault is a different kind of failure -- see
    // `grant_went_stale`.
    let report = |e: std::io::Error| {
        if grant_went_stale(in_vault, &e) {
            needs_folder_grant(&target)
        } else {
            e.to_string()
        }
    };

    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(report)?;
        }
    }
    let before = std::fs::read_to_string(path).ok();
    // Tangling replaces a file the user keeps: same guard and same
    // atomicity as any other whole-content write.
    nobo_core::utils::fs_utils::write_atomic_checked(path, &code).map_err(report)?;

    record_best_effort(
        db,
        HistoryEntryKind::TangleWrite { filename: filename_of(path) },
        false,
        vec![FileSnapshot { path: target_path, before, after: Some(code) }],
    );

    Ok(())
}

/// Whether a failed write means the folder grant is no longer backed by
/// the operating system, rather than that something went wrong.
///
/// nobo remembers a grant; the capability behind it belongs to the
/// sandbox. Under Flatpak the folder was opened to the app by a portal,
/// and if that hand-over does not survive a restart the write comes back
/// EACCES against a folder nobo still lists as granted. Answering the
/// same question again -- one more trip through the picker -- is
/// something the user can act on; "permission denied" is not.
///
/// Inside the vault a permission problem is a real problem and stays one:
/// the vault is not portal-granted, so nothing about picking it again
/// would help, and offering to would be a lie about the cause.
fn grant_went_stale(in_vault: bool, error: &std::io::Error) -> bool {
    !in_vault && error.kind() == std::io::ErrorKind::PermissionDenied
}

/// The refusal that is a question: this target needs a folder the user
/// has picked.
///
/// Prefixed with a stable, unlocalised marker so the frontend can offer
/// the folder chooser instead of showing a failure -- the same shape as
/// nobo_sync's `sync-index-missing:`. The sentence after it is localised;
/// the marker never is.
fn needs_folder_grant(target: &Path) -> String {
    format!(
        "tangle-outside-vault: {}",
        nobo_core::t!(
            "Das Tangle-Ziel liegt außerhalb des Tresors: {} -- dafür muss der Ordner erst freigegeben werden.",
            "The tangle target is outside the vault: {} -- that folder has to be allowed first.",
            target.display()
        )
    )
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
/// here, unlike for the vault (`nobo_tauri::permission_utils`
/// at startup). Those two scopes govern what the WEBVIEW may reach --
/// `readTextFile`, the asset protocol -- and tangling needs neither: the
/// write happens in Rust through `std::fs`. Granting a folder so one code
/// block may be written there must not also hand every script in the
/// renderer read and write access to it.
pub fn babel_grant_tangle_root(directory: String, target_path: String) -> Result<(), String> {
    let directory = Path::new(&directory);
    let target = Path::new(&target_path);

    if !crate::tangle_roots::is_inside(target, directory) {
        return Err(nobo_core::t!(
            "Der gewählte Ordner enthält das Tangle-Ziel nicht ({}) -- es wurde nichts freigegeben und nichts geschrieben.",
            "The chosen folder does not contain the tangle target ({}) -- nothing was allowed and nothing was written.",
            nobo_core::utils::fs_utils::canonicalize_or_self(target).display()
        ));
    }

    crate::tangle_roots::grant(directory)
}

/// Every folder outside the vault that tangling may write to -- for the
/// settings screen, and so a grant is reviewable rather than invisible.
pub fn babel_tangle_roots() -> Vec<String> {
    crate::tangle_roots::granted()
}

/// Takes a folder grant back.
///
/// Only nobo's own record of it: on an unsandboxed build the process
/// could always have written there, and under Flatpak the portal's
/// hand-over is the portal's to withdraw (Settings -> Applications, or
/// `flatpak permission-remove`). What this guarantees is that nobo will
/// not write there again without asking -- which is the decision the
/// user made here, and the reason the list is shown at all.
pub fn babel_revoke_tangle_root(directory: String) -> Result<(), String> {
    crate::tangle_roots::revoke(Path::new(&directory))
}

/// This feature's own settings, read and written on their own (see
/// nobo_core::config::section). Not part of `Config`: the general
/// settings have no business carrying an interpreter path, and a build
/// without org-babel has no business having one at all.
pub fn babel_settings() -> Settings {
    crate::SETTINGS.load()
}

pub fn babel_save_settings(settings: Settings) -> Result<(), String> {
    crate::SETTINGS.save(&settings)
}

#[cfg(test)]
mod tangle_grant_tests {
    use super::*;
    use tempfile::TempDir;

    fn denied() -> std::io::Error {
        std::io::Error::from(std::io::ErrorKind::PermissionDenied)
    }

    /// The case the Flatpak worry comes down to: a folder nobo still
    /// lists as granted that the sandbox no longer opens. The user gets
    /// the question again, not an error they cannot act on.
    #[test]
    fn a_refused_write_outside_the_vault_asks_for_the_folder_again() {
        assert!(grant_went_stale(false, &denied()));
    }

    /// And the three things that are NOT that.
    #[test]
    fn every_other_failure_stays_a_failure() {
        assert!(!grant_went_stale(true, &denied()), "the vault is not portal-granted");
        assert!(
            !grant_went_stale(false, &std::io::Error::from(std::io::ErrorKind::NotFound)),
            "a missing folder is not a missing permission"
        );
        assert!(
            !grant_went_stale(false, &std::io::Error::from(std::io::ErrorKind::StorageFull)),
            "a full disk is not a missing permission"
        );
    }

    /// The check the folder chooser cannot make for itself, and the whole
    /// reason this command exists rather than the frontend writing the
    /// grant: a block naming `~/.bashrc` must not become writable because
    /// the user picked the folder they thought the code was going to.
    #[test]
    fn picking_a_folder_that_does_not_contain_the_target_grants_nothing() {
        let home = TempDir::new().unwrap();
        let projekte = home.path().join("projekte");
        std::fs::create_dir_all(&projekte).unwrap();
        let bashrc = home.path().join(".bashrc");

        let refused = babel_grant_tangle_root(projekte.to_string_lossy().into_owned(), bashrc.to_string_lossy().into_owned());

        assert!(refused.is_err(), "granting must be refused: {refused:?}");
        assert!(
            !crate::tangle_roots::granted().iter().any(|d| d == &projekte.to_string_lossy()),
            "and nothing may be written to the store"
        );
    }
}
