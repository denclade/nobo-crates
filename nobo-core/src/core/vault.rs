//! The vault, as a feature is allowed to see it.
//!
//! A feature needs to read what is in the vault -- the headings of a
//! file, which files declare themselves a cookbook, the notes and their
//! titles. Until now it reached into `IndexManager` and `org_parser` for
//! that, which are the core's own machinery: the index has a write half,
//! a lock, a watcher and a rescan, and the parser has a dozen entry
//! points for the editor's sake. Everything a feature touched there was
//! something that could not be changed afterwards.
//!
//! So this is the narrow door: a read-only view with the handful of
//! questions features actually ask, and nothing that writes. What is
//! behind it -- an in-memory index today, something else tomorrow -- is
//! the core's business.
//!
//! ```ignore
//! let vault = Vault::new(&index, &vault_root);
//! for (path, text) in vault.files_of_kind(cookbook::KIND) {
//!     // …
//! }
//! ```
//!
//! # What it is not
//!
//! - **Not a write path.** Changing the vault goes through the commands
//!   that own that (see `core::indexer::notify_file_changed`, which also
//!   tells the frontend). A view that could write would need to say when
//!   the index is stale, and that is the core's problem, not a feature's.
//! - **Not a cache to hold on to.** Every method reads the index as it is
//!   now. Take a `Vault` for the length of one command, not for the
//!   length of a session.
//! - **Not sorted.** Nothing here promises an order; a caller that needs
//!   one says so itself, because whichever it needs, another caller needs
//!   a different one.

use std::path::{Path, PathBuf};

use crate::core::file_kind::{self, FileKind};
use crate::core::indexer::IndexManager;
use crate::core::models::{Heading, Note};

pub struct Vault<'a> {
    index: &'a IndexManager,
    root: &'a Path,
}

impl<'a> Vault<'a> {
    pub fn new(index: &'a IndexManager, root: &'a Path) -> Self {
        Vault { index, root }
    }

    /// Where the vault is. For building a path INTO it -- reading it is
    /// what the methods below are for.
    pub fn root(&self) -> &Path {
        self.root
    }

    /// The headings of one file, parsed on the spot when the index has
    /// not seen it yet (a mirror a feature has just written, for
    /// instance). Empty for a file that is not there -- a missing file is
    /// a file with nothing in it, not an error to handle at every call
    /// site.
    pub fn headings(&self, path: &Path) -> Vec<Heading> {
        self.index.get_headings_or_parse(path)
    }

    /// Every note in the vault: its path, its title, what the sidebar
    /// shows. Notes, not files -- a mirror is not one.
    pub fn notes(&self) -> Vec<Note> {
        self.index.all_notes()
    }

    /// Every org file with its text, as the index already holds it.
    ///
    /// The text, because what a file IS lives inside it (see
    /// `core::file_kind`) -- `files_of_kind` is the same thing with the
    /// question already asked, and it is what you almost always want:
    /// this one hands out a copy of the whole vault, which on a search
    /// path is a copy per keystroke.
    pub fn org_files(&self) -> Vec<(PathBuf, String)> {
        self.collect(|_| true)
    }

    /// Every file that declares itself KIND, with its text.
    ///
    /// Only those files are copied -- the kind is read off each body
    /// while the index is still locked, so a vault of a thousand notes
    /// with three cookbooks in it costs three.
    pub fn files_of_kind(&self, kind: FileKind) -> Vec<(PathBuf, String)> {
        self.collect(move |text| file_kind::from_lines(&text.lines().collect::<Vec<_>>()) == kind)
    }

    /// Every file that is, or could be declared, a KIND file -- what a
    /// feature's "which files are mine" settings list offers. Out are the
    /// files the app generates (a mirror would be undone on its next
    /// refresh) and the app's own files in hidden folders (`.nobo/`: the
    /// GTD lists, the archive) unless they already are KIND files. Each
    /// with its text, its note title if it has one, and whether it is a
    /// KIND file now; in no particular order.
    pub fn kind_candidates(&self, kind: FileKind) -> Vec<KindCandidate> {
        let titles: std::collections::HashMap<String, String> = self.notes().into_iter().map(|note| (note.path, note.title)).collect();
        self.org_files()
            .into_iter()
            .filter_map(|(path, text)| {
                let found = file_kind::from_lines(&text.lines().collect::<Vec<_>>());
                let is_kind = found == kind;
                let hidden = crate::utils::fs_utils::has_hidden_ancestor(path.strip_prefix(self.root).unwrap_or(&path));
                if !file_kind::capabilities(found).editable || (hidden && !is_kind) {
                    return None;
                }
                let title = titles.get(path.to_string_lossy().as_ref()).cloned();
                Some(KindCandidate { path, text, title, is_kind })
            })
            .collect()
    }

    fn collect(&self, keep: impl Fn(&str) -> bool) -> Vec<(PathBuf, String)> {
        self.index.org_file_bodies_where(keep).into_iter().map(|(path, text)| (PathBuf::from(path), text)).collect()
    }
}

/// One file `Vault::kind_candidates` offers.
pub struct KindCandidate {
    pub path: PathBuf,
    pub text: String,
    /// Its note title, when the index knows it as a note.
    pub title: Option<String>,
    pub is_kind: bool,
}

/// Headings out of text that is not in the vault (yet).
///
/// For a feature that has just built a mirror file and wants to read back
/// what it wrote, without waiting for the index to catch up. Anything
/// already in the vault goes through `Vault::headings`.
pub fn parse_headings(path: &Path, text: &str) -> Vec<Heading> {
    crate::core::org_parser::parse_headings_from_lines(path, text.lines())
}

/// What one org heading line says, before anything is made of it:
/// What a heading line is made of -- see `org_parser::ParsedHeadingLine`.
///
/// A named struct, not a tuple. The alias this replaced described its own
/// last field as "priority" when it was in fact the statistics cookie, and
/// nothing could catch that.
pub use crate::core::org_parser::ParsedHeadingLine as HeadingLine;

/// A heading line as the app writes one: stars, title, and the tags
/// pushed out to org's tag column -- for a feature that writes headings
/// of its own (a bookmark) and should not look different from one the
/// app's own editor writes.
pub fn format_heading_line(level: usize, title: &str, tags: &[String]) -> String {
    crate::core::heading_utils::format_new_heading_line(level, &None, None, title, tags, None)
}

/// HEADING written back as a line -- with its TODO keyword, priority and
/// statistics cookie, which a feature that only changes the title or the
/// tags must not drop.
pub fn format_heading(heading: &HeadingLine) -> String {
    let title = match &heading.raw_cookie {
        Some(cookie) => format!("{} {cookie}", heading.title),
        None => heading.title.clone(),
    };
    crate::core::heading_utils::format_new_heading_line(
        heading.level,
        &heading.state,
        heading.priority.as_deref(),
        &title,
        &heading.tags,
        None,
    )
}

/// One org heading line.
///
/// For a feature that scans a file its own way (see
/// `nobo_cookbook::outline`) rather than taking the parsed headings.
pub fn parse_heading_line(line: &str) -> Option<HeadingLine> {
    crate::core::org_parser::parse_heading_line(line)
}

/// Which lines sit inside a closed code block (`#+BEGIN_SRC` ... `#+END_SRC`
/// and every other `#+BEGIN_<name>`), marker lines included.
///
/// For a feature that scans a file its own way: what is inside is text,
/// and a scan that reads a `*`-line in there as a heading disagrees with
/// the core about where things are -- which turns into a WRITE at the
/// wrong place the moment the scan decides where to insert something.
/// The rule itself is the core's (org_parser::closed_block_lines).
pub fn closed_block_lines<S: AsRef<str>>(lines: &[S]) -> Vec<bool> {
    crate::core::org_parser::closed_block_lines(lines)
}

/// One `:KEY: value` property line.
pub fn parse_property(line: &str) -> Option<(String, String)> {
    crate::core::org_parser::parse_property(line)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_heading_keeps_its_keyword_priority_and_cookie() {
        let mut heading = parse_heading_line("** TODO [#A] reqwest [1/2] :http:").unwrap();
        heading.tags.push("readlater".into());
        let line = format_heading(&heading);
        assert!(line.starts_with("** TODO [#A] reqwest [1/2]"), "{line}");
        assert!(line.ends_with(":http:readlater:"), "{line}");
    }

    const KIND: FileKind = FileKind::new("vault-test-kind");

    inventory::submit! {
        crate::core::file_kind::FileKindSpec {
            kind: KIND,
            served_by: None,
            editable: true,
            states: crate::core::file_kind::StateSupport::Full,
            reorderable: true,
            can_add_headings: true,
        }
    }

    fn vault_with(files: &[(&str, &str)]) -> (tempfile::TempDir, IndexManager) {
        let dir = tempfile::tempdir().unwrap();
        for (name, text) in files {
            std::fs::write(dir.path().join(name), text).unwrap();
        }
        let index = IndexManager::new();
        index.scan_directory(dir.path());
        (dir, index)
    }

    /// Candidates for a kind: ordinary notes and the KIND files, not the
    /// app's own files in `.nobo/`.
    #[test]
    fn kind_candidates_leave_out_the_apps_own_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".nobo")).unwrap();
        std::fs::write(dir.path().join("notiz.org"), "* A\n").unwrap();
        std::fs::write(dir.path().join(".nobo/actions.org"), "* TODO x\n").unwrap();
        std::fs::write(dir.path().join(".nobo/mine.org"), "#+NOBO_TYPE: vault-test-kind\n").unwrap();
        let index = IndexManager::new();
        index.scan_directory(dir.path());

        let mut found: Vec<(String, bool)> = Vault::new(&index, dir.path())
            .kind_candidates(KIND)
            .into_iter()
            .map(|c| (c.path.file_name().unwrap().to_string_lossy().into_owned(), c.is_kind))
            .collect();
        found.sort();
        assert_eq!(found, [("mine.org".to_string(), true), ("notiz.org".to_string(), false)]);
    }

    #[test]
    fn it_answers_with_the_headings_of_one_file() {
        let (dir, index) = vault_with(&[("notizen.org", "* TODO Milch\n** DONE Butter\n")]);
        let vault = Vault::new(&index, dir.path());

        let headings = vault.headings(&dir.path().join("notizen.org"));

        assert_eq!(headings.len(), 2);
        assert_eq!(headings[0].title, "Milch");
    }

    /// A file that is not there has nothing in it -- every caller would
    /// otherwise write the same `unwrap_or_default`.
    #[test]
    fn a_missing_file_has_no_headings() {
        let (dir, index) = vault_with(&[]);
        let vault = Vault::new(&index, dir.path());

        assert!(vault.headings(&dir.path().join("weg.org")).is_empty());
    }

    /// The question features actually ask: which files are mine?
    #[test]
    fn it_finds_the_files_of_one_kind() {
        let (dir, index) = vault_with(&[("meins.org", "#+NOBO_TYPE: vault-test-kind\n* Etwas\n"), ("notizen.org", "* Etwas anderes\n")]);
        let vault = Vault::new(&index, dir.path());

        let mine = vault.files_of_kind(KIND);

        assert_eq!(mine.len(), 1, "{mine:?}");
        assert!(mine[0].0.ends_with("meins.org"));
        assert!(mine[0].1.contains("Etwas"));
    }

    /// Text a feature has just written, before the index has seen it.
    #[test]
    fn it_parses_text_that_is_not_in_the_vault_yet() {
        let headings = parse_headings(Path::new("/vault/neu.org"), "* TODO Frisch geschrieben\n");

        assert_eq!(headings.len(), 1);
        assert_eq!(headings[0].title, "Frisch geschrieben");
    }
}
