// SPDX-FileCopyrightText: 2026 Dennis Burgermeister
// SPDX-License-Identifier: GPL-3.0-or-later

//! The heading commands -- the bodies the app's Tauri commands (and any
//! other host) call, split by what they do. Everything public is
//! re-exported here, so callers keep naming `commands_headings::*`.

mod archive;
mod edit;
mod maintenance;
mod read;
mod refile;
#[cfg(test)]
mod tests;

pub use archive::*;
pub use edit::*;
pub use maintenance::*;
pub use read::*;
pub use refile::*;
