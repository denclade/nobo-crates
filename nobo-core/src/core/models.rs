use std::collections::HashMap;
use std::num::NonZeroU32;
use serde::{Deserialize, Serialize};

// --- Domain Models

#[derive(Debug, Clone, Serialize, Deserialize, Default, specta::Type)]
pub struct Note {
    pub path: String,
    pub identifier: String,
    pub title: String,
    pub date_str: String,
    pub tags: Vec<String>,
}

/// `Default` is what fixtures and partial constructions build on:
/// `Heading { title: "x".into(), ..Default::default() }`.
///
/// Without it, every new field here meant hunting down a dozen struct
/// literals across eight files -- test helpers that each wrote out all
/// twenty-odd fields to set three. Adding `raw_drawers`, `raw_logbook`,
/// `raw_content` and `priority` cost four such rounds.
///
/// Deliberately NOT used by `merge::merge_headings`, which lists every
/// field on purpose: a new field there needs a merge decision, and the
/// compiler refusing to build is how that decision gets made.
#[derive(Debug, Serialize, Deserialize, Clone, Default, specta::Type)]
pub struct Heading {
    pub id: String,
    pub file_path: String,
    #[specta(type = specta_typescript::Number)]
    pub line_no: usize,
    #[specta(type = specta_typescript::Number)]
    pub last_line: usize,
    #[specta(type = specta_typescript::Number)]
    pub level: usize,

    pub state: Option<String>,
    /// Org's priority cookie -- the letter in `* TODO [#A] Etwas`.
    ///
    /// Stored as written rather than as an enum: org's range is
    /// configurable (`org-highest-priority`, default A-C, but numeric and
    /// wider ranges are in use), and this app has no reason to have an
    /// opinion about somebody else's scale.
    ///
    /// Before this it round-tripped as part of the TITLE, so it survived
    /// but nothing could see it -- sorting, filtering and search all read
    /// it as three characters of text.
    #[serde(default)]
    #[specta(optional)]
    pub priority: Option<String>,
    pub title: String,
    #[serde(default)]
    pub tags: Vec<String>,

    pub scheduled: Option<OrgTimestamp>,
    pub deadline: Option<OrgTimestamp>,
    /// A bare active timestamp standing on its own line in the body, which
    /// is org's way of saying "this belongs in the agenda that day"
    /// without claiming anyone plans to work on it. Calendar entries are
    /// written exactly like that (see nobo_calendar::org).
    ///
    /// Derived on read and never written back: the line itself lives in
    /// `content`, so a rewrite preserves it verbatim -- unlike `scheduled`,
    /// which the block builder re-emits as a `SCHEDULED:` line and would
    /// therefore duplicate.
    #[serde(default)]
    pub occurrence: Option<OrgTimestamp>,
    pub effort_minutes: Option<u32>,
    // Number of finished child tasks. Leaves = None
    pub progress_stats: Option<ProgressStats>,
    pub closed: Option<String>,
    pub properties: HashMap<String, String>,
    pub history: Vec<String>,
    /// Raw `CLOCK: [start]`/`CLOCK: [start]--[end] =>  H:MM` lines, exactly
    /// as they appear on disk (see utils::clock_utils for the exact
    /// Emacs-compatible format and parsing) -- same convention as
    /// `history`: opaque strings, newest first, prepended by clock_in/
    /// clock_out. Written back inside a `:LOGBOOK:` drawer alongside
    /// `history` (see heading_utils::build_org_heading_block_inner).
    #[serde(default)]
    pub clocks: Vec<String>,
    /// Lines inside the `:LOGBOOK:` drawer that this build does not model.
    ///
    /// `clocks` and `history` cover `CLOCK:` and `- State` lines; anything
    /// else in there used to fall through to `content` and be written back
    /// BELOW the regenerated drawer, as flowing body text. That is where
    /// `org-log-done note`, `org-log-reschedule` and `C-c C-z` put their
    /// notes -- and a note's indented continuation line ended up under a
    /// different log entry entirely, so the link between a note and the
    /// state change it explains was destroyed.
    ///
    /// Written back inside the drawer, after the typed entries. The exact
    /// interleaving with `clocks`/`history` is not preserved -- those are
    /// regenerated in a fixed order (clocks first, pinned by test) -- but
    /// the lines stay in the drawer they belong to, and in their own
    /// order.
    #[serde(default)]
    #[specta(optional)]
    pub logbook_extra: Vec<String>,
    /// The whole `:LOGBOOK:` body exactly as it was read, markers
    /// excluded.
    ///
    /// Written back verbatim whenever nothing changed the log -- which is
    /// most rewrites, since a state change, a title edit or an ancestor
    /// touch has no business reordering it. `clocks`, `history` and
    /// `logbook_extra` are the typed view and regenerate the drawer in a
    /// fixed order (clocks first), so relying on them alone moved a
    /// `- Note taken on ...` away from the state change it explains --
    /// the very association the note exists to record.
    ///
    /// Same idea as `raw_content`: unedited is written back, edited is
    /// rebuilt.
    #[serde(default)]
    #[specta(optional)]
    pub raw_logbook: Vec<String>,
    /// Drawers other than `:PROPERTIES:` and `:LOGBOOK:`, verbatim,
    /// markers included.
    ///
    /// The parser knew three shapes and dropped every other drawer's
    /// markers, leaving its contents as body text: `:NOTES:` / `geheime
    /// Notiz` / `:END:` came back as a bare paragraph. The text survived,
    /// the structure did not -- folding, non-export, and whatever meaning
    /// the drawer carried.
    ///
    /// Kept as raw lines rather than parsed: this build has no opinion
    /// about what is in someone else's drawer, and round-tripping it
    /// byte-for-byte is the whole job.
    #[serde(default)]
    #[specta(optional)]
    pub raw_drawers: Vec<Vec<String>>,
    /// The body exactly as it was read, before the ghost-prefix offset was
    /// stripped from it.
    ///
    /// `content` is the user-facing text and goes to the frontend;
    /// this is what gets written back when nothing edited the body. The
    /// offset is not symmetric: the read side leaves a line with `<=
    /// level` stars alone (a hand-authored shallower section), and the
    /// write side cannot tell such a line apart from one the app itself
    /// demoted, because after stripping they are identical. So every
    /// rewrite that did not touch the body -- a state change, a clock-in,
    /// `touch_ancestors`, a merge -- pushed a hand-written `* Kapitel 2`
    /// one level deeper.
    ///
    /// Empty means "no verbatim copy": a heading built in memory rather
    /// than parsed, or a merged one, where the body is new text and the
    /// normal offset applies.
    #[serde(default)]
    #[specta(optional)]
    pub raw_content: String,
    /// The property keys in the order the file had them.
    ///
    /// `properties` is a `HashMap`, whose iteration order Rust randomises
    /// per process, so the writer sorted the keys before emitting them --
    /// necessary for the merge's determinism contract, and it meant every
    /// edit rewrote a hand-arranged drawer alphabetically. One churned
    /// file per edit, carried to every device by sync.
    ///
    /// This keeps the arrangement without giving up determinism: known
    /// keys go out in this order, anything new after them, sorted. Empty
    /// for a heading built in memory or merged, where sorted is the only
    /// order there is.
    #[serde(default)]
    #[specta(optional)]
    pub property_order: Vec<String>,
    /// Property lines whose key the drawer already had, kept verbatim as
    /// (key, value) in the order the file had them.
    ///
    /// `properties` is a map, so `:ZUTAT: Mehl` followed by `:ZUTAT:
    /// Wasser` can only keep one of the two -- and the writer rebuilds
    /// the drawer from that map, so the other one was gone the next time
    /// anything about the heading was edited. Org's own answer to wanting
    /// two values is `:ZUTAT+:` (see `accumulated_property_value`), but a
    /// file that repeats the key literally is still a file somebody
    /// wrote, and half of it disappearing on an unrelated edit is data
    /// loss.
    ///
    /// The FIRST occurrence is the one in `properties`, which is what org
    /// itself reads for a repeated key; the rest live here and are
    /// written back after it. Empty for every drawer that does not repeat
    /// a key, which is nearly all of them.
    #[serde(default)]
    #[specta(optional)]
    pub repeated_properties: Vec<(String, String)>,
    /// This heading's own clocked minutes (sum of its closed `clocks`
    /// entries only -- an open/running one doesn't count until it's
    /// stopped), or `None` if it has none. Derived, set by
    /// task_utils::annotate_clocked_time after parsing, analogous to
    /// `progress_stats`.
    #[serde(default)]
    pub own_clocked_minutes: Option<u32>,
    /// `own_clocked_minutes` plus every descendant's, rolled up the same
    /// way `progress_stats` counts a subtree -- `None` when the whole
    /// subtree (this heading included) has no clocked time at all.
    #[serde(default)]
    pub total_clocked_minutes: Option<u32>,

    pub content: String,

    #[serde(default)]
    pub children: Vec<Heading>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, specta::Type)]
pub struct OrgTimestamp {
    /// ISO: "2026-08-05"
    pub start_date: String,
    /// e.g. "14:00"
    pub start_time: Option<String>,
    /// Only when range (`<a>--<b>`)
    pub end_date: Option<String>,
    /// e.g. (`14:00-15:30`)
    pub end_time: Option<String>,
    /// true = active `<>`, false = inactive `[]`
    pub active: bool,
    pub repeater: Option<Repeater>,
    /// Tokens inside the timestamp this build does not model: a warning
    /// period (`-5d`), a delay (`--2d`), a diary sexp, anything org grows
    /// next.
    ///
    /// Carried through verbatim and written back after the repeater. Not
    /// covered by `raw`, which `repeat_utils` overwrites with the
    /// formatted output -- so the two producers would contradict each
    /// other. Without this, `<2026-08-05 Wed -5d>` came back as
    /// `<2026-08-05 Wed>` on the next write and the warning was gone.
    #[serde(default)]
    #[specta(optional)]
    pub extras: Vec<String>,
    /// Original text without brackets, as fallback to not loose data
    pub raw: String,
}

pub const TODO_STATES: &[&str] = &["TODO", "PLANNING", "NEXT", "IN-PROGRESS", "DONE", "OBE", "WONT-DO"];
pub const FINISHED_STATES: [&str; 3] = ["DONE", "OBE", "WONT-DO"];

/// `:project:` tag lookup, shared by every place that needs to tell a
/// project heading apart from a plain one (org_parser's structural rule,
/// heading_utils's projects_tree/apply_content_heading_offset,
/// task_utils's archivable-project detection) -- kept in one place so a
/// future tag-name change or case-sensitivity tweak can't land in only
/// some of them.
pub(crate) fn has_project_tag(tags: &[String]) -> bool {
    tags.iter().any(|t| t.eq_ignore_ascii_case("project"))
}

/// Reads a heading PROPERTIES value, whatever case its key was written in
/// -- Org treats drawer keys case-insensitively and the parser stores them
/// exactly as typed, so a hand-written `:effort:` has to answer to
/// `ORG_EFFORT_PROPERTY` just like `:EFFORT:` does.
///
/// This and the two writers below are the only way this codebase touches a
/// properties map, so a read and the write that follows it can never
/// disagree about which key they meant. The frontend has one counterpart
/// for the same reason: utils/headingUtils.ts's `orgProperty`.
pub fn property_value<'a>(properties: &'a HashMap<String, String>, name: &str) -> Option<&'a str> {
    properties
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

/// The same, with org's `:KEY+:` accumulation applied.
///
/// Org reads `:ZUTAT: Mehl` followed by `:ZUTAT+: Wasser` as one property
/// holding both, space-joined. nobo kept them as two separate keys -- so
/// they round-tripped correctly but every reader saw only the first half.
///
/// Separate from `property_value` on purpose: the keys this app owns
/// (CUSTOM_ID, EFFORT, CATEGORY) are single-valued and have no meaningful
/// `+` form, and joining is only right for the ones a person wrote.
pub fn accumulated_property_value(properties: &HashMap<String, String>, name: &str) -> Option<String> {
    let plus = format!("{name}+");
    let base = property_value(properties, name);
    let extra: Vec<&str> = properties
        .iter()
        .filter(|(key, _)| key.eq_ignore_ascii_case(&plus))
        .map(|(_, value)| value.as_str())
        .collect();

    match (base, extra.is_empty()) {
        (None, true) => None,
        (base, _) => {
            let mut parts: Vec<&str> = base.into_iter().collect();
            parts.extend(extra);
            Some(parts.join(" "))
        }
    }
}

/// Sets NAME, keeping whatever casing the file already used for it --
/// silently rewriting a hand-written `:Effort:` as `:EFFORT:` would put a
/// spurious diff in the user's own file.
pub(crate) fn set_property(properties: &mut HashMap<String, String>, name: &str, value: String) {
    let key = property_key(properties, name).unwrap_or_else(|| name.to_string());
    properties.insert(key, value);
}

/// Removes NAME whatever its casing, reporting whether there was one.
pub(crate) fn remove_property(properties: &mut HashMap<String, String>, name: &str) -> bool {
    match property_key(properties, name) {
        Some(key) => properties.remove(&key).is_some(),
        None => false,
    }
}

fn property_key(properties: &HashMap<String, String>, name: &str) -> Option<String> {
    properties.keys().find(|key| key.eq_ignore_ascii_case(name)).cloned()
}

/// A `*`-line counts as real org structure only with a recognized TODO
/// keyword or a `:project:` tag. Shared between the read and write sides,
/// whose round-trip guarantee depends on both using the same predicate.
pub(crate) fn is_real_structure(state: &Option<String>, tags: &[String]) -> bool {
    state.is_some() || has_project_tag(tags)
}

pub const ORG_PROPERTIES_START: &str = ":PROPERTIES:";
pub const ORG_PROPERTIES_END: &str = ":END:";
pub(crate) const ORG_CLOSED: &str = "CLOSED:";
pub(crate) const ORG_SCHEDULED: &str = "SCHEDULED:";
pub(crate) const ORG_DEADLINE: &str = "DEADLINE:";
pub(crate) const ORG_STATE_LOG: &str = "- State";

/// One `- State "DONE" from "TODO" [timestamp]` line, the way Emacs
/// writes it.
///
/// The padding sits OUTSIDE the quotes. That is not cosmetic: Emacs' own
/// `org-log-note-headings` is `"State %-12s from %-12S %t"`, and its `%s`
/// already expands to the state WITH its quotes -- so the twelve
/// characters count the quotes and the spaces land after them. Padding
/// inside them, which is what this wrote before, puts the trailing blanks
/// into the recorded state itself, and anything reading the value back
/// gets `DONE        ` rather than `DONE`.
pub(crate) fn state_log_line(new_state: &str, old_state: &str, timestamp: &str) -> String {
    format!(
        "{} {:<12} from {:<12} {}",
        ORG_STATE_LOG,
        format!("\"{}\"", new_state),
        format!("\"{}\"", old_state),
        timestamp
    )
}
pub(crate) const ORG_LOGBOOK_START: &str = ":LOGBOOK:";
pub(crate) const ORG_LOGBOOK_END: &str = ":END:";
/// The Areas/Verantwortlichkeiten property (see the frontend's
/// categoryGroups.ts) -- matched case-insensitively wherever it's read
/// (see commands_headings::get_time_tracking_summary), same convention
/// EFFORT/UPDATED/LAST_ACTIVITY already use here (`eq_ignore_ascii_case`)
/// and the frontend's own categoryGroups.ts::getCategory independently
/// arrived at for the same reason: a hand-written `:Category:`/
/// `:category:` is common enough that a case-sensitive lookup silently
/// missed it entirely.
pub(crate) const ORG_CATEGORY_PROPERTY: &str = "CATEGORY";
// Tags are aligned at right
pub(crate) const ORG_TAGS_COLUMN: usize = 80;
// For lines longer than ORG_TAGS_COLUMN just use one space
pub(crate) const ORG_TAGS_MIN_GAP: usize = 1;
pub(crate) const DATE_FORMAT_CLOSED: &str = "%Y-%m-%d %a %H:%M";
/// `UPDATED`/`LAST_ACTIVITY` only -- seconds, unlike every org-visible
/// timestamp above, which org itself writes to the minute.
///
/// Both are nobo-internal bookkeeping, never shown (see TaskCommonFields'
/// HIDDEN_PROPERTIES) and only ever string-compared. At minute resolution
/// two headings touched within the same minute compare EQUAL, so
/// "most recently active first" left them in document order -- adding a
/// subtask to the lower of two projects visibly did nothing.
///
/// Values written before this are shorter and therefore sort as older than
/// any same-minute value written since, which is exactly right.
pub(crate) const DATE_FORMAT_ACTIVITY: &str = "%Y-%m-%d %a %H:%M:%S";
pub(crate) const DATE_FORMAT_LOG: &str = "[%Y-%m-%d %a %H:%M]";


/// Canonical record for one path under the notes vault
#[derive(Debug, Clone, Serialize, specta::Type)]
pub struct FileEntry {
    pub path: String,
    pub name: String,
    pub is_dir: bool,
    /// This entry's own name starts with `.` (e.g. `.attachments`, not
    /// `photo.png` inside it -- see `is_in_hidden_dir` for that case).
    pub is_hidden: bool,
    pub is_in_hidden_dir: bool,
    pub created_at: Option<String>,
    pub modified_at: Option<String>,
}

// --- View Models

#[derive(Debug, Clone, Serialize, Deserialize, specta::Type)]
pub struct DirEntry {
    pub path: String,
    pub name: String,
    pub is_dir: bool,
    pub note: Option<Note>,
    /// Declared in the file itself (see core::favorites), so it follows
    /// the vault to every device instead of living in one device's
    /// settings.
    #[serde(default)]
    pub favorite: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
pub struct ProgressStats {
    pub done: u32,
    pub total: NonZeroU32,
}

impl ProgressStats {
    pub fn progress(&self) -> u32 {
        (self.done * 100) / self.total.get()
    }
}

/// One category's (or, `category: None`, the "Ohne Bereich" fallback
/// bucket's) rolled-up clocked time -- see
/// commands_headings::get_time_tracking_summary.
#[derive(Debug, Clone, Serialize, specta::Type)]
pub struct CategoryTimeTotal {
    pub category: Option<String>,
    #[specta(type = specta_typescript::Number)]
    pub total_minutes: u32,
}

/// Vault-wide time-tracking rollup, grouped by :CATEGORY: -- iterates only
/// level-1 headings (projects + standalone actions, see
/// get_time_tracking_summary's own doc comment for why), each contributing
/// its own already-computed `total_clocked_minutes` (own + every
/// descendant, see task_utils::annotate_clocked_time).
#[derive(Debug, Clone, Serialize, specta::Type)]
pub struct TimeTrackingSummary {
    pub by_category: Vec<CategoryTimeTotal>,
    #[specta(type = specta_typescript::Number)]
    pub grand_total_minutes: u32,
}

#[derive(Serialize, specta::Type)]
pub struct SearchResult {
    pub file: DirEntry,
    pub score: f32,
    pub display_path: String,
}

#[derive(Serialize, specta::Type)]
pub struct FullTextSearchResult {
    pub file: DirEntry,
    pub score: f32,
    pub display_path: String,
    #[specta(type = specta_typescript::Number)]
    pub occurrences: usize,
}

/// A field that can be set, cleared, or left alone: `null` clears it, a
/// value sets it, and leaving it out keeps whatever the heading has.
///
/// Plain serde cannot tell the first from the last. `Option<Option<T>>`
/// maps a JSON null to the OUTER `None`, which is exactly what an absent
/// field gives -- so "take the deadline away" arrived as "change
/// nothing", and every doc comment below describing `Some(None)` was
/// describing a state nothing could produce. This is what makes them
/// true: whatever arrives, including null, is wrapped in `Some`, and only
/// an absent field is left as `None` by the `default`.
fn clearable<'de, T, D>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    T: Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    Option::deserialize(deserializer).map(Some)
}

/// All fields are individually omittable (partial-update payload)
#[derive(Clone, Deserialize, Default, specta::Type)]
pub struct HeadingUpdatePayload {
    #[specta(optional)]
    pub state: Option<String>,
    /// `Some(Some("A"))` sets it, `Some(None)` clears it, `None` leaves it
    /// -- the same shape `scheduled` and `deadline` use (see `clearable`).
    #[specta(optional)]
    #[serde(default, deserialize_with = "clearable")]
    pub priority: Option<Option<String>>,
    #[specta(optional)]
    pub title: Option<String>,
    #[specta(optional)]
    #[serde(default, deserialize_with = "clearable")]
    pub scheduled: Option<Option<OrgTimestamp>>,
    #[specta(optional)]
    #[serde(default, deserialize_with = "clearable")]
    pub deadline: Option<Option<OrgTimestamp>>,
    #[specta(optional)]
    pub content: Option<String>,
    #[specta(optional)]
    pub closed: Option<String>,
    #[specta(optional)]
    pub properties: Option<HashMap<String, String>>,
    #[specta(optional)]
    pub history: Option<Vec<String>>,
    #[specta(optional)]
    pub clocks: Option<Vec<String>>,
    #[specta(optional)]
    pub tags: Option<Vec<String>>,
    #[specta(optional)]
    #[serde(default, deserialize_with = "clearable")]
    pub effort_minutes: Option<Option<u32>>,
}

/// Which heading a write is meant for, as the CALLER last saw it.
///
/// A line number alone cannot answer that. Every insert or deletion above
/// a heading shifts it, so the frontend's copy goes stale the moment any
/// other write lands -- and a stale number still addresses SOME heading,
/// just the wrong one. That is how an edit to one task ended up
/// overwriting another (a description written, saved, and gone again).
///
/// So the number is only ever a hint here; `heading_identity::
/// resolve_target` re-finds the heading by its CUSTOM_ID (or, for one
/// that has none yet, by its title) and refuses the write outright rather
/// than applying it to whatever else moved into that line. Callers build
/// one straight from the Heading they are acting on -- see the frontend's
/// `headingTarget()`.
#[derive(Debug, Clone, Deserialize, Serialize, specta::Type)]
pub struct HeadingTarget {
    #[specta(type = specta_typescript::Number)]
    pub line_no: usize,
    /// The heading's `:CUSTOM_ID:` when it had one -- the only identity
    /// that survives a rename, a refile or a shifted file.
    #[specta(optional)]
    pub custom_id: Option<String>,
    /// Fallback identity for a heading without a CUSTOM_ID (hand-written
    /// files, notes that were never backfilled). Only identifies while it
    /// is unique in the file -- two headings sharing a title identify
    /// neither, and the write is refused (see
    /// heading_identity::resolve_target).
    #[specta(optional)]
    pub title: Option<String>,
}

/// One heading in a batch update: which heading it is (see HeadingTarget),
/// and its own payload. Payloads are independent -- a state change is
/// usually uniform across the batch, while a tag merge differs per heading.
#[derive(Deserialize, specta::Type)]
pub struct BulkHeadingUpdate {
    pub target: HeadingTarget,
    pub payload: HeadingUpdatePayload,
}

/// One heading's own entry within a bulk_refile_headings batch -- TARGET
/// identifies which source heading (within the ONE source file that batch
/// call covers), DEFAULT_STATE is the same per-item "give it TODO if it has
/// no state of its own" nudge single-item refile already applies (see
/// useRefile.ts's refileTo: `source.state ? null : "TODO"`), kept
/// independent per entry since a batch can freely mix items that already
/// have a state with plain ones that don't.
#[derive(Deserialize, specta::Type)]
pub struct BulkRefileSource {
    pub target: HeadingTarget,
    #[specta(optional)]
    pub default_state: Option<String>,
}

#[derive(Deserialize, specta::Type)]
pub struct HeadingCreatePayload {
    pub file_path: String,
    pub title: String,
    // No `level`: it is not the caller's to decide. With a parent it is
    // that parent's level plus one, without one it is a top-level heading
    // -- and a level the caller worked out from its own (possibly stale)
    // copy of the parent could contradict where the heading actually
    // lands. See create_heading_in_file.
    #[specta(optional)]
    pub state: Option<String>,
    pub tags: Vec<String>,
    /// The heading the new one becomes a child of, or None to append at
    /// the end of the file. Identified the same guarded way as any other
    /// write target (see HeadingTarget) -- a stale line number would
    /// otherwise file a subtask under whichever heading moved into it.
    #[specta(optional)]
    pub parent: Option<HeadingTarget>,
    #[specta(optional)]
    pub scheduled: Option<OrgTimestamp>,
    #[specta(optional)]
    pub effort_minutes: Option<u32>,
    #[specta(optional)]
    pub content: Option<String>,
    /// :CATEGORY: (GTD area) to stamp on the new heading. Only projects
    /// carry one, so this is set from the new-project form and left unset
    /// everywhere else.
    #[specta(optional)]
    pub category: Option<String>,
}

#[derive(Serialize, specta::Type)]
pub struct NoteLoadResult {
    pub content: String,
    pub original_content: String,
    pub has_temp: bool,
    pub temp_content: Option<String>,
}

#[derive(Serialize, specta::Type)]
pub struct DiffChunk {
    pub tag: String,
    pub value: String,
}

// --- History / Undo-Redo

/// One file's content before/after a history action -- `None` for `before`
/// means the file didn't exist yet (the action created it); `None` for
/// `after` means the action deleted it. Undo restores `before` (deleting
/// the file if `None`); redo restores `after`. See core::history.
#[derive(Debug, Clone, Serialize, Deserialize, specta::Type)]
pub struct FileSnapshot {
    pub path: String,
    pub before: Option<String>,
    pub after: Option<String>,
}

/// Structured, backend-owned action descriptor -- deliberately not a
/// human-readable string. The frontend renders the actual localized label
/// from this (see describeHistoryEntry in src/utils/historyLabels.ts),
/// since this app is bilingual and Rust shouldn't own German/English prose.
#[derive(Debug, Clone, Serialize, Deserialize, specta::Type)]
#[serde(tag = "kind")]
pub enum HistoryEntryKind {
    FileCreated { filename: String },
    FileEdited { filename: String },
    FileDeleted { filename: String },
    FileRenamed { old_filename: String, new_filename: String },
    FileMoved { filename: String, target_dir: String },
    FileDuplicated { source_filename: String, new_filename: String },
    /// No snapshot content (binary) -- see core::history's size/type limits.
    AttachmentAdded { filename: String },
    HeadingCreated { filename: String, title: String },
    HeadingUpdated { filename: String, title: String },
    HeadingDeleted { filename: String, title: String },
    HeadingsBulkUpdated { filename: String, count: u32 },
    HeadingsBulkDeleted { filename: String, count: u32 },
    /// A deleted heading's subtree reinserted under its original parent,
    /// or at file end if it was top-level.
    HeadingRestored { filename: String, title: String },
    /// Recorded by core::history::restore::apply_bulk -- the whole-file
    /// counterpart to HeadingRestored, for a HeadingsBulkDeleted/
    /// HeadingsBulkUpdated/HeadingsBulkRefiled entry reverted back to its
    /// own before-snapshot text (COUNT carried over from the original
    /// entry's own count, so the label reads symmetrically, e.g. "7
    /// Einträge in projects.org wiederhergestellt").
    HeadingsBulkRestored { filename: String, count: u32 },
    /// Recorded by commands_headings::backfill_custom_ids -- one entry per
    /// file it actually touched, so the one-time CUSTOM_ID backfill is
    /// itself a normal, undo-able history action.
    CustomIdsBackfilled { filename: String, count: u32 },
    /// Recorded by commands_headings::remove_custom_ids -- the Maintenance
    /// section's counterpart to CustomIdsBackfilled above, one entry per
    /// file it actually touched.
    CustomIdsRemoved { filename: String, count: u32 },
    /// Recorded by commands_headings::remove_empty_properties_drawers --
    /// Maintenance's "delete empty drawers" action, one entry per file it
    /// actually touched.
    EmptyPropertiesDrawersRemoved { filename: String, count: u32 },
    Archived { filename: String, count: u32 },
    Refiled { title: String, target_filename: String },
    HeadingsBulkRefiled { count: u32, target_filename: String },
    TangleWrite { filename: String },
    LinksReplaced { old_identifier: String, new_identifier: String, file_count: u32 },
    /// Recorded by commands_headings::clock_in -- a new open CLOCK entry
    /// started on this heading (auto-closing whatever other heading's
    /// timer, if any, was running -- see that command's own doc comment).
    ClockStarted { filename: String, title: String },
    /// Recorded by commands_headings::clock_out -- this heading's own open
    /// CLOCK entry closed with a duration.
    ClockStopped { filename: String, title: String },
}

/// Result of core::history::restore::check -- whether a single, arbitrary
/// past HeadingDeleted entry can be reinserted against the CURRENT document
/// AST (not the linear undo/redo stack, see that module's own doc comment),
/// plus everything the frontend needs to preview it before the user
/// confirms. `reason`/`raw_block`/`current_text`/`preview_text` are mutually
/// exclusive in practice: `reason` is set (others `None`) when infeasible,
/// the other three are set (reason `None`) when feasible.
#[derive(Debug, Clone, Serialize, specta::Type)]
pub struct HeadingRestoreCheck {
    pub feasible: bool,
    /// German plain text, same convention as core::errors::not_found_at_line
    /// -- shown to the user verbatim, not run through Paraglide.
    pub reason: Option<String>,
    pub raw_block: Option<String>,
    pub current_text: Option<String>,
    pub preview_text: Option<String>,
    /// Other files besides the one CURRENT_TEXT/PREVIEW_TEXT diff -- always
    /// empty for a single-heading restore (HeadingDeleted/HeadingUpdated,
    /// always one file), populated for a whole-file bulk restore that spans
    /// more than one (e.g. a cross-file HeadingsBulkRefiled entry's source
    /// AND target). Restoring still reverts every one of them, even though
    /// only the primary file's diff is shown -- see restore::apply_bulk.
    pub additional_filenames: Vec<String>,
}

/// Contains information about missing Custom ID fields which should be presented
/// to the user to help them decide whether to backfill the Custom IDs.
#[derive(Debug, Clone, Serialize, specta::Type)]
pub struct CustomIdBackfillPreview {
    pub missing_count: u32,
    /// Some examples of heading titles which would be affected.
    pub example_titles: Vec<String>,
    /// One fully rendered example of what a heading's own block looks like
    /// once stamped (raw org-text).
    pub example_block: Option<String>,
}

/// Identifies one specific heading, file included -- the same
/// `(file_path, HeadingTarget)` pair `update_heading_in_file` and friends
/// take as two separate params, bundled into one struct for the commands
/// that deal with several headings across several files at once
/// (find_orphaned_custom_ids/remove_custom_ids) instead of parallel
/// arrays. Carrying the full target, not a bare line number, is what lets
/// those act on exactly the headings their own preview step listed even
/// if the file moved on in between -- see heading_identity::resolve_target.
#[derive(Debug, Clone, Deserialize, Serialize, specta::Type)]
pub struct HeadingLocation {
    pub file_path: String,
    pub target: HeadingTarget,
}

/// One heading whose own CUSTOM_ID is never referenced by any `[[denote:
/// ...::#h:ID][...]]` link anywhere in the vault -- see
/// note_utils::find_orphaned_custom_ids. Surfaced in Settings' Maintenance
/// section so the user can review before deleting, same "preview, then
/// confirm" shape as CustomIdBackfillPreview above.
#[derive(Debug, Clone, Serialize, specta::Type)]
pub struct OrphanedCustomId {
    pub location: HeadingLocation,
    pub title: String,
    pub custom_id: String,
}

/// A plain `.org` file under the vault that ISN'T a recognized Denote note
/// (see note_utils::parse_note_record -- no Denote-shaped filename, so no
/// stable identifier) -- see note_utils::find_unpromoted_org_files.
/// Surfaced in Settings' Maintenance section so the user can review and
/// promote (note_utils::promote_org_file_to_note) the ones they actually
/// want treated as a real note, instead of every stray `.org` file
/// silently gaining one unasked. `title` is read from the file's own
/// `#+TITLE:` if it has one, else derived from the filename.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, specta::Type)]
pub struct UnpromotedOrgFile {
    pub path: String,
    pub title: String,
}

/// One heading whose own `:PROPERTIES:...:END:` drawer has nothing left in
/// it -- see note_utils::find_empty_properties_drawers. A heading with NO
/// drawer at all never appears here; this is specifically the "drawer
/// still exists but is pointless" case, e.g. left behind by hand-editing,
/// or (before remove_custom_ids started cleaning this up itself, see
/// heading_utils::remove_custom_id_and_empty_drawer) an older CUSTOM_ID
/// removal.
#[derive(Debug, Clone, Serialize, specta::Type)]
pub struct EmptyPropertiesDrawer {
    pub location: HeadingLocation,
    pub title: String,
}

/// One entry in the linear undo/redo stack -- `undone` reflects whether
/// this entry is currently past the undo boundary (i.e. redo-able rather
/// than undo-able). See core::history for the SQLite-backed store this is
/// read from/written to.
#[derive(Debug, Clone, Serialize, Deserialize, specta::Type)]
pub struct HistoryEntry {
    #[specta(type = specta_typescript::Number)]
    pub id: i64,
    /// RFC3339 (chrono::Local::now()), same convention as babel::trust's
    /// `trusted_at`.
    pub timestamp: String,
    pub kind: HistoryEntryKind,
    pub is_draft: bool,
    pub undone: bool,
    pub snapshots: Vec<FileSnapshot>,
}

#[derive(serde::Deserialize, Clone, Copy, PartialEq, specta::Type)]
#[serde(rename_all = "snake_case")]
pub enum RefilePosition {
    Into,
    Before,
    After,
}


#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "snake_case")]
pub enum RepeaterKind {
    /// + — moves exactly one interval
    Cumulative,
    /// ++ — schiebt until future
    CatchUp,
    /// .+ — calculates from closing date
    Restart,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, specta::Type)]
pub struct Repeater {
    pub kind: RepeaterKind,
    pub value: u32,
    /// h, d, w, m, y
    pub unit: char,
    /// The `/N<unit>` half of a habit (`.+1d/3d`) -- org's "and at most
    /// this long after the scheduled date".
    ///
    /// Kept verbatim; nothing here interprets it yet. Dropping it was the
    /// expensive half of the loss: without a repeater surviving the
    /// rewrite, `repeat_utils::repeat_on_finish` stops firing, so ticking
    /// the habit off sets it to DONE once and archives it -- the routine
    /// simply ceases to exist.
    #[serde(default)]
    #[specta(optional)]
    pub habit_max: Option<String>,
}

impl OrgTimestamp {
    pub fn has_time(&self) -> bool {
        self.start_time.is_some()
    }

    pub fn duration_minutes(&self) -> Option<u32> {
        let (start, end) = (self.start_time.as_ref()?, self.end_time.as_ref()?);
        let to_min = |s: &str| -> Option<u32> {
            let (h, m) = s.split_once(':')?;
            Some(h.parse::<u32>().ok()? * 60 + m.parse::<u32>().ok()?)
        };
        let (s, e) = (to_min(start)?, to_min(end)?);
        (e >= s).then(|| e - s)
    }

    pub fn unparsed(raw: &str) -> Self {
        Self {
            start_date: String::new(),
            start_time: None,
            end_date: None,
            end_time: None,
            active: true,
            repeater: None,
            extras: Vec::new(),
            raw: raw.trim().to_string(),
        }
    }

    pub fn is_unparsed(&self) -> bool {
        self.start_date.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /* --- state_log_line --- */

    #[test]
    fn state_log_line_pads_outside_the_quotes_like_emacs() {
        // Emacs' own template is `State %-12s from %-12S %t`, and its
        // `%s` already carries the quotes -- so the field width counts
        // them and the blanks land behind the closing quote.
        assert_eq!(
            state_log_line("DONE", "TODO", "[2026-08-30 Sun 15:05]"),
            "- State \"DONE\"       from \"TODO\"       [2026-08-30 Sun 15:05]"
        );
    }

    #[test]
    fn state_log_line_keeps_the_state_free_of_padding() {
        // The whole point: what stands between the quotes is the state,
        // nothing else. Padded inside, every reader had to trim.
        let line = state_log_line("NEXT", "", "[2026-08-30 Sun 15:05]");
        assert!(line.contains("\"NEXT\""), "{line}");
        assert!(line.contains("from \"\""), "{line}");
    }

    #[test]
    fn state_log_line_does_not_cut_a_long_state() {
        let line = state_log_line("WAITING-FOR-REVIEW", "TODO", "[2026-08-30 Sun 15:05]");
        assert!(line.contains("\"WAITING-FOR-REVIEW\" from"), "{line}");
    }

    fn props(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn property_value_ignores_the_keys_case() {
        let p = props(&[("Effort", "0:30")]);
        assert_eq!(property_value(&p, "EFFORT"), Some("0:30"));
        assert_eq!(property_value(&p, "CUSTOM_ID"), None);
    }

    /// The whole reason writes go through set_property: rewriting a
    /// hand-written `:Effort:` as `:EFFORT:` would show up as a change in
    /// the user's own file that they never made.
    #[test]
    fn set_property_updates_in_place_and_keeps_the_existing_spelling() {
        let mut p = props(&[("Effort", "0:30")]);
        set_property(&mut p, "EFFORT", "1:00".to_string());
        assert_eq!(p.len(), 1, "no second, differently-cased key: {p:?}");
        assert_eq!(p.get("Effort").map(String::as_str), Some("1:00"));
    }

    #[test]
    fn set_property_falls_back_to_the_canonical_spelling_for_a_new_one() {
        let mut p = HashMap::new();
        set_property(&mut p, "EFFORT", "1:00".to_string());
        assert_eq!(p.get("EFFORT").map(String::as_str), Some("1:00"));
    }

    #[test]
    fn remove_property_removes_whatever_case_it_was_written_in() {
        let mut p = props(&[("Effort", "0:30"), ("CUSTOM_ID", "h:1")]);
        assert!(remove_property(&mut p, "EFFORT"));
        assert!(!remove_property(&mut p, "EFFORT"), "already gone");
        assert_eq!(p.len(), 1, "left everything else alone: {p:?}");
    }

    /* --- a field that can be set, cleared, or left alone --- */

    fn payload(json: &str) -> HeadingUpdatePayload {
        serde_json::from_str(json).unwrap_or_else(|e| panic!("{json}: {e}"))
    }

    /// The three states, on the three fields that have them. The middle
    /// one is why `clearable` exists: plain serde gave a JSON null the
    /// same answer as an absent field, so nothing could ever say "take
    /// this away" -- and every caller that tried wrote a shape the core
    /// refused outright.
    #[test]
    fn null_clears_a_field_and_an_absent_one_leaves_it_alone() {
        let set = payload(r#"{"effort_minutes": 45, "priority": "A"}"#);
        assert_eq!(set.effort_minutes, Some(Some(45)));
        assert_eq!(set.priority, Some(Some("A".to_string())));

        let cleared = payload(r#"{"effort_minutes": null, "priority": null, "scheduled": null}"#);
        assert_eq!(cleared.effort_minutes, Some(None), "null means take it away");
        assert_eq!(cleared.priority, Some(None));
        assert!(matches!(cleared.scheduled, Some(None)));

        let untouched = payload(r#"{"title": "unrelated"}"#);
        assert_eq!(untouched.effort_minutes, None, "absent means leave it alone");
        assert_eq!(untouched.priority, None);
        assert!(untouched.scheduled.is_none());
    }

    /// A timestamp is sent as itself. It used to be wrapped in a
    /// one-element array by callers who read the doc comment as "nest it
    /// once", which serde reads as a struct given its fields positionally
    /// -- refused with a message about the first field's type, and
    /// nowhere near the real mistake.
    #[test]
    fn a_timestamp_is_sent_as_itself_not_wrapped() {
        let set = payload(r#"{"scheduled": {"start_date": "2026-09-23", "active": true, "raw": ""}}"#);
        let stamp = set.scheduled.expect("present").expect("set");
        assert_eq!(stamp.start_date, "2026-09-23");

        assert!(serde_json::from_str::<HeadingUpdatePayload>(
            r#"{"scheduled": [{"start_date": "2026-09-23", "active": true, "raw": ""}]}"#
        ).is_err(), "a wrapped timestamp is refused rather than half-read");
    }
}
