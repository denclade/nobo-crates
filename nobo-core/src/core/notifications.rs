//! What the user is told when somebody else adds something, everywhere.
//!
//! In the core rather than among the features, because it is not one: no
//! cargo flag, no switch, no screen of its own, and two features (CalDAV
//! and the background worker) share it. A "feature" nobody can point at
//! or switch off is a service, and services live here -- the same reason
//! `nobo_http` and `nobo_webdav` do.
//!
//! One decision, five platforms. What is worth saying, and in which
//! words, is decided here exactly once (`messages`); how it reaches a
//! screen is the host's, and never this module's:
//!
//! | Where | How |
//! |---|---|
//! | The running Tauri app -- Linux, Windows, macOS, Android, iOS | `nobo_tauri::notifications::report`, through tauri-plugin-notification |
//! | Android with the app closed | the background worker: no `AppHandle` exists there, so it takes the finished messages over JNI and posts them itself (see `features::background`, `SyncWorker.kt`) |
//! | harbour-nobo (Qt/Silica) | its own delivery, over the same `messages` |
//!
//! Below that the differences stop being ours: a plugin talks to the XDG
//! notification service over D-Bus on Linux, WinRT toasts on Windows,
//! `UNUserNotificationCenter` on macOS and iOS, and the
//! `NotificationManager` on Android. Nothing above this module knows
//! which of those it is, and no caller composes notification text --
//! that was the actual duplication: the wording used to live in the
//! frontend AND in the worker's Kotlin, in two languages each.
//!
//! Permission is the one thing this module does not do, because it
//! cannot: Android needs a dialog, and a dialog needs a user looking at
//! the app. The frontend asks at the moment it makes sense -- right after
//! subscribing to something (see `store/notifications.ts`). Desktop has
//! no such concept and always answers yes.

use serde::Serialize;

/// One subscription that gained entries this device had never seen.
///
/// "Never seen" is the whole point: not our own pushes coming back, not
/// an entry that merely changed, and nothing on a first sync -- see
/// `ListSyncOutcome::arrived`, which is where the number comes from.
#[derive(Debug, Clone, PartialEq)]
pub struct Arrival {
    pub name: String,
    pub arrived: usize,
}

/// Whether the user is looking at the app right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Presence {
    Foreground,
    Background,
}

/// One finished notification, ready for whatever will post it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Message {
    /// Stable per subscription, so a second pass over the same list can
    /// replace its notification instead of stacking another one next to
    /// it. Honoured on Android; the desktop backends drop it (see
    /// `report`).
    pub id: i32,
    pub title: String,
    pub body: String,
}

/// What to tell the user about a finished sync -- possibly nothing.
///
/// Every rule that keeps this from becoming noise is in this one
/// function, so there is a single place to read them and a single place
/// to change them:
///
/// - **Nothing while the app is in the foreground.** The list is on
///   screen and updates itself; a notification for something the user can
///   already see is noise.
/// - **Only what arrived.** A subscription with nothing new says nothing.
/// - **One per subscription, with a count.** Three notifications for
///   three items is how an app gets muted.
pub fn messages(arrivals: &[Arrival], presence: Presence, language: &str) -> Vec<Message> {
    if presence == Presence::Foreground {
        return Vec::new();
    }
    arrivals
        .iter()
        .filter(|arrival| arrival.arrived > 0)
        .map(|arrival| Message { id: id_for(&arrival.name), title: arrival.name.clone(), body: body(arrival.arrived, language) })
        .collect()
}

/// The app's own language, not the device's.
///
/// nobo has a language setting, and a notification in a different
/// language from the app that sent it reads as somebody else's. Two
/// strings is few enough to keep here rather than reaching into the
/// frontend's message catalogue, which a background worker cannot load
/// anyway.
fn body(count: usize, language: &str) -> String {
    let german = language.split(['_', '-']).next() == Some("de");
    match (german, count) {
        (true, 1) => crate::t!("Ein neuer Eintrag", "One new entry"),
        (true, _) => crate::t!("{count} neue Einträge", "{count} new entries"),
        (false, 1) => "One new entry".to_string(),
        (false, _) => format!("{count} new entries"),
    }
}

/// A stable id per subscription name.
///
/// Spelled out (FNV-1a) rather than taken from `DefaultHasher`, whose
/// value is explicitly not promised to stay the same between runs -- and
/// an id that changes turns "replace that notification" into "post
/// another one".
fn id_for(name: &str) -> i32 {
    let mut hash: u32 = 0x811c_9dc5;
    for byte in name.as_bytes() {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    // Positive: Android takes an int id, and a negative one is legal but
    // reads like an error in every log that prints it.
    (hash >> 1) as i32
}

/// The language the messages should be in: the app's own setting, with
/// English for a device that has none. Shared by every delivery path,
/// Tauri or not.
pub fn configured_language() -> String {
    crate::config::settings::Config::load().map(|config| config.language).unwrap_or_else(|| "en_US".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arrival(name: &str, arrived: usize) -> Arrival {
        Arrival { name: name.to_string(), arrived }
    }

    #[test]
    fn one_message_per_subscription_that_gained_something() {
        let arrivals = [arrival("Einkauf", 3), arrival("Baumarkt", 0), arrival("Familie", 1)];

        let messages = messages(&arrivals, Presence::Background, "de_DE");

        assert_eq!(messages.len(), 2, "the one with nothing new says nothing");
        assert_eq!(messages[0].title, "Einkauf");
        let count = 3;
        assert_eq!(messages[0].body, crate::t!("{count} neue Einträge", "{count} new entries"));
        assert_eq!(messages[1].body, crate::t!("Ein neuer Eintrag", "One new entry"), "singular, not '1 neue Einträge'");
    }

    /// The list is on screen and updates itself; telling someone about
    /// something they can see is how an app gets muted.
    #[test]
    fn nothing_at_all_while_the_app_is_in_front() {
        let arrivals = [arrival("Einkauf", 3)];

        assert!(messages(&arrivals, Presence::Foreground, "de_DE").is_empty());
    }

    #[test]
    fn it_speaks_the_language_the_app_is_set_to() {
        let arrivals = [arrival("Shopping", 2), arrival("Errands", 1)];

        let messages = messages(&arrivals, Presence::Background, "en_US");

        assert_eq!(messages[0].body, "2 new entries");
        assert_eq!(messages[1].body, "One new entry");
    }

    /// An unknown language is not a reason to say nothing -- English is
    /// the app's other locale and a better answer than silence.
    #[test]
    fn an_unknown_language_falls_back_to_english() {
        let messages = messages(&[arrival("Shopping", 2)], Presence::Background, "fr_FR");

        assert_eq!(messages[0].body, "2 new entries");
    }

    /// The id is what makes the next pass REPLACE this notification
    /// instead of adding a second one next to it.
    #[test]
    fn the_same_subscription_keeps_its_id() {
        let first = messages(&[arrival("Einkauf", 1)], Presence::Background, "de_DE");
        let second = messages(&[arrival("Einkauf", 4)], Presence::Background, "de_DE");

        assert_eq!(first[0].id, second[0].id);
        assert_ne!(first[0].id, id_for("Baumarkt"));
        assert!(first[0].id > 0, "a negative id reads like an error in a log");
    }
}
