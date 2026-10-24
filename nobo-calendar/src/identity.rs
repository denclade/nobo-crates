// SPDX-FileCopyrightText: 2026 Dennis Burgermeister
// SPDX-License-Identifier: GPL-3.0-or-later

//! How an ICS subscription is named locally.
//!
//! Its own module for the same reason caldav::identity is one: the id
//! names a file in the SYNCED vault, so what it is derived from is a
//! design decision with consequences, not an implementation detail of
//! whatever happens to compute it.

/// Namespace for calendar-feed ids -- its own, so a value here can never
/// collide with a CalDAV list id (see caldav::identity).
const FEED_NAMESPACE: uuid::Uuid = uuid::uuid!("b48c05e7-6f21-4a93-8d7c-51e2a0f6c39d");

/// The id for the feed at URL.
///
/// Derived, not rolled per device, for the same reason as a CalDAV list's
/// (see caldav::identity::subscription_id, which explains the whole
/// problem): the id names a file in the SYNCED vault, so two devices
/// subscribing to one feed used to end up mirroring it twice -- every
/// event duplicated in the agenda. Simpler here than over there, because a
/// feed is identified by nothing but its URL: no account, no credentials,
/// and the mirror is purely derived from what was fetched.
pub fn subscription_id(url: &str) -> String {
    // Normalized FIRST, and with the same function the fetch uses: a
    // calendar hands out `webcal://` links, a browser hands out the
    // `https://` one for the same file, and hashing them as written gave
    // two ids, two mirror files and every event twice in the agenda --
    // exactly what deriving the id is supposed to prevent.
    let normalized = super::sync::normalize_url(url);
    uuid::Uuid::new_v5(&FEED_NAMESPACE, normalized.trim().trim_end_matches('/').as_bytes()).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The point of deriving it: two devices subscribing to one feed name
    /// the same file, instead of mirroring it twice into the synced vault.
    #[test]
    fn the_same_feed_always_gets_the_same_id() {
        let url = "https://example.com/cal/basic.ics";
        assert_eq!(subscription_id(url), subscription_id(url));
        assert_eq!(subscription_id(url), subscription_id("  https://example.com/cal/basic.ics/ "));
    }

    /// A calendar app offers `webcal://`, its web page offers `https://`,
    /// and both are the same file. Subscribing on two devices from two
    /// places must still be one subscription.
    #[test]
    fn the_same_feed_under_its_two_schemes_is_one_subscription() {
        assert_eq!(subscription_id("webcal://example.com/cal/basic.ics"), subscription_id("https://example.com/cal/basic.ics"),);
    }

    #[test]
    fn different_feeds_stay_apart() {
        assert_ne!(subscription_id("https://example.com/cal/a.ics"), subscription_id("https://example.com/cal/b.ics"),);
    }

    #[test]
    fn the_id_is_a_plain_uuid_so_it_is_safe_as_a_file_name() {
        let id = subscription_id("https://example.com/cal/a.ics?token=x/y");
        assert!(uuid::Uuid::parse_str(&id).is_ok(), "{id}");
    }
}
