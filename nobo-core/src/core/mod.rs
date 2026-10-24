// SPDX-FileCopyrightText: 2026 Dennis Burgermeister
// SPDX-License-Identifier: GPL-3.0-or-later

pub mod edna;
pub mod errors;
pub mod heading_identity;
pub mod heading_utils;
pub mod history;
pub mod i18n;
pub mod indexer;
pub mod models;
pub mod org_parser;
pub mod org_syntax;
pub mod refile;
pub mod search;
pub mod task_utils;
// What the frontend used to compute and a second front end would have had
// to reimplement: the agenda, and the filter grammar (see each module).
pub mod agenda;
pub mod filter;
// The org grammar the frontend used to keep to itself (see org_ast).
pub mod app_files;
pub mod clock;
pub mod completion;
pub mod cost;
pub mod db;
pub mod expansions;
pub mod favorites;
pub mod features;
pub mod file_kind;
pub mod links;
pub mod merge;
pub mod migrations;
pub mod notifications;
pub mod once_at_a_time;
pub mod org_ast;
pub mod org_convert;
pub mod org_edit;
pub mod org_highlight;
pub mod org_insert;
pub mod org_sort;
pub mod org_table;
pub mod secrets;
pub mod vault;
pub mod vault_write;
