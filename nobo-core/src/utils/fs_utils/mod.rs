// SPDX-FileCopyrightText: 2026 Dennis Burgermeister
// SPDX-License-Identifier: GPL-3.0-or-later

//! File system helpers, by what they are for. Everything is re-exported
//! here, so `fs_utils::write_atomic` and friends keep their names.

mod attachments;
mod listing;
mod paths;
#[cfg(test)]
mod tests;
mod write;

pub use attachments::*;
pub use listing::*;
pub use paths::*;
pub use write::*;
pub(crate) use write::{refuse_unwritable_kind_of, write_checked};
