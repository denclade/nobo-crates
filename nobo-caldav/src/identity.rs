//! How a subscription is named locally, derived from what it subscribes TO
//! rather than rolled per device.
//!
//! The id names the mirrored file under `.nobo/caldav/`, and that file
//! lives in the synced vault. A random id per device meant the same remote
//! list arrived as two files -- every task twice in Next Actions and
//! search, and edits made in the copy the device does not own going
//! nowhere (that device only pushes the lists in its own settings.json,
//! which is not synced). Deriving the id makes both devices name the one
//! file, which the file format was already built for: the ETag stays out
//! of it precisely so the vault sync and the CalDAV sync can share it (see
//! org.rs), the headings carry stable CUSTOM_IDs so a three-way merge
//! lines up, and pushes are If-Match guarded.

/// Namespace for list ids -- its own, so a value here can never collide
/// with a heading's CUSTOM_ID (see org::custom_id_for, which has its own).
const LIST_NAMESPACE: uuid::Uuid = uuid::uuid!("6f1d9a3c-2b47-4e58-9a10-7c3e5d84f2b1");

/// Namespace for calendar (VEVENT) ids. Separate from the list namespace
/// above because one collection commonly offers both components -- a
/// Nextcloud calendar takes VEVENT and VTODO -- and subscribing to both
/// must produce two files, not one that two syncs overwrite in turn.
const CALENDAR_NAMESPACE: uuid::Uuid = uuid::uuid!("0c9f2b6e-58a4-4d31-b7e0-4a8d16c5f3b2");

/// Namespace for account ids, i.e. keychain keys.
const ACCOUNT_NAMESPACE: uuid::Uuid = uuid::uuid!("d51e7f84-3a29-4c60-8bb5-9f2e0d7a4c16");

/// The id for the collection at COLLECTION_URL on the account logged in as
/// USERNAME.
///
/// Deliberately keyed on the whole URL, host included, not just the path.
/// The same person can hold accounts on two servers whose DAV layout AND
/// username match -- two Nextclouds both serving
/// `/remote.php/dav/calendars/dennis/personal/` is not exotic -- and
/// giving those one id would merge two unrelated lists into one file.
/// The price is the opposite error: reaching one server under two
/// addresses (LAN vs. DDNS) on two devices yields two ids again. That is
/// the mistake worth making of the two -- it degrades to the old
/// duplicate-file behaviour, while a wrong merge destroys data.
///
/// Only the noise a server or a user types inconsistently is normalized
/// away: case in scheme and host (both case-insensitive per RFC 3986),
/// the default port, and a trailing slash.
pub fn subscription_id(username: &str, collection_url: &str) -> String {
    let key = format!("{}\n{}", username.trim(), normalize_url(collection_url));
    uuid::Uuid::new_v5(&LIST_NAMESPACE, key.as_bytes()).to_string()
}

/// The same, for a calendar subscription to the collection -- see
/// CALENDAR_NAMESPACE on why it cannot be the same value.
pub fn calendar_subscription_id(username: &str, collection_url: &str) -> String {
    let key = format!("{}\n{}", username.trim(), normalize_url(collection_url));
    uuid::Uuid::new_v5(&CALENDAR_NAMESPACE, key.as_bytes()).to_string()
}

/// Which login a subscription needs, derived from the subscription itself.
///
/// This is what makes credentials work per device without a device having
/// to be told anything: a mirror file names its collection and its user,
/// every device computes the same account id from those two, and looks in
/// its own keychain under it. A device that finds nothing simply cannot
/// sync that one subscription (see caldav::registry) -- it does not lose
/// the subscription, and no other device is affected.
///
/// Keyed on the server, not the collection: one password opens all of a
/// server's collections, and asking again per list would be busywork. Two
/// accounts on one server stay apart through the username.
pub fn account_id(username: &str, collection_url: &str) -> String {
    let key = format!("{}\n{}", username.trim(), server_of(collection_url));
    uuid::Uuid::new_v5(&ACCOUNT_NAMESPACE, key.as_bytes()).to_string()
}

/// Scheme and host of a URL, normalized like the rest of it -- the
/// address a login belongs to, as opposed to the collection it opens.
pub fn server_of(url: &str) -> String {
    let normalized = normalize_url(url);
    match normalized.split_once("://") {
        Some((scheme, rest)) => format!("{scheme}://{}", rest.split('/').next().unwrap_or(rest)),
        None => normalized,
    }
}

fn normalize_url(url: &str) -> String {
    let trimmed = url.trim().trim_end_matches('/');
    let Some((scheme, rest)) = trimmed.split_once("://") else {
        return trimmed.to_string();
    };
    let scheme = scheme.to_ascii_lowercase();
    let (authority, path) = rest.split_once('/').map_or((rest, ""), |(a, p)| (a, p));
    let authority = authority.to_ascii_lowercase();
    let authority = match scheme.as_str() {
        "https" => authority.strip_suffix(":443").unwrap_or(&authority),
        "http" => authority.strip_suffix(":80").unwrap_or(&authority),
        _ => &authority,
    };
    if path.is_empty() { format!("{scheme}://{authority}") } else { format!("{scheme}://{authority}/{path}") }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole point: two devices, one subscription, one id -- and with
    /// it one file in the synced vault.
    #[test]
    fn the_same_collection_always_gets_the_same_id() {
        let a = subscription_id("dennis", "https://cloud.example.com/remote.php/dav/calendars/dennis/personal/");
        let b = subscription_id("dennis", "https://cloud.example.com/remote.php/dav/calendars/dennis/personal/");
        assert_eq!(a, b);
    }

    #[test]
    fn a_trailing_slash_the_port_and_the_hosts_case_are_noise() {
        let canonical = subscription_id("dennis", "https://cloud.example.com/remote.php/dav/calendars/dennis/personal/");
        for variant in [
            "https://cloud.example.com/remote.php/dav/calendars/dennis/personal",
            "https://Cloud.Example.COM/remote.php/dav/calendars/dennis/personal/",
            "HTTPS://cloud.example.com/remote.php/dav/calendars/dennis/personal/",
            "https://cloud.example.com:443/remote.php/dav/calendars/dennis/personal/",
            "  https://cloud.example.com/remote.php/dav/calendars/dennis/personal/  ",
        ] {
            assert_eq!(subscription_id("dennis", variant), canonical, "{variant}");
        }
    }

    /// The path IS case-sensitive -- only scheme and host are not.
    #[test]
    fn two_collections_on_one_server_stay_apart() {
        let personal = subscription_id("dennis", "https://cloud.example.com/dav/calendars/dennis/personal/");
        let work = subscription_id("dennis", "https://cloud.example.com/dav/calendars/dennis/Personal/");
        assert_ne!(personal, work);
    }

    /// Why the host is part of the key: same layout, same username, two
    /// servers. Merging these into one file would mix two people's -- or
    /// two lives' -- task lists together.
    #[test]
    fn the_same_path_on_a_different_server_is_a_different_list() {
        let path = "/remote.php/dav/calendars/dennis/personal/";
        assert_ne!(
            subscription_id("dennis", &format!("https://privat.example.com{path}")),
            subscription_id("dennis", &format!("https://arbeit.example.com{path}")),
        );
    }

    #[test]
    fn two_accounts_on_one_server_stay_apart() {
        let url = "https://cloud.example.com/remote.php/dav/calendars/shared/team/";
        assert_ne!(subscription_id("dennis", url), subscription_id("anna", url));
    }

    #[test]
    fn the_id_is_a_plain_uuid_so_it_is_safe_as_a_file_name() {
        let id = subscription_id("dennis", "https://cloud.example.com/dav/x/");
        assert!(uuid::Uuid::parse_str(&id).is_ok(), "{id}");
        assert!(id.chars().all(|c| c.is_ascii_hexdigit() || c == '-'), "{id}");
    }

    /// One login per server: every collection on it resolves to the same
    /// keychain entry, so a user enters the password once.
    #[test]
    fn every_collection_on_one_server_shares_one_account() {
        let personal = account_id("dennis", "https://cloud.example.com/remote.php/dav/calendars/dennis/personal/");
        let work = account_id("dennis", "https://cloud.example.com/remote.php/dav/calendars/dennis/work/");
        assert_eq!(personal, work);
    }

    #[test]
    fn two_users_or_two_servers_are_two_accounts() {
        let mine = account_id("dennis", "https://cloud.example.com/dav/calendars/dennis/personal/");
        let theirs = account_id("alex", "https://cloud.example.com/dav/calendars/alex/personal/");
        let elsewhere = account_id("dennis", "https://other.example.com/dav/calendars/dennis/personal/");
        assert_ne!(mine, theirs);
        assert_ne!(mine, elsewhere);
    }

    /// A collection that serves both components is two subscriptions and
    /// therefore two files -- one id for both would have the task sync and
    /// the calendar sync overwrite each other's file.
    #[test]
    fn tasks_and_events_in_one_collection_are_two_subscriptions() {
        let url = "https://cloud.example.com/dav/calendars/dennis/personal/";
        assert_ne!(subscription_id("dennis", url), calendar_subscription_id("dennis", url));
    }

    /// The same normalization as the list id, or a device would compute a
    /// different account for the same server.
    #[test]
    fn the_account_ignores_the_noise_a_url_is_typed_with() {
        let canonical = account_id("dennis", "https://cloud.example.com/dav/calendars/dennis/personal/");
        for spelling in [
            "https://Cloud.Example.COM/dav/calendars/dennis/personal",
            "https://cloud.example.com:443/dav/calendars/dennis/other/",
            "  https://cloud.example.com/dav/  ",
        ] {
            assert_eq!(account_id("dennis", spelling), canonical, "{spelling}");
        }
    }
}
