//! The CalDAV subscriptions, read out of the mirrored files themselves.
//!
//! Same rule as the ICS feeds next door (nobo_calendar::registry): the
//! file in the vault IS the subscription, and there is no list of them in
//! settings.json. Its header names the collection and the user it belongs
//! to:
//!
//! ```org
//! #+TITLE: Einkauf
//! #+NOBO_TYPE: caldav-list
//! #+NOBO_CALDAV_URL: https://cloud.example.com/remote.php/dav/calendars/dennis/einkauf/
//! #+NOBO_CALDAV_USER: dennis
//! ```
//!
//! What that buys, and it is the whole point of doing it this way: the
//! subscription itself is synced along with the vault, so a list or
//! calendar added on one device exists on all of them. Only the PASSWORD
//! is per device -- it stays in the OS keychain and is never written
//! anywhere near the vault. A device that has it syncs the subscription; a
//! device that does not still SEES it, marked as needing a login, with the
//! mirrored content readable as it last arrived. Nothing is missing there,
//! and nothing has to be set up twice except typing the password once.

use std::path::{Path, PathBuf};

use nobo_core::core::file_kind;

use super::identity;
use super::model::{CalDavKind, CalDavSubscription};

pub const URL_KEYWORD: &str = "#+NOBO_CALDAV_URL:";
pub const USER_KEYWORD: &str = "#+NOBO_CALDAV_USER:";

impl CalDavKind {
    /// Tasks are mirrored next to the other CalDAV files, events next to
    /// the ICS calendars -- an event is a calendar entry whatever protocol
    /// it arrived over, and the agenda reads that one directory.
    pub fn dir(self, vault_root: &Path) -> PathBuf {
        match self {
            CalDavKind::Tasks => nobo_core::core::vault_write::app_dir(vault_root).join("caldav"),
            CalDavKind::Events => nobo_calendar_mirror::dir(vault_root),
        }
    }

    pub fn id_for(self, username: &str, collection_url: &str) -> String {
        match self {
            CalDavKind::Tasks => identity::subscription_id(username, collection_url),
            CalDavKind::Events => identity::calendar_subscription_id(username, collection_url),
        }
    }
}

impl CalDavSubscription {
    pub fn file(&self, vault_root: &Path) -> PathBuf {
        self.kind.dir(vault_root).join(format!("{}.org", self.id))
    }
}

/// Every CalDAV subscription in the vault, tasks and calendars together.
pub fn list(vault_root: &Path) -> Vec<CalDavSubscription> {
    let mut subscriptions: Vec<CalDavSubscription> = [CalDavKind::Tasks, CalDavKind::Events]
        .into_iter()
        .flat_map(|kind| list_of_kind(vault_root, kind))
        .collect();
    subscriptions.sort_by_key(|subscription| subscription.name.to_lowercase());
    subscriptions
}

pub fn list_of_kind(vault_root: &Path, kind: CalDavKind) -> Vec<CalDavSubscription> {
    let Ok(entries) = std::fs::read_dir(kind.dir(vault_root)) else { return Vec::new() };
    entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|e| e.eq_ignore_ascii_case("org")))
        .filter_map(|path| read(&path))
        .collect()
}

pub fn find(vault_root: &Path, id: &str) -> Option<CalDavSubscription> {
    list(vault_root).into_iter().find(|subscription| subscription.id == id)
}

/// Reads one mirror file's header back into the subscription it describes.
///
/// A file that carries no CalDAV URL is not one -- an ICS mirror in the
/// same directory says `#+NOBO_CALENDAR_URL:` instead, and is read by
/// nobo_calendar::registry.
pub fn read(path: &Path) -> Option<CalDavSubscription> {
    let text = std::fs::read_to_string(path).ok()?;
    let lines: Vec<&str> = text.lines().collect();
    let url = file_kind::leading_keyword(&lines, URL_KEYWORD)?;
    let username = file_kind::leading_keyword(&lines, USER_KEYWORD).unwrap_or_default();
    // The declaration, not the directory: it is what makes the file
    // read-only or editable, so it must also be what decides how it syncs.
    let kind = match file_kind::from_lines(&lines) {
        crate::LIST_KIND => CalDavKind::Tasks,
        nobo_calendar_mirror::MIRROR_KIND => CalDavKind::Events,
        _ => return None,
    };

    Some(CalDavSubscription {
        id: path.file_stem()?.to_string_lossy().into_owned(),
        color: file_kind::leading_keyword(&lines, nobo_calendar_mirror::COLOR_KEYWORD)
            .filter(|color| !color.is_empty()),
        name: file_kind::leading_keyword(&lines, "#+TITLE:").filter(|t| !t.is_empty()).unwrap_or_else(|| url.clone()),
        account_id: identity::account_id(&username, &url),
        username,
        url,
        kind,
    })
}

/// The subscription a collection would produce, without writing anything.
pub fn subscription_for(kind: CalDavKind, name: &str, username: &str, url: &str) -> CalDavSubscription {
    let url = url.trim().to_string();
    let username = username.trim().to_string();
    let name = name.trim();
    CalDavSubscription {
        id: kind.id_for(&username, &url),
        color: None,
        name: if name.is_empty() { url.clone() } else { name.to_string() },
        account_id: identity::account_id(&username, &url),
        username,
        url,
        kind,
    }
}

/// Writes the subscription's file with nothing in it yet.
///
/// Subscribing has to leave a file behind before the first sync runs: it
/// is what carries the subscription to the other devices, and what keeps
/// it here if the server is unreachable right now.
pub fn write_empty(vault_root: &Path, subscription: &CalDavSubscription) -> Result<(), String> {
    nobo_core::core::vault_write::write_generated(&subscription.file(vault_root), &header(subscription))
}

/// The header every mirror of this subscription starts with -- the part
/// that IS the subscription, written by the task renderer and the calendar
/// renderer alike so a rewrite can never drop it.
pub fn header(subscription: &CalDavSubscription) -> String {
    let kind_declaration = match subscription.kind {
        CalDavKind::Tasks => crate::LIST_KIND.declaration(),
        CalDavKind::Events => nobo_calendar_mirror::MIRROR_KIND.declaration(),
    };
    // English for everyone, and deliberately NOT run through `t!` --
    // unlike every other sentence a user reads.
    //
    // This one is FILE CONTENT. The file goes through the vault sync to
    // devices whose language setting is their own (settings.json is per
    // device), so a localised line would be rewritten past itself on
    // every sync between a German and an English device: each one
    // regenerates the mirror, each rewrite is a content change, each
    // change is an upload. Forever, over a comment.
    //
    // So it is one language for every device, and English is the one the
    // rest of the file already speaks -- `#+TITLE:`, `#+COMMENT:` and
    // every other keyword around it. This is a generated artefact, not
    // prose the user wrote.
    let comment = match subscription.kind {
        CalDavKind::Tasks => "#+COMMENT: Synced by nobo with a CalDAV list. Editing is fine; changes are uploaded.",
        CalDavKind::Events => "#+COMMENT: Generated by nobo from a CalDAV calendar. Changes are lost on the next refresh.",
    };
    let color = match &subscription.color {
        Some(color) => format!("{} {color}\n", nobo_calendar_mirror::COLOR_KEYWORD),
        None => String::new(),
    };
    format!(
        "#+TITLE: {}\n{kind_declaration}\n{URL_KEYWORD} {}\n{USER_KEYWORD} {}\n{color}{comment}\n\n",
        subscription.name, subscription.url, subscription.username
    )
}

/// Unsubscribing: the file is the subscription, so removing it is all of
/// it -- and the vault sync carries that to the other devices.
pub fn remove(vault_root: &Path, subscription: &CalDavSubscription) -> Result<(), String> {
    match std::fs::remove_file(subscription.file(vault_root)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("could not remove the subscription file: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vault() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    fn subscribe(root: &Path, kind: CalDavKind, name: &str) -> CalDavSubscription {
        let subscription = subscription_for(kind, name, "dennis", "https://cloud.example.com/dav/cal/dennis/einkauf/");
        write_empty(root, &subscription).unwrap();
        subscription
    }

    /// The round trip everything rests on: what one device wrote, another
    /// reads back as the same subscription -- with no settings involved.
    #[test]
    fn a_written_subscription_reads_back_out_of_its_file() {
        let vault = vault();
        let written = subscribe(vault.path(), CalDavKind::Tasks, "Einkauf");

        let found = list(vault.path());
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, written.id);
        assert_eq!(found[0].name, "Einkauf");
        assert_eq!(found[0].username, "dennis");
        assert_eq!(found[0].kind, CalDavKind::Tasks);
        assert_eq!(found[0].account_id, written.account_id);
    }

    #[test]
    fn a_calendar_reads_back_as_a_calendar_and_lives_with_the_other_calendars() {
        let vault = vault();
        let written = subscribe(vault.path(), CalDavKind::Events, "Familie");

        assert!(written.file(vault.path()).starts_with(nobo_calendar_mirror::dir(vault.path())));
        let found = list(vault.path());
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].kind, CalDavKind::Events);
    }

    /// One collection, both components: two files, or the two syncs
    /// overwrite each other.
    #[test]
    fn subscribing_to_tasks_and_events_of_one_collection_gives_two_files() {
        let vault = vault();
        subscribe(vault.path(), CalDavKind::Tasks, "Einkauf");
        subscribe(vault.path(), CalDavKind::Events, "Einkauf");

        assert_eq!(list(vault.path()).len(), 2);
    }

    /// The ICS feeds share the calendars directory and must not be read as
    /// CalDAV subscriptions with an empty user.
    #[test]
    fn an_ics_mirror_is_not_a_caldav_subscription() {
        let vault = vault();
        let feed = nobo_calendar::registry::subscription_for("Feiertage", "https://example.com/cal.ics");
        nobo_calendar::registry::write_without_events(vault.path(), &feed).unwrap();

        assert!(list(vault.path()).is_empty());
        // ... and the other way round, or the two registries would fight
        // over one file.
        subscribe(vault.path(), CalDavKind::Events, "Familie");
        assert_eq!(nobo_calendar::registry::list(vault.path()).len(), 1);
    }

    #[test]
    fn unsubscribing_removes_the_file_and_is_idempotent() {
        let vault = vault();
        let subscription = subscribe(vault.path(), CalDavKind::Tasks, "Einkauf");

        remove(vault.path(), &subscription).unwrap();
        remove(vault.path(), &subscription).unwrap();

        assert!(list(vault.path()).is_empty());
    }

    /// Every device derives the same account for one server, which is what
    /// lets it find the password its own keychain holds.
    #[test]
    fn the_account_is_derived_from_the_file_not_stored_in_it() {
        let vault = vault();
        subscribe(vault.path(), CalDavKind::Tasks, "Einkauf");

        let found = &list(vault.path())[0];
        assert_eq!(
            found.account_id,
            identity::account_id("dennis", "https://cloud.example.com/dav/cal/dennis/einkauf/")
        );
    }
}
