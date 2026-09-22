//! The CalDAV wire protocol: finding task lists, and reading and writing
//! the VTODOs in one.
//!
//! Everything below is plain RFC 4791 plus `getctag`, which every server
//! that matters implements. Nothing here knows about org files.

use reqwest::{Client, StatusCode};

use nobo_webdav::MultiStatusEntry;
use super::model::RemoteList;

/// What a write is allowed to overwrite.
///
/// Spelled out rather than an `Option<&str>` where `None` means "must not
/// exist yet": that reads like "no condition" at every call site, and a
/// write meant to be unconditional got the strictest condition there is --
/// every reorder failed with 412 and never reached the server.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Precondition<'a> {
    /// The object must not exist yet (`If-None-Match: *`) -- a task created
    /// here, which must not silently overwrite something already there.
    New,
    /// The object must still be this version (`If-Match`), so a push cannot
    /// clobber an edit another client made since this sync's listing.
    Version(&'a str),
    /// Write regardless. For a change that is worth less than the round
    /// trip to re-read and retry, and that no other client's edit conflicts
    /// with.
    Any,
}

/// How many objects one `calendar-multiget` asks for. Enough that a few
/// hundred tasks take a handful of requests, small enough that neither the
/// request nor the response becomes an outlier a server might refuse.
pub const READ_BATCH: usize = 100;

/// One object read back in full.
#[derive(Debug, Clone, PartialEq)]
pub struct FetchedObject {
    pub url: String,
    pub etag: String,
    pub body: String,
}

/// One task object in a collection, as the server currently holds it.
#[derive(Debug, Clone, PartialEq)]
pub struct RemoteObject {
    /// Absolute URL of the `.ics` -- the address a PUT or DELETE goes to.
    pub url: String,
    pub etag: String,
}

pub struct CalDavClient {
    client: Client,
    base: String,
}

impl CalDavClient {
    pub fn new(base_url: &str, username: &str, password: &str) -> Result<Self, String> {
        use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION};

        let mut headers = HeaderMap::new();
        let mut auth = HeaderValue::from_str(&nobo_webdav::basic_auth_header(username, password))
            .map_err(|e| format!("invalid credentials: {e}"))?;
        auth.set_sensitive(true);
        headers.insert(AUTHORIZATION, auth);

        let builder = nobo_http::builder().default_headers(headers).timeout(nobo_http::ONE_FETCH);

        Ok(Self {
            client: builder.build().map_err(|e| nobo_http::describe_error(&e))?,
            base: base_url.trim_end_matches('/').to_string(),
        })
    }

    /// Every collection under `base` that holds VTODOs.
    ///
    /// Deliberately a plain `Depth: 1` PROPFIND rather than the full
    /// principal/calendar-home-set discovery dance: the user pastes the
    /// address of their calendar home (which is what Nextcloud shows them),
    /// and one request answers the question.
    /// Every calendar collection the account can see, with what each one
    /// holds. Filtering by component is the caller's business: the same
    /// collection can be subscribed to twice, once for its tasks and once
    /// for its events.
    pub async fn list_collections(&self) -> Result<Vec<RemoteList>, String> {
        const BODY: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:prop>
    <d:displayname/>
    <d:resourcetype/>
    <c:supported-calendar-component-set/>
  </d:prop>
</d:propfind>"#;

        // The addresses the collections actually live under -- asked of
        // the server rather than assumed from what was typed, and there
        // can be more than one (see calendar_homes).
        let homes = self.calendar_homes().await;
        if !homes.is_empty() {
            let mut collections: Vec<RemoteList> = Vec::new();
            for home in homes {
                let Ok(entries) = self.propfind(&home, "1", BODY).await else { continue };
                for collection in Self::collections_in(entries, &home) {
                    // One calendar shared by two routes is still one
                    // calendar -- and two rows over one file would be two
                    // subscriptions fighting over it.
                    if !collections.iter().any(|known| same_collection(&known.url, &collection.url)) {
                        collections.push(collection);
                    }
                }
            }
            return Ok(collections);
        }

        let (base, entries) = self.discover(BODY).await?;

        // The address people have at hand is usually the one their client
        // shows for a single list, not the calendar home -- and a Depth: 1
        // listing of a calendar returns that calendar plus its objects,
        // so only ever one list. Recognizing that and asking one level up
        // is the difference between "you have one list" and the truth.
        let target_is_a_calendar = entries
            .iter()
            .find(|e| same_collection(&e.href, &base))
            .is_some_and(|e| e.has_child_element("resourcetype", "calendar"));
        let entries = match parent_url(&base).filter(|_| target_is_a_calendar) {
            Some(parent) => self.propfind(&parent, "1", BODY).await.unwrap_or(entries),
            None => entries,
        };

        Ok(Self::collections_in(entries, &base))
    }

    /// The calendar collections among a listing's entries.
    fn collections_in(entries: Vec<nobo_webdav::MultiStatusEntry>, base: &str) -> Vec<RemoteList> {
        entries
            .into_iter()
            .filter(|e| e.has_child_element("resourcetype", "calendar"))
            .filter(|e| e.supports_component("VTODO") || e.supports_component("VEVENT"))
            .map(|e| RemoteList {
                supports_tasks: e.supports_component("VTODO"),
                supports_events: e.supports_component("VEVENT"),
                display_name: e
                    .prop("displayname")
                    .filter(|n| !n.is_empty())
                    .map(String::from)
                    // A nameless collection is still usable; its last path
                    // segment is what Nextcloud shows in that case too.
                    .unwrap_or_else(|| last_segment(&e.href)),
                url: absolute_to(base, &e.href),
            })
            .collect()
    }

    /// The collection tag of one list. Unchanged means nothing changed
    /// remotely, and the whole per-object round trip can be skipped.
    ///
    /// `None` from a server that does not implement it -- then every sync
    /// has to do the full listing, which is correct, just slower.
    pub async fn ctag(&self, list_url: &str) -> Result<Option<String>, String> {
        const BODY: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:" xmlns:cs="http://calendarserver.org/ns/">
  <d:prop><cs:getctag/></d:prop>
</d:propfind>"#;

        let entries = self.propfind(list_url, "0", BODY).await?;
        Ok(entries.first().and_then(|e| e.prop("getctag")).map(String::from))
    }

    /// Every VTODO object in the list with its current ETag -- one request
    /// for the whole collection, so the sync can decide what to fetch.
    pub async fn list_objects(&self, list_url: &str, component: &str) -> Result<Vec<RemoteObject>, String> {
        // The component has to be the one actually being mirrored. A
        // collection commonly holds both, and a query filtered to VTODO
        // returns nothing at all from a calendar of appointments -- which
        // is not an error anywhere, just an empty list. A lenient server
        // hands back everything regardless and hides the mistake; a strict
        // one (SOGo) does exactly what it was asked and the mirror stays
        // empty.
        let body = format!(
            r#"<?xml version="1.0" encoding="utf-8"?>
<c:calendar-query xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:prop><d:getetag/></d:prop>
  <c:filter>
    <c:comp-filter name="VCALENDAR"><c:comp-filter name="{}"/></c:comp-filter>
  </c:filter>
</c:calendar-query>"#,
            nobo_webdav::escape_text(component)
        );
        let xml = self
            .request(nobo_webdav::method("REPORT"), list_url, Some(("1", &body)))
            .await?;
        Ok(nobo_webdav::parse_multistatus(&xml)?
            .into_iter()
            // The collection answers for itself too, without an ETag.
            .filter(|e| e.prop("getetag").is_some())
            .map(|e| RemoteObject {
                etag: e.prop("getetag").unwrap_or_default().to_string(),
                url: self.absolute(&e.href),
            })
            .collect())
    }

    pub async fn get_object(&self, url: &str) -> Result<String, String> {
        let response = self.client.get(url).send().await.map_err(describe)?;
        if !response.status().is_success() {
            return Err(format!("could not read the task: HTTP {}", response.status()));
        }
        response.text().await.map_err(describe)
    }

    /// Reads many objects in one request.
    ///
    /// `calendar-multiget` (RFC 4791 7.9) exists for exactly this: a list
    /// of hrefs in, their bodies out. Fetching them one at a time costs a
    /// full round trip each -- on a list of a few hundred tasks that is
    /// minutes of waiting against seconds here.
    ///
    /// Batched, because the request names every href and the response
    /// carries every body: one enormous exchange trades one problem for
    /// another, and a server may refuse it outright.
    pub async fn get_objects(&self, list_url: &str, urls: &[String]) -> Result<Vec<FetchedObject>, String> {
        const BATCH: usize = READ_BATCH;

        let mut fetched = Vec::with_capacity(urls.len());
        for batch in urls.chunks(BATCH) {
            let hrefs: String =
                batch.iter().map(|url| format!("  <d:href>{}</d:href>\n", nobo_webdav::escape_text(url))).collect();
            let body = format!(
                r#"<?xml version="1.0" encoding="utf-8"?>
<c:calendar-multiget xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:prop><d:getetag/><c:calendar-data/></d:prop>
{hrefs}</c:calendar-multiget>"#
            );

            let raw = self.request(nobo_webdav::method("REPORT"), list_url, Some(("1", &body))).await?;
            for entry in nobo_webdav::parse_multistatus(&raw)? {
                // A body is the whole point; an entry without one is a
                // per-href error status the caller cannot use.
                let Some(data) = entry.props.get("calendar-data") else { continue };
                fetched.push(FetchedObject {
                    url: self.absolute(&entry.href),
                    etag: entry.props.get("getetag").cloned().unwrap_or_default(),
                    body: data.clone(),
                });
            }
        }
        Ok(fetched)
    }

    /// Writes an object under the given precondition.
    ///
    /// `Ok(None)` when the server accepted the write but returned no ETag,
    /// which is legal -- the next sync then simply re-reads the object.
    pub async fn put_object(
        &self,
        url: &str,
        ics: &str,
        precondition: Precondition<'_>,
    ) -> Result<Option<String>, String> {
        let mut request = self
            .client
            .put(url)
            .header(reqwest::header::CONTENT_TYPE, "text/calendar; charset=utf-8")
            .body(ics.to_string());
        request = match precondition {
            Precondition::New => request.header(reqwest::header::IF_NONE_MATCH, "*"),
            Precondition::Version(etag) => request.header(reqwest::header::IF_MATCH, etag),
            Precondition::Any => request,
        };

        let response = request.send().await.map_err(describe)?;
        if response.status() == StatusCode::PRECONDITION_FAILED {
            return Err(CONFLICT_MARKER.to_string());
        }
        if !response.status().is_success() {
            return Err(format!("could not save the task: HTTP {}", response.status()));
        }
        Ok(response
            .headers()
            .get(reqwest::header::ETAG)
            .and_then(|v| v.to_str().ok())
            .map(String::from))
    }

    /// Conditional as well, for the same reason: a task edited elsewhere
    /// since this sync started must not be deleted on the strength of a
    /// stale listing.
    pub async fn delete_object(&self, url: &str, expected_etag: Option<&str>) -> Result<(), String> {
        let mut request = self.client.delete(url);
        if let Some(etag) = expected_etag {
            request = request.header(reqwest::header::IF_MATCH, etag);
        }
        let response = request.send().await.map_err(describe)?;
        if response.status() == StatusCode::PRECONDITION_FAILED {
            return Err(CONFLICT_MARKER.to_string());
        }
        // Already gone is the outcome we wanted.
        if response.status() == StatusCode::NOT_FOUND || response.status().is_success() {
            return Ok(());
        }
        Err(format!("could not delete the task: HTTP {}", response.status()))
    }

    /// URL for a task created here. The UID as the filename is the
    /// convention every CalDAV client follows, and it keeps the mapping
    /// between object and task recoverable from the URL alone.
    pub fn object_url(&self, list_url: &str, uid: &str) -> String {
        format!("{}/{}.ics", list_url.trim_end_matches('/'), sanitize_segment(uid))
    }

    /// Where this account's calendars live, asked of the server.
    ///
    /// The address a person has at hand is their server
    /// (`https://dav.mailbox.org`), or some page of its web UI -- almost
    /// never the collection directory, which is what a listing needs. Both
    /// SOGo and mailbox.org answer a PROPFIND on the server root, but with
    /// their PRINCIPAL, not with any calendars: "this server offers no
    /// calendars", which is nonsense to read when they are right there.
    ///
    /// RFC 4791 says how to ask properly, and it is two steps: the root
    /// names the logged-in principal (`current-user-principal`), and the
    /// principal names its calendar home (`calendar-home-set`). Both are
    /// tried against the configured address and, if that answers nothing,
    /// against the `/.well-known/caldav` the server advertises (RFC 6764).
    ///
    /// More than one home, because a calendar somebody else shared is not
    /// in yours: the principal also names the principals it may read or
    /// write on behalf of (`calendar-proxy-read-for` /
    /// `-write-for`, the Apple CalendarServer extension both SOGo and
    /// Nextcloud speak), and their homes hold exactly those shared
    /// calendars. Without this a SOGo account shows only its own.
    ///
    /// Empty when the server does not play along -- the caller then falls
    /// back to listing the address as given, which is right for anyone who
    /// pasted their collection directory in the first place.
    async fn calendar_homes(&self) -> Vec<String> {
        const PRINCIPAL_BODY: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:">
  <d:prop><d:current-user-principal/></d:prop>
</d:propfind>"#;
        const HOME_BODY: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav" xmlns:cs="http://calendarserver.org/ns/">
  <d:prop>
    <c:calendar-home-set/>
    <cs:calendar-proxy-read-for/>
    <cs:calendar-proxy-write-for/>
  </d:prop>
</d:propfind>"#;

        for root in [self.base.clone(), format!("{}/.well-known/caldav", origin_of(&self.base))] {
            let Ok(entries) = self.propfind(&root, "0", PRINCIPAL_BODY).await else { continue };
            let Some(principal) = entries.iter().find_map(|e| e.prop_href("current-user-principal")) else {
                continue;
            };

            // One level of delegation only: a principal you may read for
            // does not lend you ITS delegations, and following those would
            // be a walk over the server's whole address book -- one
            // PROPFIND per principal, on every listing.
            let mut homes = Vec::new();
            let mut seen = std::collections::HashSet::new();
            let mut delegated = Vec::new();

            let own = self.absolute(principal);
            seen.insert(own.clone());
            if let Ok(entries) = self.propfind(&own, "0", HOME_BODY).await {
                collect_homes(&entries, &mut homes, |href| self.absolute(href));
                for entry in &entries {
                    for property in ["calendar-proxy-read-for", "calendar-proxy-write-for"] {
                        for href in entry.prop_hrefs.get(property).into_iter().flatten() {
                            delegated.push(self.absolute(href));
                        }
                    }
                }
            }

            for principal in delegated {
                if !seen.insert(principal.clone()) {
                    continue;
                }
                let Ok(entries) = self.propfind(&principal, "0", HOME_BODY).await else { continue };
                // Their delegations are deliberately not read -- see above.
                collect_homes(&entries, &mut homes, |href| self.absolute(href));
            }

            if !homes.is_empty() {
                return homes;
            }
        }
        Vec::new()
    }

    /// Lists the configured address, falling back to the server's own
    /// advertised DAV root.
    ///
    /// A bare server address (`https://cloud.example.com`) is what people
    /// have at hand, but PROPFIND against it answers 405: the DAV endpoint
    /// sits somewhere below, and where exactly is the server's business
    /// (`/remote.php/dav/` on Nextcloud, something else elsewhere). RFC
    /// 6764 is how a server says where: `/.well-known/caldav` redirects
    /// to it. Asked only when the address as given did not work, so a
    /// correct one costs nothing.
    ///
    /// Returns the address the answer actually came from -- the caller
    /// compares hrefs against it.
    async fn discover(&self, body: &str) -> Result<(String, Vec<MultiStatusEntry>), String> {
        let direct = self.propfind(&self.base, "1", body).await;
        if direct.is_ok() {
            return direct.map(|entries| (self.base.clone(), entries));
        }

        let well_known = format!("{}/.well-known/caldav", origin_of(&self.base));
        match self.propfind(&well_known, "1", body).await {
            Ok(entries) => Ok((well_known, entries)),
            // The original failure is the one worth reporting: the user
            // gave that address, and "no /.well-known/caldav" would send
            // them looking in the wrong place.
            Err(_) => direct.map(|entries| (self.base.clone(), entries)),
        }
    }

    async fn propfind(&self, url: &str, depth: &str, body: &str) -> Result<Vec<MultiStatusEntry>, String> {
        let xml = self.request(nobo_webdav::method("PROPFIND"), url, Some((depth, body))).await?;
        nobo_webdav::parse_multistatus(&xml)
    }

    async fn request(
        &self,
        method: reqwest::Method,
        url: &str,
        body: Option<(&str, &str)>,
    ) -> Result<String, String> {
        let mut request = self.client.request(method, url);
        if let Some((depth, xml)) = body {
            request = request
                .header("Depth", depth)
                .header(reqwest::header::CONTENT_TYPE, "application/xml; charset=utf-8")
                .body(xml.to_string());
        }
        let response = request.send().await.map_err(describe)?;
        let status = response.status();
        if !status.is_success() && status != StatusCode::MULTI_STATUS {
            return Err(match status {
                StatusCode::UNAUTHORIZED => "login rejected -- check the user name and password".to_string(),
                StatusCode::NOT_FOUND => "no such calendar at that address".to_string(),
                other => format!("the server answered HTTP {other}"),
            });
        }
        response.text().await.map_err(describe)
    }

    /// A multistatus href may be a path (`/dav/cal/x.ics`) or a full URL,
    /// depending on the server.
    fn absolute(&self, href: &str) -> String {
        if href.starts_with("http://") || href.starts_with("https://") {
            return href.to_string();
        }
        let origin = origin_of(&self.base);
        format!("{origin}{href}")
    }
}

/// Sentinel for "the server's version is not the one this device expected".
/// Kept as one constant so the sync can recognize the case without
/// matching on message text.
pub const CONFLICT_MARKER: &str = "caldav-precondition-failed";

fn describe(error: reqwest::Error) -> String {
    nobo_http::request_failed(&error)
}

/// Whether an href and a URL name the same collection, comparing only the
/// path and ignoring a trailing slash -- servers are inconsistent about
/// both the slash and whether the href is absolute.
fn same_collection(href: &str, url: &str) -> bool {
    let path_of = |value: &str| {
        let without_scheme = value.split_once("://").map_or(value, |(_, rest)| {
            rest.find('/').map_or("", |index| &rest[index..])
        });
        without_scheme.trim_end_matches('/').to_string()
    };
    !href.is_empty() && path_of(href) == path_of(url)
}

/// One level up, or `None` at the root.
/// The collection one level up -- a calendar's home, given the calendar.
/// `None` at the host, which has no parent collection.
pub fn parent_url(url: &str) -> Option<String> {
    let trimmed = url.trim_end_matches('/');
    let cut = trimmed.rfind('/')?;
    // Never past the host: "https://x" has no parent collection.
    if trimmed[..cut].ends_with(':') || !trimmed[..cut].contains("//") {
        return None;
    }
    Some(trimmed[..cut].to_string())
}

fn last_segment(href: &str) -> String {
    href.trim_end_matches('/').rsplit('/').next().unwrap_or(href).to_string()
}

/// Adds every `calendar-home-set` in ENTRIES to HOMES, without
/// duplicates.
fn collect_homes(
    entries: &[MultiStatusEntry],
    homes: &mut Vec<String>,
    absolute: impl Fn(&str) -> String,
) {
    for entry in entries {
        if let Some(home) = entry.prop_href("calendar-home-set") {
            let home = absolute(home);
            if !homes.contains(&home) {
                homes.push(home);
            }
        }
    }
}

/// A href made absolute against a URL from the same server -- the free
/// function behind CalDavClient::absolute, for the places that have a
/// reference URL but no client (see collections_in).
fn absolute_to(reference: &str, href: &str) -> String {
    if href.starts_with("http://") || href.starts_with("https://") {
        return href.to_string();
    }
    format!("{}{href}", origin_of(reference))
}

/// Scheme and host of a URL, without any path.
fn origin_of(url: &str) -> String {
    let (scheme, rest) = match url.split_once("://") {
        Some((scheme, rest)) => (scheme, rest),
        None => return String::new(),
    };
    let host = rest.split('/').next().unwrap_or(rest);
    format!("{scheme}://{host}")
}

/// A UID is server-chosen and may contain anything; a path segment may
/// not. Everything outside a conservative set becomes `-`.
fn sanitize_segment(uid: &str) -> String {
    uid.chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '-' })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_string_contains, header, method as http_method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn client(server: &MockServer) -> CalDavClient {
        CalDavClient::new(&format!("{}/dav/calendars/dennis", server.uri()), "dennis", "hunter2").unwrap()
    }

    const HOME_RESPONSE: &str = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:cs="http://calendarserver.org/ns/" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:response>
    <d:href>/dav/calendars/dennis/</d:href>
    <d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop></d:propstat>
  </d:response>
  <d:response>
    <d:href>/dav/calendars/dennis/einkauf/</d:href>
    <d:propstat><d:prop>
      <d:displayname>Einkauf</d:displayname>
      <d:resourcetype><d:collection/><c:calendar/></d:resourcetype>
      <c:supported-calendar-component-set><c:comp name="VTODO"/></c:supported-calendar-component-set>
    </d:prop></d:propstat>
  </d:response>
  <d:response>
    <d:href>/dav/calendars/dennis/termine/</d:href>
    <d:propstat><d:prop>
      <d:displayname>Termine</d:displayname>
      <d:resourcetype><d:collection/><c:calendar/></d:resourcetype>
      <c:supported-calendar-component-set><c:comp name="VEVENT"/></c:supported-calendar-component-set>
    </d:prop></d:propstat>
  </d:response>
</d:multistatus>"#;

    /// A calendar of appointments has no business appearing as a task list
    /// to subscribe to.
    #[tokio::test]
    async fn only_collections_that_hold_vtodos_are_offered() {
        let server = MockServer::start().await;
        Mock::given(http_method("PROPFIND"))
            .and(path("/dav/calendars/dennis"))
            .and(header("Depth", "1"))
            .respond_with(ResponseTemplate::new(207).set_body_string(HOME_RESPONSE))
            .mount(&server)
            .await;

        let lists: Vec<_> = client(&server).list_collections().await.unwrap().into_iter().filter(|l| l.supports_tasks).collect();

        assert_eq!(lists.len(), 1);
        assert_eq!(lists[0].display_name, "Einkauf");
        assert_eq!(lists[0].url, format!("{}/dav/calendars/dennis/einkauf/", server.uri()));
    }

    /// The address a client shows for one list is the one people paste,
    /// and a Depth: 1 listing of a calendar answers with that calendar and
    /// nothing else -- so the other lists silently do not exist. Noticing
    /// that the target is itself a calendar and asking its parent is what
    /// turns "you have one list" into all of them.
    #[tokio::test]
    async fn pointing_at_a_single_list_still_finds_its_siblings() {
        const ONE_CALENDAR: &str = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:response>
    <d:href>/dav/calendars/dennis/einkauf/</d:href>
    <d:propstat><d:prop>
      <d:displayname>Einkauf</d:displayname>
      <d:resourcetype><d:collection/><c:calendar/></d:resourcetype>
      <c:supported-calendar-component-set><c:comp name="VTODO"/></c:supported-calendar-component-set>
    </d:prop></d:propstat>
  </d:response>
</d:multistatus>"#;
        let server = MockServer::start().await;
        Mock::given(http_method("PROPFIND"))
            .and(path("/dav/calendars/dennis/einkauf"))
            .respond_with(ResponseTemplate::new(207).set_body_string(ONE_CALENDAR))
            .mount(&server)
            .await;
        // The parent knows about a second list the calendar itself could
        // never have mentioned.
        const SIBLINGS: &str = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:response>
    <d:href>/dav/calendars/dennis/einkauf/</d:href>
    <d:propstat><d:prop>
      <d:displayname>Einkauf</d:displayname>
      <d:resourcetype><d:collection/><c:calendar/></d:resourcetype>
      <c:supported-calendar-component-set><c:comp name="VTODO"/></c:supported-calendar-component-set>
    </d:prop></d:propstat>
  </d:response>
  <d:response>
    <d:href>/dav/calendars/dennis/vorrat/</d:href>
    <d:propstat><d:prop>
      <d:displayname>Vorrat</d:displayname>
      <d:resourcetype><d:collection/><c:calendar/></d:resourcetype>
      <c:supported-calendar-component-set><c:comp name="VTODO"/></c:supported-calendar-component-set>
    </d:prop></d:propstat>
  </d:response>
</d:multistatus>"#;
        Mock::given(http_method("PROPFIND"))
            .and(path("/dav/calendars/dennis"))
            .respond_with(ResponseTemplate::new(207).set_body_string(SIBLINGS))
            .mount(&server)
            .await;

        let client =
            CalDavClient::new(&format!("{}/dav/calendars/dennis/einkauf", server.uri()), "dennis", "hunter2").unwrap();
        let lists: Vec<_> = client.list_collections().await.unwrap().into_iter().filter(|l| l.supports_tasks).collect();

        assert_eq!(lists.iter().map(|l| l.display_name.as_str()).collect::<Vec<_>>(), ["Einkauf", "Vorrat"]);
    }

    /// ...and pointing at the home is not climbed out of: it is not a
    /// calendar, so there is nothing above it worth asking about.
    #[tokio::test]
    async fn a_calendar_home_is_not_climbed_out_of() {
        let server = MockServer::start().await;
        Mock::given(http_method("PROPFIND"))
            .and(path("/dav/calendars/dennis"))
            .respond_with(ResponseTemplate::new(207).set_body_string(HOME_RESPONSE))
            .mount(&server)
            .await;
        Mock::given(http_method("PROPFIND"))
            .and(path("/dav/calendars"))
            .respond_with(ResponseTemplate::new(207).set_body_string(HOME_RESPONSE))
            .expect(0)
            .mount(&server)
            .await;

        let lists: Vec<_> = client(&server).list_collections().await.unwrap().into_iter().filter(|l| l.supports_tasks).collect();

        assert_eq!(lists.len(), 1);
    }

    /// One request, every body -- and the hrefs it asked for actually in
    /// it, since a multiget that names nothing returns nothing.
    #[tokio::test]
    async fn many_objects_come_back_from_a_single_report() {
        const MULTIGET: &str = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:response>
    <d:href>/dav/calendars/dennis/einkauf/a.ics</d:href>
    <d:propstat><d:prop>
      <d:getetag>"v1"</d:getetag>
      <c:calendar-data>BEGIN:VCALENDAR
BEGIN:VTODO
UID:a
END:VTODO
END:VCALENDAR
</c:calendar-data>
    </d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
  </d:response>
  <d:response>
    <d:href>/dav/calendars/dennis/einkauf/b.ics</d:href>
    <d:propstat><d:prop>
      <d:getetag>"v2"</d:getetag>
      <c:calendar-data>BEGIN:VCALENDAR
BEGIN:VTODO
UID:b
END:VTODO
END:VCALENDAR
</c:calendar-data>
    </d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
  </d:response>
</d:multistatus>"#;
        let server = MockServer::start().await;
        Mock::given(http_method("REPORT"))
            .and(path("/dav/calendars/dennis/einkauf"))
            .and(body_string_contains("calendar-multiget"))
            .and(body_string_contains("/dav/calendars/dennis/einkauf/a.ics"))
            .and(body_string_contains("/dav/calendars/dennis/einkauf/b.ics"))
            .respond_with(ResponseTemplate::new(207).set_body_string(MULTIGET))
            .expect(1)
            .mount(&server)
            .await;

        let list = format!("{}/dav/calendars/dennis/einkauf", server.uri());
        let urls = vec![format!("{list}/a.ics"), format!("{list}/b.ics")];
        let fetched = client(&server).get_objects(&list, &urls).await.unwrap();

        assert_eq!(fetched.len(), 2);
        assert_eq!(fetched[0].url, urls[0]);
        assert_eq!(fetched[0].etag, "\"v1\"");
        assert!(fetched[0].body.contains("UID:a"), "{:?}", fetched[0].body);
        assert!(fetched[1].body.contains("UID:b"), "{:?}", fetched[1].body);
    }

    /// What Nextcloud actually answers: two propstat blocks per response
    /// (200 for what it found, 404 for what it did not), several task
    /// lists, and collections that are not calendars at all.
    const NEXTCLOUD_HOME: &str = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:cs="http://calendarserver.org/ns/" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:response>
    <d:href>/remote.php/dav/calendars/dennis/</d:href>
    <d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
    <d:propstat><d:prop><d:displayname/><c:supported-calendar-component-set/></d:prop><d:status>HTTP/1.1 404 Not Found</d:status></d:propstat>
  </d:response>
  <d:response>
    <d:href>/remote.php/dav/calendars/dennis/einkauf/</d:href>
    <d:propstat><d:prop>
      <d:displayname>Einkauf</d:displayname>
      <d:resourcetype><d:collection/><c:calendar/></d:resourcetype>
      <c:supported-calendar-component-set><c:comp name="VTODO"/></c:supported-calendar-component-set>
    </d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
  </d:response>
  <d:response>
    <d:href>/remote.php/dav/calendars/dennis/persoenlich/</d:href>
    <d:propstat><d:prop>
      <d:displayname>Persönlich</d:displayname>
      <d:resourcetype><d:collection/><c:calendar/></d:resourcetype>
      <c:supported-calendar-component-set><c:comp name="VEVENT"/><c:comp name="VTODO"/></c:supported-calendar-component-set>
    </d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
  </d:response>
  <d:response>
    <d:href>/remote.php/dav/calendars/dennis/arbeit/</d:href>
    <d:propstat><d:prop>
      <d:displayname>Arbeit</d:displayname>
      <d:resourcetype><d:collection/><c:calendar/></d:resourcetype>
      <c:supported-calendar-component-set><c:comp name="VTODO"/><c:comp name="VJOURNAL"/></c:supported-calendar-component-set>
    </d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
  </d:response>
  <d:response>
    <d:href>/remote.php/dav/calendars/dennis/geteilt/</d:href>
    <d:propstat><d:prop>
      <d:displayname>Geteilte Liste</d:displayname>
      <d:resourcetype><d:collection/><c:calendar/></d:resourcetype>
    </d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
  </d:response>
  <d:response>
    <d:href>/remote.php/dav/calendars/dennis/inbox/</d:href>
    <d:propstat><d:prop>
      <d:resourcetype><d:collection/><c:schedule-inbox/></d:resourcetype>
    </d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
  </d:response>
</d:multistatus>"#;

    /// The reported bug: three task lists on the server, one shown.
    #[tokio::test]
    async fn every_list_that_holds_tasks_is_offered_not_just_the_first() {
        let server = MockServer::start().await;
        Mock::given(http_method("PROPFIND"))
            .and(path("/dav/calendars/dennis"))
            .respond_with(ResponseTemplate::new(207).set_body_string(NEXTCLOUD_HOME))
            .mount(&server)
            .await;

        let lists: Vec<_> = client(&server).list_collections().await.unwrap().into_iter().filter(|l| l.supports_tasks).collect();

        assert_eq!(
            lists.iter().map(|l| l.display_name.as_str()).collect::<Vec<_>>(),
            // "Geteilte Liste" says nothing about what it holds, which
            // RFC 4791 means as "everything" -- dropping it is exactly how
            // lists go missing.
            ["Einkauf", "Persönlich", "Arbeit", "Geteilte Liste"],
        );
    }

    #[tokio::test]
    async fn requests_carry_basic_auth() {
        let server = MockServer::start().await;
        Mock::given(http_method("PROPFIND"))
            .and(header("Authorization", nobo_webdav::basic_auth_header("dennis", "hunter2").as_str()))
            .respond_with(ResponseTemplate::new(207).set_body_string(HOME_RESPONSE))
            .mount(&server)
            .await;

        // Matches no mock, and wiremock answers 404, if the header is
        // missing or wrong.
        client(&server).list_collections().await.unwrap();
    }

    #[tokio::test]
    async fn the_ctag_is_read_from_the_collection_itself() {
        let server = MockServer::start().await;
        Mock::given(http_method("PROPFIND"))
            .and(path("/dav/calendars/dennis/einkauf/"))
            .and(header("Depth", "0"))
            .respond_with(ResponseTemplate::new(207).set_body_string(
                r#"<d:multistatus xmlns:d="DAV:" xmlns:cs="http://calendarserver.org/ns/"><d:response>
                   <d:href>/dav/calendars/dennis/einkauf/</d:href>
                   <d:propstat><d:prop><cs:getctag>sync-42</cs:getctag></d:prop></d:propstat>
                   </d:response></d:multistatus>"#,
            ))
            .mount(&server)
            .await;

        let ctag = client(&server).ctag(&format!("{}/dav/calendars/dennis/einkauf/", server.uri())).await.unwrap();

        assert_eq!(ctag.as_deref(), Some("sync-42"));
    }

    /// Not every server implements it. That has to mean "do the full
    /// listing", never "nothing changed".
    #[tokio::test]
    async fn a_server_without_a_ctag_reports_none_rather_than_failing() {
        let server = MockServer::start().await;
        Mock::given(http_method("PROPFIND"))
            .respond_with(ResponseTemplate::new(207).set_body_string(
                r#"<d:multistatus xmlns:d="DAV:"><d:response><d:href>/dav/calendars/dennis/einkauf/</d:href>
                   <d:propstat><d:prop/></d:propstat></d:response></d:multistatus>"#,
            ))
            .mount(&server)
            .await;

        let ctag = client(&server).ctag(&format!("{}/dav/calendars/dennis/einkauf/", server.uri())).await.unwrap();

        assert_eq!(ctag, None);
    }

    #[tokio::test]
    async fn listing_objects_returns_each_task_with_its_etag() {
        let server = MockServer::start().await;
        Mock::given(http_method("REPORT"))
            .and(path("/dav/calendars/dennis/einkauf/"))
            .and(body_string_contains("VTODO"))
            .respond_with(ResponseTemplate::new(207).set_body_string(
                r#"<d:multistatus xmlns:d="DAV:"><d:response>
                     <d:href>/dav/calendars/dennis/einkauf/</d:href>
                     <d:propstat><d:prop/></d:propstat>
                   </d:response><d:response>
                     <d:href>/dav/calendars/dennis/einkauf/abc.ics</d:href>
                     <d:propstat><d:prop><d:getetag>"v1"</d:getetag></d:prop></d:propstat>
                   </d:response></d:multistatus>"#,
            ))
            .mount(&server)
            .await;

        let objects = client(&server).list_objects(&format!("{}/dav/calendars/dennis/einkauf/", server.uri()), "VTODO").await.unwrap();

        // The collection answers for itself too, without an ETag -- taking
        // it for a task would create a phantom entry in every list.
        assert_eq!(objects.len(), 1);
        assert_eq!(objects[0].etag, "\"v1\"");
        assert!(objects[0].url.ends_with("/einkauf/abc.ics"));
    }

    /* --- writing --- */

    /// Without If-Match a push silently overwrites an edit another client
    /// made between this sync's listing and its write.
    #[tokio::test]
    async fn updating_a_task_is_conditional_on_the_version_it_was_read_at() {
        let server = MockServer::start().await;
        Mock::given(http_method("PUT"))
            .and(path("/dav/calendars/dennis/einkauf/abc.ics"))
            .and(header("If-Match", "\"v1\""))
            .respond_with(ResponseTemplate::new(204).insert_header("ETag", "\"v2\""))
            .mount(&server)
            .await;

        let etag = client(&server)
            .put_object(&format!("{}/dav/calendars/dennis/einkauf/abc.ics", server.uri()), "BEGIN:VCALENDAR\r\n", Precondition::Version("\"v1\""))
            .await
            .unwrap();

        assert_eq!(etag.as_deref(), Some("\"v2\""));
    }

    /// A task created here must not overwrite one that already exists at
    /// the same address.
    #[tokio::test]
    async fn creating_a_task_refuses_to_overwrite_an_existing_one() {
        let server = MockServer::start().await;
        Mock::given(http_method("PUT"))
            .and(header("If-None-Match", "*"))
            .respond_with(ResponseTemplate::new(201))
            .mount(&server)
            .await;

        let result = client(&server)
            .put_object(&format!("{}/dav/calendars/dennis/einkauf/new.ics", server.uri()), "BEGIN:VCALENDAR\r\n", Precondition::New)
            .await;

        assert!(result.is_ok(), "{result:?}");
    }

    /// The one failure the sync has to recognize by kind rather than by
    /// message: somebody else got there first.
    #[tokio::test]
    async fn a_rejected_precondition_is_reported_as_a_conflict() {
        let server = MockServer::start().await;
        Mock::given(http_method("PUT"))
            .respond_with(ResponseTemplate::new(412))
            .mount(&server)
            .await;

        let error = client(&server)
            .put_object(&format!("{}/dav/calendars/dennis/einkauf/abc.ics", server.uri()), "x", Precondition::Version("\"v1\""))
            .await
            .unwrap_err();

        assert_eq!(error, CONFLICT_MARKER);
    }

    #[tokio::test]
    async fn deleting_something_already_gone_counts_as_success() {
        let server = MockServer::start().await;
        Mock::given(http_method("DELETE"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        client(&server)
            .delete_object(&format!("{}/dav/calendars/dennis/einkauf/abc.ics", server.uri()), Some("\"v1\""))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_rejected_login_says_so_instead_of_reporting_a_status_code() {
        let server = MockServer::start().await;
        Mock::given(http_method("PROPFIND"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;

        let error = client(&server).list_collections().await.unwrap_err();

        assert!(error.contains("password"), "{error}");
    }

    /* --- URL handling --- */

    #[test]
    fn an_href_may_be_a_path_or_a_full_url() {
        let c = CalDavClient::new("https://cloud.example.com/dav/cal", "u", "p").unwrap();

        assert_eq!(c.absolute("/dav/cal/x.ics"), "https://cloud.example.com/dav/cal/x.ics");
        assert_eq!(c.absolute("https://other.example.com/y.ics"), "https://other.example.com/y.ics");
    }

    /// A UID is server-chosen and may contain anything; a path segment may
    /// not.
    #[test]
    fn a_uid_with_path_characters_cannot_escape_its_collection() {
        let c = CalDavClient::new("https://cloud.example.com/dav/cal", "u", "p").unwrap();

        let url = c.object_url("https://cloud.example.com/dav/cal/einkauf/", "../../etc/passwd");

        // The property that matters is that it stays ONE path segment
        // inside the collection -- the separators are what would let it
        // climb out, not the dots.
        let segment = url.strip_prefix("https://cloud.example.com/dav/cal/einkauf/").unwrap();
        assert!(!segment.contains('/'), "{url}");
        assert!(!segment.contains("../"), "{url}");
    }

    #[test]
    fn a_trailing_slash_on_the_list_url_does_not_double_up() {
        let c = CalDavClient::new("https://cloud.example.com/dav/cal", "u", "p").unwrap();

        assert_eq!(
            c.object_url("https://cloud.example.com/dav/cal/einkauf/", "abc"),
            c.object_url("https://cloud.example.com/dav/cal/einkauf", "abc")
        );
    }

    /// The address people have at hand is the server, not its DAV root --
    /// and a PROPFIND against the bare server answers 405. RFC 6764 is how
    /// the server itself says where to look.
    #[tokio::test]
    async fn a_bare_server_address_is_resolved_through_well_known() {
        let server = MockServer::start().await;
        Mock::given(http_method("PROPFIND"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(405))
            .mount(&server)
            .await;
        Mock::given(http_method("PROPFIND"))
            .and(path("/.well-known/caldav"))
            .respond_with(ResponseTemplate::new(207).set_body_string(HOME_RESPONSE))
            .mount(&server)
            .await;

        let client = CalDavClient::new(&server.uri(), "dennis", "hunter2").unwrap();
        let collections = client.list_collections().await.unwrap();

        assert_eq!(collections.len(), 2, "{collections:?}");
    }

    /// ... but the address the user gave is what a failure has to be
    /// reported about: "no /.well-known/caldav" would send them looking in
    /// the wrong place.
    #[tokio::test]
    async fn a_wrong_address_still_reports_its_own_failure() {
        let server = MockServer::start().await;
        Mock::given(http_method("PROPFIND"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let error = CalDavClient::new(&format!("{}/nope", server.uri()), "dennis", "hunter2")
            .unwrap()
            .list_collections()
            .await
            .unwrap_err();

        assert!(error.contains("no such calendar"), "{error}");
    }

    /// What mailbox.org and SOGo actually do: a PROPFIND on the address a
    /// person has (their server) answers with their PRINCIPAL, not with
    /// any calendars. Reading that as "this server offers no calendars" is
    /// what the user saw -- the calendars are two questions further on.
    #[tokio::test]
    async fn a_server_that_answers_with_its_principal_is_followed_to_the_calendars() {
        const PRINCIPAL: &str = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:">
  <d:response>
    <d:href>/</d:href>
    <d:propstat><d:prop>
      <d:current-user-principal><d:href>/principals/dennis/</d:href></d:current-user-principal>
    </d:prop></d:propstat>
  </d:response>
</d:multistatus>"#;
        const HOME_SET: &str = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:response>
    <d:href>/principals/dennis/</d:href>
    <d:propstat><d:prop>
      <c:calendar-home-set><d:href>/dav/calendars/dennis/</d:href></c:calendar-home-set>
    </d:prop></d:propstat>
  </d:response>
</d:multistatus>"#;

        let server = MockServer::start().await;
        Mock::given(http_method("PROPFIND"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(207).set_body_string(PRINCIPAL))
            .mount(&server)
            .await;
        Mock::given(http_method("PROPFIND"))
            .and(path("/principals/dennis/"))
            .respond_with(ResponseTemplate::new(207).set_body_string(HOME_SET))
            .mount(&server)
            .await;
        Mock::given(http_method("PROPFIND"))
            .and(path("/dav/calendars/dennis/"))
            .respond_with(ResponseTemplate::new(207).set_body_string(HOME_RESPONSE))
            .mount(&server)
            .await;

        let collections = CalDavClient::new(&server.uri(), "dennis", "hunter2")
            .unwrap()
            .list_collections()
            .await
            .unwrap();

        assert_eq!(collections.len(), 2, "{collections:?}");
        assert!(collections.iter().any(|c| c.supports_tasks));
        assert!(collections.iter().any(|c| c.supports_events));
    }

    /// A calendar somebody else shared is not in your own home -- the
    /// principal points at the principals you may read for, and their
    /// homes hold it. Without following those, a SOGo account shows only
    /// its own calendars, which is what the user saw.
    #[tokio::test]
    async fn calendars_shared_by_others_are_found_too() {
        const PRINCIPAL: &str = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:">
  <d:response>
    <d:href>/SOGo/dav/</d:href>
    <d:propstat><d:prop>
      <d:current-user-principal><d:href>/SOGo/dav/dennis/</d:href></d:current-user-principal>
    </d:prop></d:propstat>
  </d:response>
</d:multistatus>"#;
        const MINE: &str = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav" xmlns:cs="http://calendarserver.org/ns/">
  <d:response>
    <d:href>/SOGo/dav/dennis/</d:href>
    <d:propstat><d:prop>
      <c:calendar-home-set><d:href>/SOGo/dav/dennis/Calendar/</d:href></c:calendar-home-set>
      <cs:calendar-proxy-read-for><d:href>/SOGo/dav/team/</d:href></cs:calendar-proxy-read-for>
    </d:prop></d:propstat>
  </d:response>
</d:multistatus>"#;
        const THEIRS: &str = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:response>
    <d:href>/SOGo/dav/team/</d:href>
    <d:propstat><d:prop>
      <c:calendar-home-set><d:href>/SOGo/dav/team/Calendar/</d:href></c:calendar-home-set>
    </d:prop></d:propstat>
  </d:response>
</d:multistatus>"#;
        const OWN_CALENDARS: &str = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:response>
    <d:href>/SOGo/dav/dennis/Calendar/personal/</d:href>
    <d:propstat><d:prop>
      <d:displayname>Privat</d:displayname>
      <d:resourcetype><d:collection/><c:calendar/></d:resourcetype>
      <c:supported-calendar-component-set><c:comp name="VEVENT"/></c:supported-calendar-component-set>
    </d:prop></d:propstat>
  </d:response>
</d:multistatus>"#;
        const SHARED_CALENDARS: &str = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:response>
    <d:href>/SOGo/dav/team/Calendar/plan/</d:href>
    <d:propstat><d:prop>
      <d:displayname>Team-Planung</d:displayname>
      <d:resourcetype><d:collection/><c:calendar/></d:resourcetype>
      <c:supported-calendar-component-set><c:comp name="VEVENT"/></c:supported-calendar-component-set>
    </d:prop></d:propstat>
  </d:response>
</d:multistatus>"#;

        let server = MockServer::start().await;
        for (at, body) in [
            // Without the trailing slash: the client trims it off the
            // configured address (see CalDavClient::new).
            ("/SOGo/dav", PRINCIPAL),
            ("/SOGo/dav/dennis/", MINE),
            ("/SOGo/dav/team/", THEIRS),
            ("/SOGo/dav/dennis/Calendar/", OWN_CALENDARS),
            ("/SOGo/dav/team/Calendar/", SHARED_CALENDARS),
        ] {
            Mock::given(http_method("PROPFIND"))
                .and(path(at))
                .respond_with(ResponseTemplate::new(207).set_body_string(body))
                .mount(&server)
                .await;
        }

        let collections = CalDavClient::new(&format!("{}/SOGo/dav/", server.uri()), "dennis", "hunter2")
            .unwrap()
            .list_collections()
            .await
            .unwrap();

        let names: Vec<&str> = collections.iter().map(|c| c.display_name.as_str()).collect();
        assert!(names.contains(&"Privat"), "{names:?}");
        assert!(names.contains(&"Team-Planung"), "{names:?}");
    }

    /// The query has to name the component actually being mirrored: a
    /// strict server (SOGo) answers a VTODO query on a calendar of
    /// appointments with nothing at all, and an empty answer is not an
    /// error anywhere -- the mirror simply stayed empty.
    #[tokio::test]
    async fn objects_are_listed_by_the_component_that_was_asked_for() {
        let server = MockServer::start().await;
        Mock::given(http_method("REPORT"))
            .and(path("/dav/calendars/dennis/familie/"))
            .and(body_string_contains("<c:comp-filter name=\"VEVENT\"/>"))
            .respond_with(ResponseTemplate::new(207).set_body_string(
                r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:">
  <d:response>
    <d:href>/dav/calendars/dennis/familie/a.ics</d:href>
    <d:propstat><d:prop><d:getetag>"v1"</d:getetag></d:prop></d:propstat>
  </d:response>
</d:multistatus>"#,
            ))
            .mount(&server)
            .await;

        let objects = client(&server)
            .list_objects(&format!("{}/dav/calendars/dennis/familie/", server.uri()), "VEVENT")
            .await
            .unwrap();

        assert_eq!(objects.len(), 1);
    }

    /// One level, and only one: a principal you may read for does not lend
    /// you ITS delegations. Following those would be a PROPFIND per
    /// principal on the server, on every listing.
    #[tokio::test]
    async fn delegation_is_not_followed_transitively() {
        const PRINCIPAL: &str = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:">
  <d:response><d:href>/dav/</d:href><d:propstat><d:prop>
    <d:current-user-principal><d:href>/dav/me/</d:href></d:current-user-principal>
  </d:prop></d:propstat></d:response>
</d:multistatus>"#;
        fn home_with_delegate(home: &str, delegate: &str) -> String {
            format!(
                r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav" xmlns:cs="http://calendarserver.org/ns/">
  <d:response><d:href>/dav/x/</d:href><d:propstat><d:prop>
    <c:calendar-home-set><d:href>{home}</d:href></c:calendar-home-set>
    <cs:calendar-proxy-read-for><d:href>{delegate}</d:href></cs:calendar-proxy-read-for>
  </d:prop></d:propstat></d:response>
</d:multistatus>"#
            )
        }
        const CALENDARS: &str = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:response><d:href>/dav/me/cal/privat/</d:href><d:propstat><d:prop>
    <d:displayname>Privat</d:displayname>
    <d:resourcetype><d:collection/><c:calendar/></d:resourcetype>
    <c:supported-calendar-component-set><c:comp name="VEVENT"/></c:supported-calendar-component-set>
  </d:prop></d:propstat></d:response>
</d:multistatus>"#;

        let server = MockServer::start().await;
        for (at, body) in [
            ("/dav", PRINCIPAL.to_string()),
            // Me -> delegate A; A -> delegate B. B must never be asked.
            ("/dav/me/", home_with_delegate("/dav/me/cal/", "/dav/a/")),
            ("/dav/a/", home_with_delegate("/dav/a/cal/", "/dav/b/")),
            ("/dav/me/cal/", CALENDARS.to_string()),
            ("/dav/a/cal/", CALENDARS.to_string()),
        ] {
            Mock::given(http_method("PROPFIND"))
                .and(path(at))
                .respond_with(ResponseTemplate::new(207).set_body_string(body))
                .mount(&server)
                .await;
        }
        Mock::given(http_method("PROPFIND"))
            .and(path("/dav/b/"))
            .respond_with(ResponseTemplate::new(207).set_body_string(PRINCIPAL))
            .expect(0)
            .mount(&server)
            .await;

        CalDavClient::new(&format!("{}/dav", server.uri()), "me", "hunter2")
            .unwrap()
            .list_collections()
            .await
            .unwrap();
    }
}
