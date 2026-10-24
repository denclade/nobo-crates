// SPDX-FileCopyrightText: 2026 Dennis Burgermeister
// SPDX-License-Identifier: GPL-3.0-or-later

//! What a SRC-block execution looks like on the wire.
//!
//! Compiled in every build, babel or not: the commands next door keep
//! their signatures either way, so one `bindings.ts` describes every
//! build (see the module doc in `mod.rs`).

use serde::{Deserialize, Serialize};

/// Languages SRC-block execution currently understands. Deliberately not a
/// free-form string: the execution engine has real per-language branches
/// (how to invoke the interpreter, how to format a `:var` assignment), so
/// an unsupported language should be a compile-time-checked non-option in
/// Rust, and a clear "not supported yet" in the UI rather than a silent
/// no-op.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "tauri", derive(specta::Type))]
#[serde(rename_all = "lowercase")]
pub enum BabelLanguage {
    Bash,
    Python,
}

impl BabelLanguage {
    /// Maps a SRC block's language token (`#+BEGIN_SRC python`, `sh`, ...)
    /// to a supported engine, or `None` if execution isn't offered for it.
    pub fn from_block_language(language: &str) -> Option<Self> {
        match language.to_lowercase().as_str() {
            "bash" | "sh" | "shell" => Some(Self::Bash),
            "python" | "python3" | "py" => Some(Self::Python),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "tauri", derive(specta::Type))]
pub struct ExecutionResult {
    pub stdout: String,
    pub stderr: String,
    #[cfg_attr(feature = "tauri", specta(type = specta_typescript::Number))]
    pub exit_code: i32,
    pub from_cache: bool,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "tauri", derive(specta::Type))]
pub struct InterpreterInfo {
    pub path: String,
    pub version: Option<String>,
}

/// Single `:var` assignment, already split/typed on the frontend (which
/// owns the header-arg text and any quoting/splitting of e.g.
/// `:var x=1 y=2`) -- this side only needs the resolved key/value pairs.
#[derive(Debug, serde::Deserialize)]
#[cfg_attr(feature = "tauri", derive(specta::Type))]
pub struct BabelVar {
    pub key: String,
    pub value: String,
}

#[derive(Debug, serde::Deserialize)]
#[cfg_attr(feature = "tauri", derive(specta::Type))]
pub struct ExecuteBabelBlockPayload {
    pub language: BabelLanguage,
    pub code: String,
    pub vars: Vec<BabelVar>,
    /// `:session <name>` -- `None` runs a fresh one-shot process.
    pub session: Option<String>,
    /// Owning file's path, used as half of the session key so blocks in
    /// different files never accidentally share a session.
    pub file_path: String,
    /// `:cache yes` -- ignored when `session` is set (a session is
    /// inherently stateful, "skip if unchanged" doesn't apply to it).
    pub use_cache: bool,
}

/// What this feature keeps in `settings.json`, under its own key.
///
/// Its own section rather than a field in `Config` (see
/// `config::section`): the interpreter is babel's business, a build
/// without babel has no reason to carry the field, and saving the general
/// settings has no reason to be able to change it.
/// Named `Settings` here and `BabelSettings` on the wire (the `serde`
/// rename is what specta reads for the exported type name; it does not
/// touch the JSON keys): inside the feature the short name is right, but
/// `bindings.ts` is one flat namespace that caldav and sync are about to
/// add their own settings to.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "tauri", derive(specta::Type))]
#[serde(rename = "BabelSettings")]
pub struct Settings {
    /// The Python interpreter to run blocks with. `None` means "look for
    /// one" (see interpreters::discover_pythons).
    pub interpreter_path: Option<String>,
    /// The shell to run `bash`/`sh`/`shell` blocks with. `None` means
    /// "look for one" (see interpreters::discover_bash), which is what
    /// every install started out doing -- so it needs a serde default,
    /// and an absent key must keep meaning auto.
    ///
    /// Settable for the same reason the Python one is: the shell on PATH
    /// is not always the shell the user's scripts are written for (a Nix
    /// profile, a Homebrew bash 5 next to macOS's bash 3, busybox `sh` on
    /// a minimal system).
    #[serde(default)]
    pub bash_path: Option<String>,
}
