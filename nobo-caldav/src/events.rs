//! Read-only mirroring of a CalDAV calendar's events.
//!
//! Deliberately one-way, unlike the task lists next door. A VEVENT carries
//! attendees, invitations, recurrence exceptions and free/busy state --
//! writing one back means speaking a protocol this app does not speak, and
//! a half-spoken one would send wrong invitations to other people. Reading
//! is the useful half and it is complete: the entries land in the agenda
//! exactly like an ICS subscription's, because they go through the same
//! parser and the same renderer (see nobo_calendar_mirror, which owns the
//! shape both sides write).
//!
//! The only thing this adds over an ICS feed is the login -- which is the
//! whole point, because a private calendar has no public URL to subscribe
//! to.

use std::path::Path;
use std::sync::Mutex;

use rusqlite::Connection;

use nobo_calendar_mirror::{ics, org, CalendarEvent};

use super::model::CalDavSubscription;
use super::store;
use super::sync::CalDavTransport;

/// How many objects are asked for in one multiget -- a calendar with
/// thousands of entries must not become one enormous request.
const BATCH: usize = 50;

/// Stamped onto the stored ctag, and bumped whenever THIS side changes
/// what it makes of a given server state.
///
/// The ctag answers "has the server changed", which is only half of "is
/// the mirror still right". The other half is whether the mirror was
/// written correctly in the first place -- and when it was not (the
/// calendar sync once asked for the wrong component and wrote an empty
/// file, see client::list_objects), the ctag matched happily ever after
/// and every later sync skipped the fetch. The mirror stayed empty until
/// the user unsubscribed and subscribed again, which is not a repair
/// anybody should have to find.
///
/// A stamp that no longer matches simply misses, so the calendar is
/// fetched once and written again. Bump it when the renderer or the fetch
/// changes.
const MIRROR_FORMAT: &str = "v2";

/// The ctag as it is stored: the server's value, plus what this side made
/// of it.
fn stamped(ctag: &str) -> String {
    format!("{MIRROR_FORMAT} {ctag}")
}

/// What one calendar refresh did.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CalendarSyncOutcome {
    /// How many entries the mirror holds now. Zero when the ctag matched
    /// and nothing was fetched -- whether the FILE changed is a question
    /// for the file, not for this.
    pub event_count: usize,
    /// Entries in this download that the mirror did not have before --
    /// somebody put an appointment in the calendar. Zero when there was no
    /// mirror yet: everything is new on a first sync, and saying so tells
    /// the user nothing (see docs/background-sync.md).
    pub arrived: usize,
}

/// Fetches the collection and rewrites the mirror file.
///
/// Skipped entirely when the collection's ctag is the one already
/// mirrored: a calendar is fetched on every sync run, and rewriting an
/// unchanged file would hand the vault sync a change to carry on every
/// single one of them.
pub async fn sync_calendar(
    transport: &dyn CalDavTransport,
    db: &Mutex<Connection>,
    subscription: &CalDavSubscription,
    file: &Path,
) -> Result<CalendarSyncOutcome, String> {
    let known_ctag = {
        let conn = db.lock().map_err(|_| "database lock poisoned".to_string())?;
        store::get_ctag(&conn, &subscription.id).map_err(|e| e.to_string())?
    };
    let remote_ctag = transport.ctag(&subscription.url).await?;

    // Only trusted when the file is actually there: a mirror the user
    // deleted, or one that never arrived on this device, has to be
    // rebuilt even though the server says nothing changed.
    if let (Some(known), Some(remote)) = (&known_ctag, &remote_ctag) {
        if known == &stamped(remote) && file.exists() {
            return Ok(CalendarSyncOutcome::default());
        }
    }

    let urls: Vec<String> =
        transport.list_objects(&subscription.url, subscription.kind.component()).await?.into_iter().map(|object| object.url).collect();

    let mut events: Vec<CalendarEvent> = Vec::new();
    for batch in urls.chunks(BATCH) {
        for (_url, body) in transport.get_objects(&subscription.url, batch).await? {
            // One unparsable object must not cost the whole calendar --
            // the others are still perfectly good entries.
            match ics::parse_calendar(&body) {
                Ok(parsed) => events.extend(parsed),
                Err(e) => log::warn!("CalDAV calendar '{}': skipping an entry: {e}", subscription.name),
            }
        }
    }

    // Chronological, so the file reads like a calendar and the diff
    // between two refreshes stays small.
    events.sort_by(|a, b| a.start.cmp(&b.start).then_with(|| a.uid.cmp(&b.uid)));

    // Read before the file is replaced: which of these entries is new is
    // not recoverable afterwards. By ICS UID rather than by count -- two
    // removed and two added is not "nothing happened".
    let arrived = match std::fs::read_to_string(file) {
        Ok(previous) => {
            let known = mirrored_uids(&previous);
            // Distinct UIDs: a recurring event whose series has exceptions
            // arrives as several VEVENTs sharing one UID, and the user is
            // told how many new entries there are, not how many records
            // describe them.
            events
                .iter()
                .filter(|event| !known.contains(&event.uid))
                .map(|event| event.uid.as_str())
                .collect::<std::collections::HashSet<_>>()
                .len()
        }
        // No mirror yet: a first sync, where everything is new.
        Err(_) => 0,
    };

    let text = format!("{}{}", super::registry::header(subscription), org::render_events(&events));
    nobo_core::core::vault_write::write_generated(file, &text)?;

    {
        let conn = db.lock().map_err(|_| "database lock poisoned".to_string())?;
        let stamp = remote_ctag.as_deref().map(stamped);
        store::set_ctag(&conn, &subscription.id, stamp.as_deref()).map_err(|e| e.to_string())?;
    }

    Ok(CalendarSyncOutcome { event_count: events.len(), arrived })
}

/// The ICS UIDs a mirror currently holds, straight out of its text.
///
/// Read as text rather than parsed: the org parser would rebuild every
/// heading to answer a question about one property line.
fn mirrored_uids(text: &str) -> std::collections::HashSet<String> {
    let prefix = format!(":{}:", org::ICS_UID_PROPERTY);
    text.lines().filter_map(|line| line.trim().strip_prefix(&prefix)).map(|uid| uid.trim().to_string()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{Precondition, RemoteObject};
    use crate::model::CalDavKind;
    use crate::registry;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const COLLECTION: &str = "https://cloud.example.com/dav/calendars/dennis/familie/";

    fn vevent(uid: &str, summary: &str, start: &str) -> String {
        format!(
            "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:{uid}\r\nSUMMARY:{summary}\r\n\
DTSTART:{start}\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n"
        )
    }

    /// Only the three calls a read-only mirror makes; writing is not part
    /// of this sync at all, and a test transport that could write would
    /// hide it if that ever changed.
    #[derive(Default)]
    struct FakeCalendar {
        objects: Vec<(String, String)>,
        ctag: Option<String>,
        fetches: AtomicUsize,
    }

    #[async_trait]
    impl CalDavTransport for FakeCalendar {
        async fn ctag(&self, _list_url: &str) -> Result<Option<String>, String> {
            Ok(self.ctag.clone())
        }
        async fn list_objects(&self, _list_url: &str, component: &str) -> Result<Vec<RemoteObject>, String> {
            // What the SOGo bug was: a calendar queried for VTODOs answers
            // with nothing, and nothing is not an error anywhere.
            assert_eq!(component, "VEVENT", "a calendar must be listed by its own component");
            self.fetches.fetch_add(1, Ordering::SeqCst);
            Ok(self.objects.iter().map(|(url, _)| RemoteObject { url: url.clone(), etag: "\"e\"".into() }).collect())
        }
        async fn get_object(&self, url: &str) -> Result<String, String> {
            self.objects.iter().find(|(candidate, _)| candidate == url).map(|(_, body)| body.clone()).ok_or_else(|| "404".to_string())
        }
        async fn put_object(&self, _url: &str, _ics: &str, _pre: Precondition<'_>) -> Result<Option<String>, String> {
            unreachable!("a calendar mirror never writes")
        }
        async fn delete_object(&self, _url: &str, _etag: Option<&str>) -> Result<(), String> {
            unreachable!("a calendar mirror never deletes")
        }
        fn object_url(&self, list_url: &str, uid: &str) -> String {
            format!("{list_url}{uid}.ics")
        }
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        file: std::path::PathBuf,
        db: Mutex<Connection>,
        subscription: crate::model::CalDavSubscription,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let subscription = registry::subscription_for(CalDavKind::Events, "Familie", "dennis", COLLECTION);
            let file = dir.path().join(format!("{}.org", subscription.id));
            let conn = Connection::open_in_memory().unwrap();
            store::init_schema(&conn).unwrap();
            Fixture { _dir: dir, file, db: Mutex::new(conn), subscription }
        }

        async fn sync(&self, server: &FakeCalendar) -> CalendarSyncOutcome {
            sync_calendar(server, &self.db, &self.subscription, &self.file).await.unwrap()
        }

        fn content(&self) -> String {
            std::fs::read_to_string(&self.file).unwrap_or_default()
        }
    }

    fn server(objects: &[(&str, String)], ctag: &str) -> FakeCalendar {
        FakeCalendar {
            objects: objects.iter().map(|(url, body)| (url.to_string(), body.clone())).collect(),
            ctag: Some(ctag.to_string()),
            fetches: AtomicUsize::new(0),
        }
    }

    #[tokio::test]
    async fn the_events_land_in_the_file_as_ordinary_calendar_entries() {
        let fixture = Fixture::new();
        let server = server(
            &[("a.ics", vevent("a", "Zahnarzt", "20260821T140000")), ("b.ics", vevent("b", "Elternabend", "20260820T190000"))],
            "ctag-1",
        );

        let outcome = fixture.sync(&server).await;

        assert_eq!(outcome.event_count, 2);
        let content = fixture.content();
        assert!(content.contains("* Zahnarzt"), "{content}");
        // Chronological, not the order the server listed them in.
        assert!(content.find("Elternabend").unwrap() < content.find("Zahnarzt").unwrap(), "{content}");
    }

    /// The mirror has to stay a subscription: the header is what every
    /// device reads it back from, and a rewrite that dropped it would
    /// unsubscribe everyone.
    #[tokio::test]
    async fn the_written_file_reads_back_as_the_same_subscription() {
        let fixture = Fixture::new();
        fixture.sync(&server(&[("a.ics", vevent("a", "Zahnarzt", "20260821T140000"))], "ctag-1")).await;

        let read_back = registry::read(&fixture.file).expect("not a subscription any more");
        assert_eq!(read_back.id, fixture.subscription.id);
        assert_eq!(read_back.url, COLLECTION);
        assert_eq!(read_back.username, "dennis");
        assert_eq!(read_back.kind, CalDavKind::Events);
        // Declared read-only, like every other calendar mirror.
        assert!(!nobo_core::core::file_kind::capabilities(nobo_core::core::file_kind::of_path(&fixture.file)).editable);
    }

    /// A calendar is fetched on every sync run. Rewriting an unchanged
    /// file would hand the vault sync a change to carry every time.
    #[tokio::test]
    async fn an_unchanged_collection_is_not_fetched_again() {
        let fixture = Fixture::new();
        let server = server(&[("a.ics", vevent("a", "Zahnarzt", "20260821T140000"))], "ctag-1");
        fixture.sync(&server).await;
        let before = std::fs::metadata(&fixture.file).unwrap().modified().unwrap();

        let outcome = fixture.sync(&server).await;

        assert_eq!(outcome.event_count, 0, "nothing was fetched");
        assert_eq!(server.fetches.load(Ordering::SeqCst), 1, "the objects were listed a second time");
        assert_eq!(std::fs::metadata(&fixture.file).unwrap().modified().unwrap(), before);
    }

    /// ... unless the file is not there: a mirror that never arrived on
    /// this device, or that the user deleted, has to be rebuilt even
    /// though the server says nothing changed.
    #[tokio::test]
    async fn a_missing_mirror_is_rebuilt_even_when_the_ctag_matches() {
        let fixture = Fixture::new();
        let server = server(&[("a.ics", vevent("a", "Zahnarzt", "20260821T140000"))], "ctag-1");
        fixture.sync(&server).await;
        std::fs::remove_file(&fixture.file).unwrap();

        let outcome = fixture.sync(&server).await;

        assert_eq!(outcome.event_count, 1);
        assert!(fixture.content().contains("Zahnarzt"));
    }

    #[tokio::test]
    async fn a_changed_ctag_refetches() {
        let fixture = Fixture::new();
        fixture.sync(&server(&[("a.ics", vevent("a", "Zahnarzt", "20260821T140000"))], "ctag-1")).await;

        let outcome = fixture.sync(&server(&[("a.ics", vevent("a", "Zahnarzt verschoben", "20260822T140000"))], "ctag-2")).await;

        assert_eq!(outcome.event_count, 1);
        assert!(fixture.content().contains("Zahnarzt verschoben"));
    }

    /// One entry a server spells in a way the parser does not take must
    /// not cost the user the rest of the calendar.
    #[tokio::test]
    async fn one_unreadable_entry_does_not_lose_the_others() {
        let fixture = Fixture::new();
        let server =
            server(&[("a.ics", "not an icalendar at all".to_string()), ("b.ics", vevent("b", "Elternabend", "20260820T190000"))], "ctag-1");

        let outcome = fixture.sync(&server).await;

        assert_eq!(outcome.event_count, 1);
        assert!(fixture.content().contains("Elternabend"));
    }

    /// The repair for the empty mirrors: a file written before this side
    /// was fixed must be fetched again, even though the server says
    /// nothing changed -- otherwise the ctag keeps a broken mirror alive
    /// forever and "sync now" does nothing at all.
    #[tokio::test]
    async fn a_mirror_from_an_older_version_is_fetched_again() {
        let fixture = Fixture::new();
        let server = server(&[("a.ics", vevent("a", "Zahnarzt", "20260821T140000"))], "ctag-1");
        // What the previous version left behind: the bare server ctag,
        // and a file with nothing in it.
        {
            let conn = fixture.db.lock().unwrap();
            store::set_ctag(&conn, &fixture.subscription.id, Some("ctag-1")).unwrap();
        }
        std::fs::create_dir_all(fixture.file.parent().unwrap()).unwrap();
        std::fs::write(&fixture.file, registry::header(&fixture.subscription)).unwrap();

        let outcome = fixture.sync(&server).await;

        assert_eq!(outcome.event_count, 1);
        assert!(fixture.content().contains("Zahnarzt"));
    }

    /// What a notification is allowed to say: entries somebody else put
    /// in the calendar, not "the file was rewritten". By UID, because two
    /// removed and two added is not "nothing happened".
    #[tokio::test]
    async fn only_entries_the_mirror_did_not_have_count_as_arrived() {
        let fixture = Fixture::new();
        let first = fixture.sync(&server(&[("a.ics", vevent("a", "Zahnarzt", "20260821T140000"))], "ctag-1")).await;
        // Nothing to compare against on a first sync -- everything is new
        // there, and saying so tells the user nothing.
        assert_eq!(first.arrived, 0);

        let second = fixture
            .sync(&server(
                &[("a.ics", vevent("a", "Zahnarzt", "20260821T140000")), ("b.ics", vevent("b", "Elternabend", "20260820T190000"))],
                "ctag-2",
            ))
            .await;

        assert_eq!(second.arrived, 1, "only the entry that was not there before");
        assert_eq!(second.event_count, 2);
    }

    /// A recurring appointment with an exception arrives as several
    /// VEVENTs under one UID. The user is told how many new entries there
    /// are, not how many records the server needed for them.
    #[tokio::test]
    async fn a_recurring_entry_counts_once() {
        let fixture = Fixture::new();
        fixture.sync(&server(&[("a.ics", vevent("a", "Zahnarzt", "20260821T140000"))], "ctag-1")).await;

        let series = String::from(
            "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:b\r\nSUMMARY:Chor\r\n\
DTSTART:20260820T190000\r\nRRULE:FREQ=WEEKLY;COUNT=3\r\nEND:VEVENT\r\nBEGIN:VEVENT\r\nUID:b\r\n\
SUMMARY:Chor\r\nDTSTART:20260827T200000\r\nRECURRENCE-ID:20260827T190000\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n",
        );
        let second = fixture.sync(&server(&[("a.ics", vevent("a", "Zahnarzt", "20260821T140000")), ("b.ics", series)], "ctag-2")).await;

        assert_eq!(second.arrived, 1, "one new appointment, however many VEVENTs describe it");
    }

    #[tokio::test]
    async fn an_entry_that_only_changed_has_not_arrived() {
        let fixture = Fixture::new();
        fixture.sync(&server(&[("a.ics", vevent("a", "Zahnarzt", "20260821T140000"))], "ctag-1")).await;

        let second = fixture.sync(&server(&[("a.ics", vevent("a", "Zahnarzt verschoben", "20260822T140000"))], "ctag-2")).await;

        assert_eq!(second.arrived, 0);
    }
}
