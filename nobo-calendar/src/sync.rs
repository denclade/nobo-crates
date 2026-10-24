// SPDX-FileCopyrightText: 2026 Dennis Burgermeister
// SPDX-License-Identifier: GPL-3.0-or-later

//! Fetching subscriptions and mirroring them into the vault.

use std::path::Path;

use crate::model::CalendarSubscription;

use super::{org, store};
use rusqlite::Connection;
use std::sync::Mutex;

pub use nobo_calendar_mirror::{dir as calendars_dir, is_mirror_file as is_calendar_file};

/// Downloads one feed and rewrites its file. Returns how many events
/// landed.
///
/// The file is written whole, never patched: the feed is the only source
/// of truth here, so there is no local edit to preserve -- and a full
/// rewrite is what makes a deleted remote event actually disappear.
pub async fn refresh_one(
    client: &reqwest::Client,
    db: &Mutex<Connection>,
    vault_root: &Path,
    subscription: &CalendarSubscription,
) -> Result<Refreshed, String> {
    let target = super::registry::file_for(vault_root, &subscription.id);
    let known = {
        let conn = db.lock().map_err(|_| "database lock poisoned".to_string())?;
        store::get(&conn, &subscription.id).map_err(|e| e.to_string())?
    };
    // The validators are only worth anything if the file they describe is
    // actually there: a mirror the user deleted, or one that never arrived
    // on this device, has to be rebuilt however current the server says it
    // is.
    let known = known.filter(|_| target.exists());

    let Some(body) = fetch(client, &subscription.url, known.as_ref()).await? else {
        let event_count = known.map(|state| state.event_count).unwrap_or_default();
        return Ok(Refreshed { event_count, unchanged: true });
    };

    let mut events = nobo_calendar_mirror::ics::parse_calendar(&body.text)?;
    // Chronological, so the file reads like a calendar and a diff between
    // two refreshes stays small.
    events.sort_by(|a, b| a.start.cmp(&b.start).then_with(|| a.uid.cmp(&b.uid)));

    // The second wall, for a server that sends no validators at all: it
    // answered 200 and we parsed the feed, but the file it would produce
    // is the one already on disk. Writing it anyway would cost a reindex,
    // an `index-updated` every screen listens to, and a changed mtime for
    // the vault sync to look at -- for a byte-identical file. One read of
    // a file we were about to overwrite is the cheapest part of this
    // function (see core::history's own measurements).
    let rendered = org::render_calendar(subscription, &events);
    let already_there = std::fs::read_to_string(&target).is_ok_and(|current| current == rendered);

    if !already_there {
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("could not create {}: {e}", parent.display()))?;
        }
        nobo_core::utils::fs_utils::write_atomic(&target, &rendered).map_err(|e| format!("could not write {}: {e}", target.display()))?;
    }

    {
        let conn = db.lock().map_err(|_| "database lock poisoned".to_string())?;
        let state = store::FeedState { etag: body.etag, last_modified: body.last_modified, event_count: events.len() };
        store::set(&conn, &subscription.id, &state).map_err(|e| e.to_string())?;
    }

    // "Unchanged" is about the vault, not about the request: a feed
    // without an ETag is downloaded every time, and saying "changed" for
    // it would wake the whole app on every pass.
    Ok(Refreshed { event_count: events.len(), unchanged: already_there })
}

/// What one feed refresh did.
pub struct Refreshed {
    pub event_count: usize,
    pub unchanged: bool,
}

/// A downloaded feed, with what identifies this version of it.
struct Body {
    text: String,
    etag: Option<String>,
    last_modified: Option<String>,
}

/// `webcal://` is what calendar apps hand out for a subscription URL; it
/// is plain HTTPS with a scheme that tells the OS to subscribe rather than
/// download. Rewritten rather than rejected, so pasting the link a
/// calendar offers just works.
pub(crate) fn normalize_url(url: &str) -> String {
    match url.strip_prefix("webcal://") {
        Some(rest) => format!("https://{rest}"),
        None => url.to_string(),
    }
}

/// Downloads the feed, or `None` when the server says it has not changed.
///
/// A conditional GET, not a HEAD followed by a GET: it is one round trip
/// either way, where HEAD-then-GET is two whenever the feed HAS changed --
/// and a 304 carries no body at all, so an unchanged feed costs the
/// request and nothing else. No download, no parse, no write, and nothing
/// for the vault sync to carry afterwards.
async fn fetch(client: &reqwest::Client, url: &str, known: Option<&store::FeedState>) -> Result<Option<Body>, String> {
    let mut request = client.get(normalize_url(url));
    if let Some(state) = known {
        // Both, where both are known: a server may honour either, and
        // sending one it does not use costs nothing.
        if let Some(etag) = &state.etag {
            request = request.header(reqwest::header::IF_NONE_MATCH, etag);
        }
        if let Some(modified) = &state.last_modified {
            request = request.header(reqwest::header::IF_MODIFIED_SINCE, modified);
        }
    }

    let response = request.send().await.map_err(|e| format!("could not reach the calendar: {}", nobo_http::request_failed(&e)))?;

    if response.status() == reqwest::StatusCode::NOT_MODIFIED {
        return Ok(None);
    }
    if !response.status().is_success() {
        return Err(format!("the calendar answered HTTP {}", response.status()));
    }

    let header = |name: reqwest::header::HeaderName| response.headers().get(name).and_then(|value| value.to_str().ok()).map(String::from);
    let etag = header(reqwest::header::ETAG);
    let last_modified = header(reqwest::header::LAST_MODIFIED);
    let text = response.text().await.map_err(|e| format!("could not read the calendar: {e}"))?;

    Ok(Some(Body { text, etag, last_modified }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `webcal://` is what a calendar app hands out for a subscription
    /// URL; it is plain HTTPS with a scheme that tells the OS to
    /// subscribe. Rewritten rather than rejected, so pasting the link a
    /// calendar offers just works -- and normalized in one place, because
    /// the id is derived from it too (see calendar::identity).
    #[test]
    fn webcal_is_the_same_address_as_https() {
        assert_eq!(normalize_url("webcal://example.com/cal.ics"), "https://example.com/cal.ics");
        assert_eq!(normalize_url("https://example.com/cal.ics"), "https://example.com/cal.ics");
    }

    /// A server that sends no ETag and no Last-Modified at all -- more
    /// common than it should be. The download cannot be avoided, but the
    /// rest can: the file it would produce is the one already on disk, so
    /// nothing is written, nothing is reindexed and no screen in the app
    /// wakes up.
    #[tokio::test]
    async fn a_feed_without_validators_is_not_written_again_when_it_did_not_change() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        const ICS: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:a\r\n\
SUMMARY:Neujahr\r\nDTSTART;VALUE=DATE:20260101\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";

        let server = MockServer::start().await;
        // Same body every time, and not a validator in sight.
        Mock::given(method("GET")).and(path("/cal.ics")).respond_with(ResponseTemplate::new(200).set_body_string(ICS)).mount(&server).await;

        let vault = tempfile::tempdir().unwrap();
        let db = Mutex::new(Connection::open_in_memory().unwrap());
        store::init_schema(&db.lock().unwrap()).unwrap();
        let subscription = super::super::registry::subscription_for("Feiertage", &format!("{}/cal.ics", server.uri()));
        let client = reqwest::Client::new();

        let first = refresh_one(&client, &db, vault.path(), &subscription).await.unwrap();
        assert!(!first.unchanged, "the first pass writes the mirror");

        let target = super::super::registry::file_for(vault.path(), &subscription.id);
        let written_at = std::fs::metadata(&target).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));

        let second = refresh_one(&client, &db, vault.path(), &subscription).await.unwrap();

        assert!(second.unchanged, "the same feed produced the same file -- nothing changed for the vault");
        assert_eq!(second.event_count, 1);
        assert_eq!(
            std::fs::metadata(&target).unwrap().modified().unwrap(),
            written_at,
            "the mirror was rewritten with byte-identical content",
        );
    }

    /// The point of the whole thing: a feed that has not changed costs one
    /// request and nothing else -- no body, no parse, no write. A holiday
    /// calendar is a megabyte or two that changes a few times a year, and
    /// downloading it on every refresh is the actual battery cost of
    /// calendar subscriptions.
    #[tokio::test]
    async fn an_unchanged_feed_is_not_downloaded_again() {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        const ICS: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:a\r\n\
SUMMARY:Neujahr\r\nDTSTART;VALUE=DATE:20260101\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";

        let server = MockServer::start().await;
        // First download: no validator sent, body and an ETag come back.
        Mock::given(method("GET"))
            .and(path("/cal.ics"))
            .and(wiremock::matchers::header_exists("host"))
            .respond_with(ResponseTemplate::new(200).set_body_string(ICS).insert_header("ETag", "\"v1\""))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        // Every later one has to carry the ETag, and gets a 304.
        Mock::given(method("GET"))
            .and(path("/cal.ics"))
            .and(header("If-None-Match", "\"v1\""))
            .respond_with(ResponseTemplate::new(304))
            .mount(&server)
            .await;

        let vault = tempfile::tempdir().unwrap();
        let db = Mutex::new(Connection::open_in_memory().unwrap());
        store::init_schema(&db.lock().unwrap()).unwrap();
        let subscription = super::super::registry::subscription_for("Feiertage", &format!("{}/cal.ics", server.uri()));
        let client = reqwest::Client::new();

        let first = refresh_one(&client, &db, vault.path(), &subscription).await.unwrap();
        assert_eq!((first.event_count, first.unchanged), (1, false));
        let written = std::fs::metadata(super::super::registry::file_for(vault.path(), &subscription.id)).unwrap().modified().unwrap();

        let second = refresh_one(&client, &db, vault.path(), &subscription).await.unwrap();

        assert!(second.unchanged, "the second refresh downloaded the feed again");
        // ... and still says what is in there, from what the first one
        // recorded.
        assert_eq!(second.event_count, 1);
        assert_eq!(
            std::fs::metadata(super::super::registry::file_for(vault.path(), &subscription.id)).unwrap().modified().unwrap(),
            written,
            "the mirror was rewritten for an unchanged feed"
        );
    }

    /// ... unless the mirror is gone: a validator describes a file, and
    /// without the file there is nothing for it to vouch for.
    #[tokio::test]
    async fn a_missing_mirror_is_fetched_again_even_when_unchanged() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        const ICS: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:a\r\n\
SUMMARY:Neujahr\r\nDTSTART;VALUE=DATE:20260101\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/cal.ics"))
            .respond_with(ResponseTemplate::new(200).set_body_string(ICS).insert_header("ETag", "\"v1\""))
            .mount(&server)
            .await;

        let vault = tempfile::tempdir().unwrap();
        let db = Mutex::new(Connection::open_in_memory().unwrap());
        store::init_schema(&db.lock().unwrap()).unwrap();
        let subscription = super::super::registry::subscription_for("Feiertage", &format!("{}/cal.ics", server.uri()));
        let client = reqwest::Client::new();

        refresh_one(&client, &db, vault.path(), &subscription).await.unwrap();
        std::fs::remove_file(super::super::registry::file_for(vault.path(), &subscription.id)).unwrap();

        let again = refresh_one(&client, &db, vault.path(), &subscription).await.unwrap();

        assert!(!again.unchanged);
        assert!(super::super::registry::file_for(vault.path(), &subscription.id).exists());
    }
}
