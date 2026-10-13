//! What a vault file IS, and what may be done with it.
//!
//! Most org files in a vault are just notes. A few are something more
//! specific -- a read-only mirror of a subscribed ICS calendar, a task
//! list kept in step with a CalDAV server -- and those need different
//! affordances: no editing here, only a checkbox there, a dedicated view
//! somewhere else.
//!
//! The kind is declared IN the file:
//!
//! ```org
//! #+NOBO_TYPE: caldav-list
//! ```
//!
//! In the file, not in a config entry or a path convention, for three
//! reasons: it survives the vault being synced to another device, it
//! survives the file being moved, and it is visible to a human opening the
//! vault in Emacs. Org ignores unknown `#+` keywords, so the file stays
//! completely valid org either way.
//!
//! # Who knows about which kinds
//!
//! Nobody, here. This module owns the mechanism -- how a kind is
//! declared, found and written -- and exactly one kind, `Notes`, because
//! "an ordinary org file" is not a feature. Every other kind is
//! contributed by whatever feature owns it:
//!
//! ```ignore
//! // features/caldav/mod.rs
//! pub const KIND: FileKind = FileKind::new("caldav-list");
//! inventory::submit! {
//!     FileKindSpec { kind: KIND, editable: true, states: StateSupport::DoneOnly, .. }
//! }
//! ```
//!
//! That is the whole registration. `inventory` collects the submissions
//! at link time, so there is no startup call anyone can forget and no
//! order to get right -- which matters because this is asked on the write
//! path, from tests, and from a background worker that never runs the
//! app's setup. The price is that the registry is assembled by the
//! linker; `every_declared_kind_is_registered` fails loudly if a build
//! ever drops one.
//!
//! Adding a kind therefore means adding those five lines next to the
//! feature that owns it. Nothing in the core has to learn about it, and
//! nothing in the app should branch on a path or a directory name to work
//! out what a file is.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Mutex, OnceLock};

/// The keyword a file declares its kind with. Case-insensitive, must be in
/// the file's leading keyword block.
pub const KIND_KEYWORD: &str = "#+NOBO_TYPE:";

/// How far into a file the declaration may sit. Org keyword blocks live at
/// the very top; scanning further would mean reading whole files on the
/// write path for nothing.
pub(crate) const SCAN_LINES: usize = 16;

/// How long a declared kind may be before it is treated as noise rather
/// than as a name (see `from_keyword`). Every real one is a short slug.
const MAX_KEYWORD_LEN: usize = 64;

/// What a file declares itself to be -- the keyword itself, so a kind is
/// nothing but a name plus the row registered under it.
///
/// Serialised as that keyword ("caldav-list"), which is what it always
/// was on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, specta::Type)]
#[serde(transparent)]
pub struct FileKind(&'static str);

impl FileKind {
    /// A kind read out of a file that nothing in this build claims.
    ///
    /// The name is kept, because it is the only thing anyone can say
    /// about such a file ("this needs caldav-list") -- and kept as a
    /// `&'static str`, because that is what a `FileKind` is, so that
    /// registered kinds stay free to copy and compare. Interned rather
    /// than leaked per call: a vault of ten thousand mirrors holds one
    /// entry, not ten thousand.
    fn unclaimed(keyword: &str) -> Self {
        static SEEN: OnceLock<Mutex<HashSet<&'static str>>> = OnceLock::new();

        let keyword = keyword.to_ascii_lowercase();
        let mut seen = SEEN.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner());
        if let Some(known) = seen.get(keyword.as_str()) {
            return FileKind(known);
        }
        let interned: &'static str = Box::leak(keyword.into_boxed_str());
        seen.insert(interned);
        FileKind(interned)
    }

    /// An ordinary note or task file. Everything the app has always done,
    /// and what anything undeclared or unrecognised falls back to.
    pub const NOTES: FileKind = FileKind::new("notes");

    /// Declares a kind. `const`, so a feature can name its own kind once
    /// and use it as a constant everywhere -- see the module doc.
    pub const fn new(keyword: &'static str) -> Self {
        FileKind(keyword)
    }

    /// The word this kind is declared with. Public because declaring a
    /// file IS a user action (see cookbook's cookbook_set_file), not just
    /// something the app reads.
    pub fn keyword(self) -> &'static str {
        self.0
    }

    /// The line to put at the top of a generated file.
    pub fn declaration(self) -> String {
        format!("{KIND_KEYWORD} {}", self.0)
    }
}

/// Which todo keywords a file's headings may take.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, specta::Type)]
#[serde(rename_all = "kebab-case")]
pub enum StateSupport {
    /// The app's full workflow (TODO/NEXT/IN-PROGRESS/...).
    Full,
    /// Open or done, nothing in between -- a checkbox. What a CalDAV list
    /// can actually represent, and what a shopping list wants.
    DoneOnly,
    /// Headings here carry no todo state at all (calendar entries).
    None,
}

/// Why a file cannot be written here, when its kind alone would allow it.
///
/// Typed rather than a message, because the words belong to the frontend
/// (which has both languages) and because the UI does different things
/// with them: a missing login gets a link to the login, a missing feature
/// gets a plain explanation and no button that could fix it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, specta::Type)]
#[serde(rename_all = "kebab-case", tag = "reason", content = "detail")]
pub enum ReadOnly {
    /// The app generates this file and rewrites it wholesale -- editing it
    /// would be undone on the next refresh.
    Generated,
    /// This build does not have the feature that serves this kind. The
    /// name is the feature's, for the message.
    FeatureMissing(String),
    /// Nothing in this build claims this kind at all -- a file from a
    /// build with more features, or from a newer version. The name is
    /// what the file declares itself to be, which is all anyone here
    /// knows about it (see `capabilities`).
    UnknownKind(String),
    /// The build has it, but it is switched off on this device (see
    /// core::features). A different sentence, and a fixable one: the UI
    /// can offer the switch, which it cannot for the case above.
    FeatureDisabled(String),
    /// The feature is here, but this device cannot use it for this file --
    /// a synced list nobody has logged in to (see WriteVeto).
    NeedsLogin,
}

/// One kind, registered by whoever owns it.
///
/// A kind cannot half-exist: registering it means answering every one of
/// these questions, which is the same thing the old enum's match arm
/// forced.
pub struct FileKindSpec {
    pub kind: FileKind,
    /// The feature that serves this kind, and whether this build has it.
    ///
    /// Registration happens even when the feature is switched off, and it
    /// has to: a vault is shared, so a build without CalDAV still meets
    /// CalDAV mirrors another device wrote. Left unregistered they would
    /// read as ordinary, writable notes -- and an edit here would be
    /// overwritten by the next sync elsewhere. `None` is for the kinds
    /// the core itself serves.
    pub served_by: Option<&'static str>,
    /// Whether the app writes to it at all. Enforced on write, not merely
    /// in the UI (see utils::fs_utils::write_lines).
    pub editable: bool,
    pub states: StateSupport,
    /// Whether the order of headings in the file is meaningful and may be
    /// changed by the user -- true for a CalDAV list, where line order IS
    /// the list order and is synced back as X-APPLE-SORT-ORDER.
    pub reorderable: bool,
    /// Whether headings may be freely added and removed here.
    pub can_add_headings: bool,
}

inventory::collect!(FileKindSpec);

inventory::submit! {
    FileKindSpec {
        kind: FileKind::NOTES,
        // The core's own kind: there is no feature to be without.
        served_by: None,
        editable: true,
        states: StateSupport::Full,
        reorderable: true,
        can_add_headings: true,
    }
}

/// Why a feature is not serving right now: gone from the build, or
/// switched off. Two different sentences for the user, and only one of
/// them has a switch to offer.
fn unavailable(feature: &str) -> ReadOnly {
    if crate::core::features::all().iter().any(|spec| spec.id == feature && spec.compiled_in) {
        ReadOnly::FeatureDisabled(feature.to_string())
    } else {
        ReadOnly::FeatureMissing(feature.to_string())
    }
}

/// What the app may do with a file of a given kind.
#[derive(Debug, Clone, PartialEq, serde::Serialize, specta::Type)]
pub struct FileCapabilities {
    pub kind: FileKind,
    pub editable: bool,
    pub states: StateSupport,
    pub reorderable: bool,
    pub can_add_headings: bool,
    /// Set exactly when `editable` is false, so the UI can say why rather
    /// than showing a lock with no explanation.
    #[specta(optional)]
    pub read_only: Option<ReadOnly>,
}

impl FileCapabilities {
    /// Nothing may be written here, and here is why.
    fn locked(mut self, reason: ReadOnly) -> Self {
        self.editable = false;
        self.can_add_headings = false;
        self.reorderable = false;
        self.read_only = Some(reason);
        self
    }
}

/// The registered kinds, by keyword.
///
/// A `FileKind` IS its keyword, so one map answers both questions asked
/// of the registry: what a file's declaration means (`from_keyword`) and
/// what may be done with a kind (`capabilities`). Built once -- the
/// registrations come from the linker and cannot change -- because both
/// are asked on the write path, for every file.
fn registry() -> &'static HashMap<&'static str, &'static FileKindSpec> {
    static REGISTRY: OnceLock<HashMap<&'static str, &'static FileKindSpec>> = OnceLock::new();
    REGISTRY.get_or_init(|| inventory::iter::<FileKindSpec>.into_iter().map(|spec| (spec.kind.0, spec)).collect())
}

fn spec(kind: FileKind) -> Option<&'static FileKindSpec> {
    registry().get(kind.0).copied()
}

/// What may be done with a file of this kind.
pub fn capabilities(kind: FileKind) -> FileCapabilities {
    match spec(kind) {
        Some(spec) => {
            let capabilities = FileCapabilities {
                kind,
                editable: spec.editable,
                states: spec.states,
                reorderable: spec.reorderable,
                can_add_headings: spec.can_add_headings,
                read_only: None,
            };
            // A kind whose feature is not here -- missing from the build
            // or switched off -- is read-only, and the write guard
            // enforces it (see fs_utils::write_lines), not just the UI.
            // Whatever wrote such a file can still write it; this build
            // only reads it. Asked at call time rather than stored,
            // because being switched off is a runtime answer.
            match spec.served_by {
                Some(feature) if !crate::core::features::is_on(feature) => {
                    capabilities.locked(unavailable(feature))
                }
                _ if !spec.editable => capabilities.locked(ReadOnly::Generated),
                _ => capabilities,
            }
        }
        // A file that declares itself something nothing in this build
        // claims. Read-only, and this arm is the whole reason the split
        // is safe: a vault is shared, so this build meets files another
        // one wrote -- a CalDAV mirror in a build compiled without
        // CalDAV, a kind from a newer version. Both used to read as
        // ordinary notes, and an edit here is an edit the owner
        // overwrites on its next pass.
        //
        // The rule is about the shape of the declaration, not about a
        // list of names: nothing in the core knows which features exist,
        // and a kind invented next year is covered by the same three
        // lines. What it can name is the declaration itself, which is
        // also the only thing that helps ("this file needs caldav-list").
        None if kind != FileKind::NOTES => FileCapabilities {
            kind,
            editable: false,
            states: StateSupport::None,
            reorderable: false,
            can_add_headings: false,
            read_only: Some(ReadOnly::UnknownKind(kind.keyword().to_string())),
        },
        // Only reachable if even the Notes row went missing, which the
        // registry test rules out. Editable: refusing writes because a
        // row is missing would lose the user's edits.
        None => FileCapabilities {
            kind,
            editable: true,
            states: StateSupport::Full,
            reorderable: true,
            can_add_headings: true,
            read_only: None,
        },
    }
}

/// The kind registered under KEYWORD -- or that keyword itself, when
/// nothing registered it.
///
/// Unknown is not an error, but it is not "notes" either: a file written
/// by a newer version of the app, or by a build with a feature this one
/// does not have, stays readable and stops being writable (see
/// `capabilities`). An empty or absurdly long value is not a kind anybody
/// declared, so it falls back to `Notes` rather than interning junk.
fn from_keyword(value: &str) -> FileKind {
    let value = value.trim();
    // Registered keywords are lower case, so the plain lookup answers
    // every ordinary file without touching the heap; only an unusual
    // spelling ("#+NOBO_TYPE: CalDAV-List") pays for a copy.
    let registered = registry()
        .get(value)
        .copied()
        .or_else(|| registry().get(value.to_ascii_lowercase().as_str()).copied());

    match registered {
        Some(spec) => spec.kind,
        None if value.is_empty() || value.len() > MAX_KEYWORD_LEN => FileKind::NOTES,
        None => FileKind::unclaimed(value),
    }
}

/// The kind declared in `lines`, or `Notes` when nothing is declared.
pub fn from_lines<S: AsRef<str>>(lines: &[S]) -> FileKind {
    leading_keyword(lines, KIND_KEYWORD).map(|value| from_keyword(&value)).unwrap_or(FileKind::NOTES)
}

/// Writes KEYWORD into a file's leading keyword block, or removes it when
/// VALUE is `None`.
///
/// The one writer for everything the app declares in a header (the kind
/// below, a favourite in core::favorites), so those cannot drift apart in
/// where they put the line or how they recognise an existing one.
///
/// Placed after the keywords already there: org keeps its file-level
/// keywords together at the top, and a declaration below the first
/// heading would neither be found again (see SCAN_LINES) nor be idiomatic
/// org. Writing what is already there is skipped entirely -- otherwise
/// every toggle would be a change for the vault sync to carry and a diff
/// in Emacs.
pub fn set_leading_keyword(path: &Path, keyword: &str, value: Option<&str>) -> Result<(), String> {
    let lines = crate::utils::fs_utils::read_lines(path).map_err(|e| e.to_string())?;
    let Some(next) = splice_leading_keyword(lines, keyword, value) else { return Ok(()) };
    crate::utils::fs_utils::write_lines(path, &next).map_err(|e| e.to_string())
}

/// LINES with KEYWORD set to VALUE (or removed for `None`), or `None` when
/// they already say that.
///
/// The rule itself, without an opinion on how the file is read or written
/// -- the app has two writers for it: this module's, which goes through
/// the write guard for a file the user owns, and the mirrors' own (see
/// calendar::registry::set_keyword), which is the generator rewriting what
/// it generated and has to bypass that guard. Only the IO differs; the
/// answer to "where does the line go" must not.
pub fn splice_leading_keyword(mut lines: Vec<String>, keyword: &str, value: Option<&str>) -> Option<Vec<String>> {
    let lowercase = keyword.to_ascii_lowercase();
    let existing = lines
        .iter()
        .take(SCAN_LINES)
        .position(|line| line.trim().to_ascii_lowercase().starts_with(&lowercase));

    let wanted = value.map(|value| format!("{keyword} {value}").trim_end().to_string());
    match (wanted, existing) {
        // Already exactly that: writing it again would be a change for the
        // vault sync to carry and a diff in Emacs, for nothing.
        (Some(line), Some(index)) if lines[index].trim() == line => return None,
        (Some(line), Some(index)) => lines[index] = line,
        (Some(line), None) => lines.insert(insert_at(&lines), line),
        (None, Some(index)) => {
            lines.remove(index);
        }
        (None, None) => return None,
    }
    Some(lines)
}

/// After the file's existing leading keywords, or at the very top when it
/// has none.
fn insert_at(lines: &[String]) -> usize {
    lines
        .iter()
        .take(SCAN_LINES)
        .take_while(|line| line.trim().starts_with("#+") || line.trim().is_empty())
        .enumerate()
        .filter(|(_, line)| line.trim().starts_with("#+"))
        .map(|(index, _)| index + 1)
        .last()
        .unwrap_or(0)
}

/// The value of KEYWORD in a file's leading keyword block, if it is there.
///
/// Shared with everything else the app declares in a file's header rather
/// than in a config entry (see this module's own doc comment for why, and
/// core::favorites for the other user of it): same case-insensitive match,
/// same "the header is at the top, do not read whole files for this"
/// limit.
pub fn leading_keyword<S: AsRef<str>>(lines: &[S], keyword: &str) -> Option<String> {
    lines.iter().take(SCAN_LINES).find_map(|line| {
        let trimmed = line.as_ref().trim();
        let rest = trimmed.get(..keyword.len())?;
        rest.eq_ignore_ascii_case(keyword)
            .then(|| trimmed[keyword.len()..].trim().to_string())
    })
}

/// The kind of the file at `path`, read from its first lines.
///
/// A missing or unreadable file is `Notes`: this must never turn a
/// transient read error into "not editable", which would silently reject
/// the user's writes.
pub fn of_path(path: &Path) -> FileKind {
    use std::io::{BufRead, BufReader};
    let Ok(file) = std::fs::File::open(path) else { return FileKind::NOTES };
    let lines: Vec<String> = BufReader::new(file)
        .lines()
        .take(SCAN_LINES)
        .map_while(Result::ok)
        .collect();
    from_lines(&lines)
}

/// A reason THIS device cannot write a file its kind says is writable.
///
/// The kind is a property of the file and travels with the vault; this is
/// not. A CalDAV list is editable on the device that has the login and
/// read-only on the one that does not -- an edit there could not be
/// uploaded and would be overwritten by the next sync elsewhere, so
/// offering it would be offering to lose it.
///
/// Registered by whoever knows (see `nobo_caldav`), asked by
/// `capabilities_for_path`. Async because the answer can need the
/// keychain, and boxed because that is what a registry of function
/// pointers can hold.
pub struct WriteVeto {
    pub kind: FileKind,
    /// What the user is told when it applies.
    pub reason: ReadOnly,
    pub vetoes: fn(std::path::PathBuf) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send>>,
}

inventory::collect!(WriteVeto);

/// What may be done with the file at PATH, on this device, right now.
///
/// `capabilities` answers from the kind alone and is what the write guard
/// uses; this one additionally asks whatever registered a veto. Only the
/// UI needs the difference -- the guard refuses on the kind, and a veto
/// exists precisely for cases the file itself cannot know about.
pub async fn capabilities_for_path(path: &Path) -> FileCapabilities {
    let mut capabilities = capabilities(of_path(path));
    for veto in inventory::iter::<WriteVeto> {
        if veto.kind != capabilities.kind {
            continue;
        }
        if (veto.vetoes)(path.to_path_buf()).await {
            log::debug!("{} is read-only here: {:?}", path.display(), veto.reason);
            capabilities = capabilities.locked(veto.reason.clone());
        }
    }
    capabilities
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Two kinds that exist only for these tests -- registered the same
    /// way a feature registers its own, which is the point: this module
    /// knows no feature, and the mechanism has to work for whoever shows
    /// up.
    const LOCKED: FileKind = FileKind::new("test-locked");
    const CHECKLIST: FileKind = FileKind::new("test-checklist");

    inventory::submit! {
        FileKindSpec {
            kind: LOCKED,
            served_by: None,
            editable: false,
            states: StateSupport::None,
            reorderable: false,
            can_add_headings: false,
        }
    }

    inventory::submit! {
        FileKindSpec {
            kind: CHECKLIST,
            served_by: None,
            editable: true,
            states: StateSupport::DoneOnly,
            reorderable: true,
            can_add_headings: true,
        }
    }

    #[test]
    fn a_file_without_a_declaration_is_an_ordinary_note() {
        assert_eq!(from_lines(&["* TODO Etwas", "Text"]), FileKind::NOTES);
        assert_eq!(from_lines::<String>(&[]), FileKind::NOTES);
    }

    #[test]
    fn the_declaration_is_read_from_the_leading_keyword_block() {
        let lines = ["#+TITLE: Einkauf", "#+NOBO_TYPE: test-checklist", ""];
        assert_eq!(from_lines(&lines), CHECKLIST);
    }

    #[test]
    fn the_keyword_is_case_insensitive_like_every_other_org_keyword() {
        assert_eq!(from_lines(&["#+nobo_type: Test-Locked"]), LOCKED);
    }

    /// Declaring a kind this build does not know must not make the file
    /// unreadable -- a newer version of the app writing an older one's
    /// vault is a normal thing to happen -- but it does make it
    /// read-only: whoever owns that kind rewrites the file, and an edit
    /// here would be lost or, worse, break it for them.
    #[test]
    fn an_unknown_kind_keeps_its_own_name() {
        let kind = from_lines(&["#+NOBO_TYPE: bullet-journal"]);

        assert_ne!(kind, FileKind::NOTES);
        assert_eq!(kind.keyword(), "bullet-journal");
        assert!(!capabilities(kind).editable);
    }

    /// Junk is not a kind: nothing declared, or a value no one could have
    /// meant as a name, stays an ordinary note rather than locking a file
    /// over a stray line.
    #[test]
    fn an_empty_or_absurd_declaration_is_not_a_kind() {
        assert_eq!(from_lines(&["#+NOBO_TYPE:"]), FileKind::NOTES);
        assert_eq!(from_lines(&["#+NOBO_TYPE:    "]), FileKind::NOTES);
        assert_eq!(from_lines(&[format!("#+NOBO_TYPE: {}", "x".repeat(MAX_KEYWORD_LEN + 1))]), FileKind::NOTES);
    }

    /// The same for a kind that names itself but registered nothing: the
    /// file is locked rather than handed the notes row, so a feature that
    /// is half wired up loses nothing and says so.
    #[test]
    fn an_unregistered_kind_is_locked_rather_than_treated_as_notes() {
        let caps = capabilities(FileKind::new("never-registered"));

        assert!(!caps.editable);
        assert_eq!(caps.states, StateSupport::None);
        assert_eq!(caps.read_only, Some(ReadOnly::UnknownKind("never-registered".to_string())));
    }

    /// Deep in the body it is prose, not a declaration -- and scanning the
    /// whole file on every write would be pointless work.
    #[test]
    fn a_declaration_far_down_the_file_is_not_honored() {
        let mut lines = vec!["text".to_string(); SCAN_LINES + 2];
        lines.push("#+NOBO_TYPE: test-checklist".to_string());
        assert_eq!(from_lines(&lines), FileKind::NOTES);
    }

    #[test]
    fn round_trips_through_its_own_declaration_line() {
        for kind in [FileKind::NOTES, LOCKED, CHECKLIST] {
            assert_eq!(from_lines(&[kind.declaration()]), kind, "{kind:?}");
        }
    }

    #[test]
    fn reads_the_kind_off_a_real_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("list.org");
        std::fs::write(&path, "#+TITLE: Einkauf\n#+NOBO_TYPE: test-checklist\n\n* TODO Milch\n").unwrap();

        assert_eq!(of_path(&path), CHECKLIST);
    }

    /// A read error must never read as "not editable" -- that would reject
    /// the user's own writes for a transient filesystem hiccup.
    #[test]
    fn a_missing_file_is_notes_not_something_locked() {
        assert_eq!(of_path(Path::new("/nonexistent/nope.org")), FileKind::NOTES);
        assert!(capabilities(of_path(Path::new("/nonexistent/nope.org"))).editable);
    }

    /// Notes is this module's own kind, and the one everything falls back
    /// to -- if it ever stopped being registered, every file in the vault
    /// would answer with the emergency row instead.
    #[test]
    fn notes_is_registered_and_unrestricted() {
        let caps = capabilities(FileKind::NOTES);

        assert!(caps.editable);
        assert!(caps.reorderable);
        assert!(caps.can_add_headings);
        assert_eq!(caps.states, StateSupport::Full);
        assert!(spec(FileKind::NOTES).is_some(), "the registry lost its own row");
    }

    /// What a registered kind gets is its own row, not a default one.
    #[test]
    fn a_registered_kind_answers_with_what_it_registered() {
        let caps = capabilities(LOCKED);

        assert!(!caps.editable);
        assert!(!caps.can_add_headings);
        assert_eq!(caps.states, StateSupport::None);
        assert_eq!(capabilities(CHECKLIST).states, StateSupport::DoneOnly);
    }

    /// The declaration goes below the keywords the file already has, not
    /// above them and not at the end -- org keeps them together at the
    /// top, and from_lines only looks at the first SCAN_LINES anyway.
    #[test]
    fn declaring_writes_below_the_existing_keywords() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("rezepte.org");
        std::fs::write(&file, "#+TITLE: Rezepte\n\n* Lasagne\n").unwrap();

        set_leading_keyword(&file, KIND_KEYWORD, Some(CHECKLIST.keyword())).unwrap();

        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "#+TITLE: Rezepte\n#+NOBO_TYPE: test-checklist\n\n* Lasagne\n"
        );
    }

    #[test]
    fn a_file_without_any_keywords_gets_the_declaration_on_the_first_line() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("rezepte.org");
        std::fs::write(&file, "* Lasagne\n").unwrap();

        set_leading_keyword(&file, KIND_KEYWORD, Some(CHECKLIST.keyword())).unwrap();

        assert_eq!(std::fs::read_to_string(&file).unwrap(), "#+NOBO_TYPE: test-checklist\n* Lasagne\n");
    }

    /// Undeclaring takes back exactly the line that was written and
    /// nothing else: the rest is the user's own text.
    #[test]
    fn undeclaring_removes_the_line_and_leaves_the_rest_byte_identical() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("rezepte.org");
        let original = "#+TITLE: Rezepte\n\n* Lasagne\n  Zwiebeln\n";
        std::fs::write(&file, original).unwrap();

        set_leading_keyword(&file, KIND_KEYWORD, Some(CHECKLIST.keyword())).unwrap();
        set_leading_keyword(&file, KIND_KEYWORD, None).unwrap();

        assert_eq!(std::fs::read_to_string(&file).unwrap(), original);
    }

    /// A file that already declares a different kind is re-declared in
    /// place -- two NOBO_TYPE lines would leave from_lines picking
    /// whichever came first, so it must never end up with both.
    #[test]
    fn re_declaring_replaces_the_existing_line() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("liste.org");
        std::fs::write(&file, "#+NOBO_TYPE: notes\n* Lasagne\n").unwrap();

        set_leading_keyword(&file, KIND_KEYWORD, Some(CHECKLIST.keyword())).unwrap();

        assert_eq!(std::fs::read_to_string(&file).unwrap(), "#+NOBO_TYPE: test-checklist\n* Lasagne\n");
    }

    /// Writing what is already there must not touch the file at all --
    /// otherwise every toggle is a change for the vault sync to carry.
    #[test]
    fn setting_what_is_already_set_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("rezepte.org");
        std::fs::write(&file, "#+NOBO_TYPE: test-checklist\n* Lasagne\n").unwrap();
        let before = std::fs::metadata(&file).unwrap().modified().unwrap();

        set_leading_keyword(&file, KIND_KEYWORD, Some(CHECKLIST.keyword())).unwrap();

        assert_eq!(std::fs::metadata(&file).unwrap().modified().unwrap(), before);
    }
}
