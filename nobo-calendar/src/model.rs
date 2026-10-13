//! One calendar entry, decoupled from both the ICS wire format and the org
//! text it becomes.

use serde::{Deserialize, Serialize};

/// One subscribed ICS calendar feed.
///
/// Not a settings type: the subscription lives in the mirrored file in the
/// vault, and this is what that file's header says (see
/// nobo_calendar::registry). `id` names the file, kept separate from
/// `name` so renaming a calendar does not orphan its file and resurrect
/// the old one as a second copy.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "tauri", derive(specta::Type))]
pub struct CalendarSubscription {
    pub id: String,
    pub name: String,
    pub url: String,
    /// Disabled keeps the subscription but stops fetching it, and its
    /// entries leave the agenda -- see registry::write_without_events.
    #[serde(default = "enabled_by_default")]
    pub enabled: bool,
    /// What its entries are marked with in the agenda, as a CSS colour.
    /// `None` until someone picks one.
    #[serde(default)]
    pub color: Option<String>,
    /// How often THIS calendar is fetched by itself.
    ///
    /// Per calendar, because that is how calendars are: a shared duty
    /// roster is worth asking about every half hour, the public holidays
    /// change twice a year, and a one-off conference schedule never needs
    /// asking again. One number for all of them is either too often for
    /// most or too rare for the one that matters.
    ///
    /// Stored in the mirror file's header like everything else a
    /// subscription consists of (see registry), so it travels with the
    /// vault: set it once, and every device fetches that calendar the way
    /// you said.
    #[serde(default)]
    pub refresh: RefreshInterval,
}

fn enabled_by_default() -> bool {
    true
}

/// What an entry is, and what marks a calendar's colour, both belong to
/// the mirror format rather than to subscriptions (see
/// nobo_calendar_mirror). Re-exported so this feature's own modules read
/// as they did.
pub use nobo_calendar_mirror::{CalendarEvent, COLOR_KEYWORD};

/// What one refresh did, per calendar -- surfaced in Settings so a feed
/// that quietly stopped resolving is visible rather than just empty.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "tauri", derive(specta::Type))]
pub struct CalendarRefreshResult {
    pub id: String,
    pub name: String,
    #[cfg_attr(feature = "tauri", specta(type = specta_typescript::Number))]
    pub event_count: usize,
    /// True when the server said the feed has not changed since the last
    /// download -- nothing was fetched, parsed or written.
    pub unchanged: bool,
    /// `None` on success.
    pub error: Option<String>,
}

/// How often the ICS subscriptions are refreshed on this device.
///
/// A separate answer from the CalDAV lists on purpose, and that is the
/// point of having it at all: a shopping list two people share is worth
/// asking about every few minutes, a public holiday calendar is not. The
/// same five-minute clock served both, so a feed that changes twice a year
/// was fetched a hundred times a day -- cheap per request (see
/// `sync::fetch`, which sends the validators and gets a 304 with no body),
/// but not free, and not free for the server either.
///
/// Stored as a name rather than a number of minutes: a settings file is
/// read by people, and `every-8-hours` says what it means where `480`
/// does not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "tauri", derive(specta::Type))]
#[serde(rename_all = "kebab-case")]
pub enum RefreshInterval {
    /// Once when the app starts, and not again while it runs.
    OnEveryStart,
    // Spelled out, because kebab-case leaves no dash before a digit
    // ("every5-minutes") and this string is read by whoever opens
    // settings.json.
    #[serde(rename = "every-5-minutes")]
    Every5Minutes,
    #[serde(rename = "every-10-minutes")]
    Every10Minutes,
    #[serde(rename = "every-15-minutes")]
    Every15Minutes,
    #[serde(rename = "every-30-minutes")]
    Every30Minutes,
    Hourly,
    #[serde(rename = "every-2-hours")]
    Every2Hours,
    /// The default: a feed that changed is on screen within a workday's
    /// coffee break, and a device that stays open all day makes six
    /// requests instead of nearly three hundred.
    #[default]
    #[serde(rename = "every-4-hours")]
    Every4Hours,
    #[serde(rename = "every-8-hours")]
    Every8Hours,
    Daily,
    Monthly,
    Yearly,
    /// Only when somebody asks for it.
    Never,
}

impl RefreshInterval {
    /// How long to wait between refreshes, or `None` for the two answers
    /// that are not a duration.
    pub fn every(self) -> Option<chrono::Duration> {
        let minutes = match self {
            RefreshInterval::Every5Minutes => 5,
            RefreshInterval::Every10Minutes => 10,
            RefreshInterval::Every15Minutes => 15,
            RefreshInterval::Every30Minutes => 30,
            RefreshInterval::Hourly => 60,
            RefreshInterval::Every2Hours => 2 * 60,
            RefreshInterval::Every4Hours => 4 * 60,
            RefreshInterval::Every8Hours => 8 * 60,
            RefreshInterval::Daily => 24 * 60,
            // Calendar months and years differ in length; for "how long
            // ago was the last one" the average is what matters, and being
            // a day out on a yearly refresh is not a thing anybody can
            // notice.
            RefreshInterval::Monthly => 30 * 24 * 60,
            RefreshInterval::Yearly => 365 * 24 * 60,
            RefreshInterval::OnEveryStart | RefreshInterval::Never => return None,
        };
        Some(chrono::Duration::minutes(minutes))
    }
}

/// Why a refresh was asked for. The interval above only means anything
/// against this: "once a day" has to let the app's own start through, and
/// a person pressing the button is never turned away.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "tauri", derive(specta::Type))]
#[serde(rename_all = "kebab-case")]
pub enum RefreshReason {
    /// The app just started, or came back to the foreground.
    Startup,
    /// The clock that ticks while the app is open.
    Periodic,
    /// Somebody pressed refresh.
    Manual,
}

/// This feature's own settings (see nobo_core::config::section).
///
/// Only one thing is in here, and only because it is not a property of the
/// calendar but of this device: when it last fetched each of them. The
/// interval itself lives in the mirror file (see
/// `CalendarSubscription::refresh`) and is therefore the same on every
/// device; when a particular phone last managed to ask is not.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "tauri", derive(specta::Type))]
pub struct Settings {
    /// Subscription id -> epoch milliseconds of its last fetch.
    ///
    /// JS numbers on the wire -- epoch milliseconds stay exact there until
    /// the year 287396 (see specta's BigInt rule, and sync's
    /// `last_synced_at`, which is the same value for the same reason).
    #[serde(default)]
    #[cfg_attr(feature = "tauri", specta(type = std::collections::HashMap<String, specta_typescript::Number>))]
    pub last_refresh_at: std::collections::HashMap<String, i64>,
}

impl RefreshInterval {
    /// Whether a calendar set to this should be fetched now.
    ///
    /// The whole decision in one place, so no caller has to remember the
    /// rules -- and so they can be read at once:
    ///
    /// - a person asking always gets one,
    /// - `Never` means never, except that,
    /// - `OnEveryStart` refreshes when the app starts and never on the
    ///   clock,
    /// - everything else is "long enough since the last one", whichever
    ///   reason asks.
    pub fn due(self, reason: RefreshReason, last_ms: Option<i64>, now_ms: i64) -> bool {
        if reason == RefreshReason::Manual {
            return true;
        }
        match self {
            RefreshInterval::Never => false,
            RefreshInterval::OnEveryStart => reason == RefreshReason::Startup,
            interval => {
                let Some(every) = interval.every() else { return false };
                let Some(last) = last_ms else { return true };
                now_ms - last >= every.num_milliseconds()
            }
        }
    }
}

#[cfg(test)]
mod refresh_interval_tests {
    use super::*;

    const MINUTE: i64 = 60_000;
    const HOUR: i64 = 60 * MINUTE;

    const NOW: i64 = 1_000_000_000;

    /// One calendar, set to INTERVAL and last fetched LAST_MS_AGO ago.
    fn calendar(interval: RefreshInterval, last_ms_ago: Option<i64>) -> (RefreshInterval, Option<i64>) {
        (interval, last_ms_ago.map(|ago| NOW - ago))
    }

    fn due(set: (RefreshInterval, Option<i64>), reason: RefreshReason) -> bool {
        set.0.due(reason, set.1, NOW)
    }

    #[test]
    fn pressing_the_button_always_refreshes_whatever_is_set() {
        for interval in [RefreshInterval::Never, RefreshInterval::Yearly, RefreshInterval::OnEveryStart] {
            assert!(due(calendar(interval, Some(MINUTE)), RefreshReason::Manual), "{interval:?}");
        }
    }

    #[test]
    fn never_means_never_otherwise() {
        let never = calendar(RefreshInterval::Never, None);

        assert!(!due(never, RefreshReason::Startup));
        assert!(!due(never, RefreshReason::Periodic));
    }

    /// "On every start" is about the start, not about time: opening the
    /// app twice in a minute refreshes twice, and leaving it open all day
    /// refreshes once.
    #[test]
    fn on_every_start_ignores_the_clock_in_both_directions() {
        let on_start = calendar(RefreshInterval::OnEveryStart, Some(MINUTE));

        assert!(due(on_start, RefreshReason::Startup));
        assert!(!due(on_start, RefreshReason::Periodic));
    }

    #[test]
    fn an_interval_waits_for_its_time_whoever_asks() {
        let every_four_hours = |ago| calendar(RefreshInterval::Every4Hours, Some(ago));

        assert!(!due(every_four_hours(3 * HOUR), RefreshReason::Periodic));
        assert!(!due(every_four_hours(3 * HOUR), RefreshReason::Startup), "a restart is not a way around it");
        assert!(due(every_four_hours(4 * HOUR), RefreshReason::Periodic), "exactly due counts as due");
        assert!(due(every_four_hours(9 * HOUR), RefreshReason::Startup));
    }

    /// A device that has never refreshed: whatever the interval says, the
    /// first pass has to happen or the calendars stay empty.
    #[test]
    fn a_device_that_never_refreshed_is_due_at_once() {
        assert!(due(calendar(RefreshInterval::Yearly, None), RefreshReason::Startup));
        assert!(due(calendar(RefreshInterval::Monthly, None), RefreshReason::Periodic));
    }

    #[test]
    fn the_default_is_four_hours() {
        assert_eq!(RefreshInterval::default(), RefreshInterval::Every4Hours);
        assert_eq!(RefreshInterval::default().every(), Some(chrono::Duration::hours(4)));
    }

    /// The point of moving this into the file: two calendars in one vault
    /// answer differently at the same moment.
    #[test]
    fn two_calendars_are_asked_at_their_own_pace() {
        let often = calendar(RefreshInterval::Every30Minutes, Some(31 * MINUTE));
        let rarely = calendar(RefreshInterval::Yearly, Some(31 * MINUTE));

        assert!(due(often, RefreshReason::Periodic), "the duty roster is due");
        assert!(!due(rarely, RefreshReason::Periodic), "the holidays are not");
    }
}
