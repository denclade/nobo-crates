// SPDX-FileCopyrightText: 2026 Dennis Burgermeister
// SPDX-License-Identifier: GPL-3.0-or-later

//! Heading read, write and batch logic, split by concern and re-exported
//! flat here, so `heading_utils::X` call sites do not care about the
//! split.
mod batch;
mod edit;
mod serialize;
mod tree;

pub use batch::*;
pub use edit::*;
pub use serialize::*;
pub use tree::*;

#[cfg(test)]
mod tests;
