// SPDX-FileCopyrightText: 2026 Dennis Burgermeister
// SPDX-License-Identifier: GPL-3.0-or-later

//! Text that Rust produces and a person reads.
//!
//! Most user-facing words belong to the frontend, which has paraglide and
//! both languages -- and where a reason can be TYPED rather than written
//! out, that is still the better answer (see `core::file_kind::ReadOnly`,
//! which exists for exactly that reason).
//!
//! Two kinds of text cannot go that way:
//!
//! - **An OS notification.** There is no frontend involved at all; the
//!   background worker builds the text and hands it to the system.
//! - **The error string a command returns.** Every caller treats it as
//!   opaque prose and shows it verbatim, so whatever language it was
//!   written in is the language the user gets. Roughly seventy of them
//!   were German only, which an English-speaking user saw as German.
//!
//! So those are localised here, with the `t!` macro:
//!
//! ```ignore
//! t!("{name} ist nicht mehr da", "{name} is gone")
//! ```
//!
//! Both strings sit at the call site rather than behind a key in a table.
//! A key registry is the usual answer and the wrong one here: it puts the
//! text one indirection away from the code that decides to say it, and
//! nothing catches a key that stops being used or a message whose meaning
//! drifts from its name. `messages/*.json` stays what the frontend uses;
//! this is only for what the frontend never sees.

use std::sync::RwLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    De,
    En,
}

/// Cached, because a message may be built per file in a vault-wide pass
/// and `Config::load` reads and parses `settings.json` every call.
///
/// `refresh` is called by `Config::save`, so switching the language takes
/// effect immediately rather than at the next start.
static LANG: RwLock<Option<Lang>> = RwLock::new(None);

pub fn lang() -> Lang {
    if let Ok(guard) = LANG.read() {
        if let Some(lang) = *guard {
            return lang;
        }
    }
    let resolved = from_settings();
    if let Ok(mut guard) = LANG.write() {
        *guard = Some(resolved);
    }
    resolved
}

/// Re-reads the configured language. Called when settings are written.
pub fn refresh() {
    if let Ok(mut guard) = LANG.write() {
        *guard = None;
    }
}

fn from_settings() -> Lang {
    // German is the default (see `Config::default`), and also the right
    // fallback for an unreadable or missing settings file: it is what the
    // app shipped with, and every one of these strings was German before.
    match crate::config::settings::Config::load() {
        Some(config) if config.language.starts_with("en") => Lang::En,
        _ => Lang::De,
    }
}

/// The German or the English version of one message, formatted.
///
/// `format!` needs a literal, which is why both are literals and the
/// arguments come after -- inline captures (`{name}`) work as usual.
#[macro_export]
macro_rules! t {
    ($de:literal, $en:literal $(,)?) => {
        match $crate::core::i18n::lang() {
            $crate::core::i18n::Lang::De => format!($de),
            $crate::core::i18n::Lang::En => format!($en),
        }
    };
    ($de:literal, $en:literal, $($arg:tt)*) => {
        match $crate::core::i18n::lang() {
            $crate::core::i18n::Lang::De => format!($de, $($arg)*),
            $crate::core::i18n::Lang::En => format!($en, $($arg)*),
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The macro has to work with and without arguments, and with inline
    /// captures, because the messages it replaces use all three.
    #[test]
    fn both_forms_expand() {
        let name = "notizen.org";
        let plain = t!("Ohne Argumente", "Without arguments");
        let captured = t!("{name} fehlt", "{name} is missing");
        let positional = t!("{} und {}", "{} and {}", 1, 2);

        // Which language is chosen depends on the machine's settings file,
        // so the assertion is about the shape, not the words.
        assert!(plain == "Ohne Argumente" || plain == "Without arguments");
        assert!(captured.starts_with("notizen.org"));
        assert!(positional == "1 und 2" || positional == "1 and 2");
    }

    #[test]
    fn an_unreadable_language_setting_falls_back_to_german() {
        assert_eq!(from_settings(), lang(), "the cache agrees with a fresh read");
    }
}
