//! One CalDAV task, in the shape both sides can be derived from.

use serde::{Deserialize, Serialize};

use chrono::NaiveDateTime;

/// A VTODO as this app understands it.
///
/// `raw` is the point of the whole struct: a VTODO from a server carries
/// far more than the handful of fields below -- alarms, RRULEs, RELATED-TO
/// parent links, per-client X-properties. Regenerating the component from
/// the parsed fields on every push would silently delete all of it, so a
/// push instead PATCHES the original text (see vtodo::apply_to_raw) and
/// only ever rewrites the properties this app actually owns.
#[derive(Debug, Clone, PartialEq)]
pub struct CalDavTask {
    pub uid: String,
    pub summary: String,
    pub description: Option<String>,
    pub status: TaskStatus,
    /// `COMPLETED` -- set exactly when `status` is Completed.
    pub completed_at: Option<NaiveDateTime>,
    /// `DTSTART` -> org `SCHEDULED`.
    pub start: Option<TaskDate>,
    /// `DUE` -> org `DEADLINE`.
    pub due: Option<TaskDate>,
    /// `CATEGORIES` -- Nextcloud's and Apple's tag system, mapped onto org
    /// tags rather than the `:CATEGORY:` property: org tags are the thing
    /// a user filters by, and a VTODO can carry several.
    ///
    /// Not all of them survive that mapping: an org tag cannot contain a
    /// space, so "Haus, Garten" has no tag form. Those are kept in
    /// `unmappable_categories` instead of dropped -- writing CATEGORIES
    /// back from the org tags alone would DELETE them from the server.
    pub categories: Vec<String>,
    pub unmappable_categories: Vec<String>,
    /// `RELATED-TO;RELTYPE=PARENT` -- how both Nextcloud Tasks and Apple
    /// Reminders model subtasks. Becomes org nesting on the way in and out
    /// (see caldav::org).
    pub parent_uid: Option<String>,
    /// The `RRULE`, as an org repeater, when it has one this app can
    /// express (see calendar::ics::repeater_from_rrule -- the same rule
    /// set, since a VTODO's RRULE is the same grammar as a VEVENT's).
    ///
    /// Read only. The server owns the recurrence: it decides when the next
    /// occurrence is due, and writing dates over it would fight that.
    pub repeater: Option<nobo_core::core::models::Repeater>,
    /// `X-APPLE-SORT-ORDER`, and ONLY when the server actually carries it.
    ///
    /// Clients write it only when something is dragged; everything else
    /// has no such property. Its scale is the same as `CREATED` -- seconds
    /// since 2001-01-01 -- which is what makes the fallback below work:
    /// an untouched task sorts by when it was made, a dragged one by the
    /// value it was given. See `sort_key`.
    pub sort_order: Option<i64>,
    /// `CREATED`. The ordering fallback, not decoration.
    pub created: Option<NaiveDateTime>,
    /// The component verbatim, as fetched. `None` for a task created here
    /// that the server has never seen.
    pub raw: Option<String>,
}

/// Date-or-datetime, kept apart because org writes them differently and a
/// whole-day due date must not acquire a midnight clock time on the way
/// through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskDate {
    Day(chrono::NaiveDate),
    At(NaiveDateTime),
}

/// RFC 5545 VTODO status, narrowed to what a task list actually uses.
///
/// `IN-PROCESS` is kept distinct rather than folded into NeedsAction: it is
/// what Nextcloud shows as "started", and collapsing it would silently
/// reset that on every push.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskStatus {
    NeedsAction,
    InProcess,
    Completed,
    Cancelled,
}

/// Seconds between the Unix epoch and Apple's (2001-01-01), the scale
/// `X-APPLE-SORT-ORDER` and `CREATED` share.
const APPLE_EPOCH_OFFSET: i64 = 978_307_200;

impl CalDavTask {
    /// Where this task sits in the list, as both Nextcloud Tasks and
    /// tasks.org compute it: the explicit sort order if one was ever
    /// written, otherwise the creation time on the same scale.
    ///
    /// Inventing a value for the tasks without one -- which is most of
    /// them, since only dragging writes the property -- puts the list in
    /// an order no other client agrees with.
    pub fn sort_key(&self) -> Option<i64> {
        self.sort_order
            .or_else(|| self.created.map(|c| c.and_utc().timestamp() - APPLE_EPOCH_OFFSET))
    }
}

impl TaskStatus {
    pub fn as_ics(self) -> &'static str {
        match self {
            TaskStatus::NeedsAction => "NEEDS-ACTION",
            TaskStatus::InProcess => "IN-PROCESS",
            TaskStatus::Completed => "COMPLETED",
            TaskStatus::Cancelled => "CANCELLED",
        }
    }

    pub fn from_ics(value: &str) -> Self {
        match value.trim().to_ascii_uppercase().as_str() {
            "COMPLETED" => TaskStatus::Completed,
            "IN-PROCESS" => TaskStatus::InProcess,
            "CANCELLED" => TaskStatus::Cancelled,
            _ => TaskStatus::NeedsAction,
        }
    }

    pub fn is_finished(self) -> bool {
        matches!(self, TaskStatus::Completed | TaskStatus::Cancelled)
    }
}

/// A remote VTODO collection the user can subscribe to.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[cfg_attr(feature = "tauri", derive(specta::Type))]
pub struct RemoteList {
    /// Whether the collection offers tasks (VTODO) and/or events
    /// (VEVENT). A Nextcloud calendar usually offers both, and each is
    /// its own subscription (see caldav::registry).
    pub supports_tasks: bool,
    pub supports_events: bool,
    /// Absolute URL of the collection.
    pub url: String,
    pub display_name: String,
}

#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
#[cfg_attr(feature = "tauri", derive(specta::Type))]
pub struct ListSyncOutcome {
    #[cfg_attr(feature = "tauri", specta(type = specta_typescript::Number))]
    pub pulled: usize,
    #[cfg_attr(feature = "tauri", specta(type = specta_typescript::Number))]
    pub pushed: usize,
    #[cfg_attr(feature = "tauri", specta(type = specta_typescript::Number))]
    pub created: usize,
    #[cfg_attr(feature = "tauri", specta(type = specta_typescript::Number))]
    pub deleted_local: usize,
    #[cfg_attr(feature = "tauri", specta(type = specta_typescript::Number))]
    pub deleted_remote: usize,
    /// Tasks where both sides had changed and the local edit won. Counted
    /// rather than silently folded into `pushed`: it is a decision made on
    /// the user's behalf, and they should be able to see it happened.
    #[cfg_attr(feature = "tauri", specta(type = specta_typescript::Number))]
    pub local_won_conflicts: usize,
    /// Entries that arrived from the server which this device had never
    /// seen -- somebody else added them. The only count worth telling the
    /// user about (see the notification rules in
    /// docs/background-sync.md): `pulled` also contains tasks this device
    /// merely re-read, and `created` is its own pushes coming back.
    ///
    /// Zero on the first sync of a subscription, where everything is new
    /// by definition and a notification would say nothing.
    #[cfg_attr(feature = "tauri", specta(type = specta_typescript::Number))]
    pub arrived: usize,
    /// Per-task failures. One unreachable task must not cost the run.
    pub errors: Vec<String>,
}

/// What a subscription mirrors: the tasks of a collection, or its events.
/// One collection can offer both, and then they are two subscriptions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "tauri", derive(specta::Type))]
#[serde(rename_all = "lowercase")]
pub enum CalDavKind {
    Tasks,
    Events,
}

impl CalDavKind {
    /// The ICS component this reads, as the server names it.
    pub fn component(self) -> &'static str {
        match self {
            CalDavKind::Tasks => "VTODO",
            CalDavKind::Events => "VEVENT",
        }
    }
}

/// One subscribed CalDAV collection, as its file describes it.
#[derive(Debug, Clone, serde::Serialize)]
#[cfg_attr(feature = "tauri", derive(specta::Type))]
pub struct CalDavSubscription {
    pub id: String,
    pub name: String,
    /// Collection URL on the server.
    pub url: String,
    pub username: String,
    pub kind: CalDavKind,
    /// The keychain entry this needs, so the frontend can ask for exactly
    /// the login that is missing (see identity::account_id).
    pub account_id: String,
    /// What a calendar's entries are marked with in the agenda -- the same
    /// keyword an ICS mirror uses (see calendar::nobo_calendar_mirror::COLOR_KEYWORD),
    /// because they are the same thing in the agenda. Unused for a task
    /// list.
    pub color: Option<String>,
}

/// A CalDAV server this device knows.
///
/// Deliberately NOT the list of what is subscribed -- that lives in the
/// vault, in the mirrored files themselves (see nobo_caldav::registry),
/// and is the same everywhere. This is the much smaller question of
/// "which servers has someone typed an address for on THIS device", kept
/// so that connecting a server and subscribing to something on it can be
/// two separate moments, and so the two settings pages (task lists,
/// calendars) show the same servers.
///
/// A device that received subscriptions through the vault does not need an
/// entry here at all: their accounts are derived from the subscriptions
/// themselves (see nobo_caldav::commands::caldav_accounts). The password is
/// never here either -- it is in the OS keychain, under the same derived
/// id (see caldav::identity::account_id).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "tauri", derive(specta::Type))]
pub struct CalDavAccount {
    /// Derived from username and server, so it matches the keychain entry
    /// and what the subscriptions compute for themselves.
    pub id: String,
    /// Shown in Settings, e.g. "Nextcloud (privat)".
    pub name: String,
    /// The calendar home, i.e. the address the server shows for "all my
    /// calendars".
    pub url: String,
    pub username: String,
}

/// What this feature keeps in `settings.json`, under its own key.
///
/// Its own section rather than fields in `Config` (see
/// `config::section`): both of these are answers about CalDAV, a build
/// without it has no use for them, and -- the part that used to hurt --
/// the frontend can no longer round-trip them. `caldav_accounts` was
/// written by the backend and edited by nobody, so every save of the
/// general settings had to re-read it from disk first and hide it from
/// the generated types. A section needs neither.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "tauri", derive(specta::Type))]
#[serde(rename = "CalDavSettings")]
pub struct Settings {
    /// The servers someone typed an address for on this device.
    #[serde(default)]
    pub accounts: Vec<CalDavAccount>,
    /// Whether ticked-off entries are hidden in the Lists view. Persisted
    /// rather than per-session: for a shopping list "hide what is done"
    /// is how someone uses the app, not a one-off.
    #[serde(default)]
    pub lists_hide_done: bool,
}
