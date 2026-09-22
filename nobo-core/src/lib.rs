//! Everything the app is made of that is not a feature.
//!
//! Its own crate for one reason: what used to be a rule is now a compile
//! error. `tools/check-layers-rust.mjs` could only say "the core must not
//! use a feature" after the fact; a crate that does not depend on the app
//! cannot, because the names are not in scope at all.
//!
//! The split was free to make, and that is the interesting part: the rule
//! had been enforced long enough that `core/`, `config/` and `utils/`
//! turned out to have exactly zero references into `commands/` or
//! `features/` -- the directories moved without a single import changing.
//!
//! The app re-exports these three (`pub use nobo_core::{config, core,
//! utils}` in nobo's lib.rs), so every `crate::core::…` path inside the
//! app still resolves. A feature crate says `nobo_core::core::…` instead,
//! which is the whole point: it can only reach what this crate offers.

pub mod config;
pub mod core;
pub mod utils;
