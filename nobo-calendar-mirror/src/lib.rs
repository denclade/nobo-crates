//! One calendar in the vault, as a file.
//!
//! Two features write one of these: an ICS subscription (nobo_calendar)
//! and a CalDAV calendar (nobo_caldav). On disk it is the same file in the
//! same shape -- same directory, same `#+NOBO_TYPE:`, same entries -- and
//! that is deliberate: an event is a calendar entry whatever protocol it
//! arrived over, and the agenda reads one directory.
//!
//! It lives here rather than in either feature because it belongs to
//! neither. CalDAV used to depend on the ICS subscriptions for it, which
//! meant a build with CalDAV accounts had to carry a feature it has
//! nothing to do with (see docs/architecture.md on shared libraries).
//!
//! What is NOT here: how a feed is fetched, what a subscription's header
//! says, how it is refreshed. Those belong to whoever subscribes.

pub mod ics;
pub mod org;

use std::path::{Path, PathBuf};

use chrono::NaiveDateTime;
use nobo_core::core::file_kind::{FileKind, FileKindSpec, StateSupport};
use nobo_core::core::models::Repeater;

/// A single occurrence, as it will appear in the vault.
///
/// Deliberately NOT a `Heading`: a calendar entry carries no TODO state and
/// is never edited in the app (see the calendars/ directory's read-only
/// rule), so it shares nothing with the task model beyond the timestamp.
#[derive(Debug, Clone, PartialEq)]
pub struct CalendarEvent {
    /// The ICS `UID`. Kept as `:ICS_UID:` so a re-import can tell a moved
    /// event from a new one, and drop what is gone remotely.
    pub uid: String,
    pub summary: String,
    /// Already converted to the machine's local time -- org timestamps
    /// carry no zone, so this has to happen before writing, not on read.
    pub start: NaiveDateTime,
    /// `None` for an event with no duration.
    pub end: Option<NaiveDateTime>,
    /// A whole-day event writes no clock time at all (`<2026-08-21 Fri>`),
    /// which is also how org itself distinguishes the two.
    pub all_day: bool,
    pub description: Option<String>,
    pub location: Option<String>,
    /// Set when the ICS `RRULE` maps onto an org repeater; see
    /// ics::repeater_from_rrule for which rules do and don't.
    pub repeater: Option<Repeater>,
}

/// The colour the calendar's entries are marked with in the agenda. In the
/// file like everything else about a subscription, so the calendar looks
/// the same on every device -- a colour that only one device knows is
/// worse than no colour, because the agenda then means something
/// different depending on where you read it.
///
/// Shared with the CalDAV calendars (see caldav::registry), which are the
/// same thing arriving over a different protocol.
pub const COLOR_KEYWORD: &str = "#+NOBO_CALENDAR_COLOR:";

/// A calendar mirror: generated from a subscription and owned by the app.
pub const MIRROR_KIND: FileKind = FileKind::new("calendar-mirror");

inventory::submit! {
    FileKindSpec {
        kind: MIRROR_KIND,
        // Nobody's in particular, and that is the honest answer: an ICS
        // subscription writes these and so does a CalDAV calendar. It is
        // read-only either way (see `editable` below), so a reader never
        // has to know which of the two made it -- and switching a feature
        // off does not turn the other one's files into ordinary notes.
        served_by: None,
        // Regenerated wholesale on every refresh, so editing it is
        // pointless AND lossy.
        editable: false,
        states: StateSupport::None,
        reorderable: false,
        can_add_headings: false,
    }
}

/// Where the mirrors live, for every kind of calendar.
pub fn dir(vault_root: &Path) -> PathBuf {
    nobo_core::core::vault_write::app_dir(vault_root).join("calendars")
}

/// The extension a mirror file has.
pub const FILE_EXTENSION: &str = "org";

/// Whether `path` is one of the generated calendar files, checked against
/// THIS vault. The write-time guard uses the vault-independent shape check
/// in fs_utils instead; this one additionally rules out a same-shaped path
/// belonging to some other vault.
pub fn is_mirror_file(vault_root: &Path, path: &Path) -> bool {
    path.parent() == Some(dir(vault_root).as_path()) && path.extension().is_some_and(|e| e.eq_ignore_ascii_case(FILE_EXTENSION))
}
