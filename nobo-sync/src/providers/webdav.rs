//! WebDAV backend, working against any WebDAV server, with no account
//! system beyond HTTP Basic Auth.
//!
//! Layout under one configured root:
//! - `{root}/index.enc` -- the one encrypted index blob, mutable, written
//!   conditionally (see below)
//! - `{root}/files/{content_hash}` -- content-addressed, so re-uploading a
//!   present hash is always a safe no-op

use async_trait::async_trait;
use reqwest::{Client, StatusCode};
use std::path::Path;

use super::{IndexVersion, SyncProvider};

pub struct WebDavProvider {
    client: Client,
    /// e.g. `https://example.com/dav/nobo` -- no trailing slash.
    root: String,
    username: String,
}

impl WebDavProvider {
    pub fn new(root: String, username: String, password: String) -> Result<Self, String> {
        use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION};

        let mut headers = HeaderMap::new();
        let credentials = nobo_webdav::basic_auth_header(&username, &password);
        let mut auth_value = HeaderValue::from_str(&credentials).map_err(|e| format!("invalid credentials: {e}"))?;
        auth_value.set_sensitive(true);
        headers.insert(AUTHORIZATION, auth_value);

        // No whole-request timeout: this client carries file transfers,
        // and the connect timeout it brings from nobo_http is what catches
        // an unreachable server.
        let client = nobo_http::builder().default_headers(headers).build().map_err(|e| nobo_http::describe_error(&e))?;

        Ok(Self { client, root: root.trim_end_matches('/').to_string(), username })
    }

    fn index_url(&self) -> String {
        format!("{}/index.enc", self.root)
    }

    fn files_dir_url(&self) -> String {
        format!("{}/files/", self.root)
    }

    fn file_url(&self, content_hash: &str) -> String {
        format!("{}/files/{}", self.root, content_hash)
    }

    /// Best-effort: creates `{root}` and `{root}/files/` if they don't
    /// exist yet (first sync from a fresh account) -- WebDAV servers 409
    /// a PUT/PROPFIND into a directory that was never MKCOL'd, so this has
    /// to happen before anything else can work at all.
    async fn ensure_directories(&self) -> Result<(), String> {
        for url in [self.root.clone(), self.files_dir_url()] {
            let resp = self
                .client
                .request(nobo_webdav::method("MKCOL"), &url)
                .send()
                .await
                .map_err(|e| format!("MKCOL {url} failed: {}", nobo_http::request_failed(&e)))?;
            // 201 Created, or 405 Method Not Allowed (already exists) are
            // both fine; anything else is a real problem.
            if !(resp.status() == StatusCode::CREATED || resp.status() == StatusCode::METHOD_NOT_ALLOWED) {
                return Err(format!("failed to create {url}: HTTP {}", resp.status()));
            }
        }
        Ok(())
    }
}

#[async_trait]
impl SyncProvider for WebDavProvider {
    async fn download_index(&self) -> Result<Option<(Vec<u8>, IndexVersion)>, String> {
        let resp = self.client.get(self.index_url()).send().await.map_err(|e| nobo_http::request_failed(&e))?;
        if resp.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !resp.status().is_success() {
            return Err(format!("failed to download index: HTTP {}", resp.status()));
        }
        // Best validator the server offered, in order. A weak ETag counts
        // as none: `If-Match` compares strongly, so `W/"abc"` never
        // matches and the upload would 412 forever. `Last-Modified` is the
        // fallback -- a coarser conditional, but still one the server
        // enforces (see IndexVersion::LastModified). "Nothing at all" is
        // its own answer, and now genuinely means nothing at all.
        let headers = resp.headers().clone();
        let strong_etag = headers
            .get(reqwest::header::ETAG)
            .and_then(|v| v.to_str().ok())
            .filter(|etag| !etag.trim_start().starts_with("W/") && !etag.trim().is_empty());
        let last_modified =
            headers.get(reqwest::header::LAST_MODIFIED).and_then(|v| v.to_str().ok()).filter(|value| !value.trim().is_empty());
        let version = match (strong_etag, last_modified) {
            (Some(etag), _) => IndexVersion::At(etag.to_string()),
            (None, Some(date)) => IndexVersion::LastModified(date.to_string()),
            (None, None) => IndexVersion::Unversioned,
        };
        let bytes = resp.bytes().await.map_err(|e| e.to_string())?.to_vec();
        Ok(Some((bytes, version)))
    }

    async fn upload_index(&self, encrypted_blob: &[u8], expected_version: &IndexVersion) -> Result<(), String> {
        self.ensure_directories().await?;

        // Conditional: the index is the one mutable resource here, so an
        // unconditional PUT can clobber another device's upload.
        // `expected_version` is the ETag this run started from, not a
        // fresh HEAD -- see IndexVersion for why that distinction
        // matters.
        let mut req = self.client.put(self.index_url()).body(encrypted_blob.to_vec());
        req = match expected_version {
            IndexVersion::At(etag) => req.header(reqwest::header::IF_MATCH, etag.as_str()),
            // No prior index as of this run's own download_index -- refuse
            // to overwrite one that appeared since (a device racing us to
            // be first).
            IndexVersion::Absent => req.header(reqwest::header::IF_NONE_MATCH, "*"),
            // No usable ETag, but the server dated the index -- so the
            // write is still conditional, just on a coarser validator.
            // Echoed back exactly as it arrived: an HTTP-date compares as
            // the string the server chose to print.
            IndexVersion::LastModified(date) => req.header(reqwest::header::IF_UNMODIFIED_SINCE, date.as_str()),
            // The index exists and the server offered nothing at all --
            // no usable ETag and no Last-Modified either. `If-None-Match:
            // *` here is simply wrong (it asserts the index does not
            // exist) and would 412 on every run.
            //
            // So the PUT goes out unconditional, and the lost-update
            // protection documented in docs/sync.md is genuinely absent
            // against this server. Said out loud rather than papered over
            // with a HEAD-then-PUT, which degenerates to the same
            // unconditional PUT on exactly these servers while looking
            // safe -- the incident providers/mod.rs describes.
            IndexVersion::Unversioned => {
                log::warn!(
                    "sync: der WebDAV-Server liefert für den Index keinen brauchbaren ETag -- \
                     der Schutz gegen gleichzeitige Änderungen von zwei Geräten ist gegen diesen Server nicht wirksam"
                );
                req
            }
        };

        let resp = req.send().await.map_err(|e| nobo_http::request_failed(&e))?;
        if resp.status() == StatusCode::PRECONDITION_FAILED {
            return Err("sync-index-conflict: another device updated the index first, retry the sync".to_string());
        }
        if !resp.status().is_success() {
            return Err(format!("failed to upload index: HTTP {}", resp.status()));
        }
        Ok(())
    }

    async fn upload_file(&self, content_hash: &str, local_path: &Path) -> Result<(), String> {
        self.ensure_directories().await?;
        let file = tokio::fs::File::open(local_path).await.map_err(|e| format!("failed to open {}: {e}", local_path.display()))?;
        let resp = self
            .client
            .put(self.file_url(content_hash))
            .body(reqwest::Body::from(file))
            .send()
            .await
            .map_err(|e| nobo_http::request_failed(&e))?;
        if !resp.status().is_success() {
            return Err(format!("failed to upload {content_hash}: HTTP {}", resp.status()));
        }
        Ok(())
    }

    async fn download_file(&self, content_hash: &str, local_path: &Path) -> Result<(), String> {
        let resp = self.client.get(self.file_url(content_hash)).send().await.map_err(|e| nobo_http::request_failed(&e))?;
        // Distinct message (matching PocketBaseProvider's, see
        // orchestrator::is_missing_remotely_error) so a genuinely-gone blob
        // -- not some other HTTP failure -- gets the same self-heal instead
        // of hard-aborting the whole sync.
        if resp.status() == StatusCode::NOT_FOUND {
            return Err(format!("file {content_hash} not found remotely"));
        }
        if !resp.status().is_success() {
            return Err(format!("failed to download {content_hash}: HTTP {}", resp.status()));
        }

        use tokio::io::AsyncWriteExt;
        let mut out = tokio::fs::File::create(local_path).await.map_err(|e| format!("failed to create {}: {e}", local_path.display()))?;
        let mut stream = resp.bytes_stream();
        use futures_util::StreamExt;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| e.to_string())?;
            out.write_all(&chunk).await.map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    async fn delete_file(&self, content_hash: &str) -> Result<(), String> {
        let resp = self.client.delete(self.file_url(content_hash)).send().await.map_err(|e| nobo_http::request_failed(&e))?;
        // A file already gone (404) is not an error -- deleting is idempotent.
        if !resp.status().is_success() && resp.status() != StatusCode::NOT_FOUND {
            return Err(format!("failed to delete {content_hash}: HTTP {}", resp.status()));
        }
        Ok(())
    }

    async fn list_remote_files(&self) -> Result<Vec<String>, String> {
        self.ensure_directories().await?;

        let body = r#"<?xml version="1.0" encoding="utf-8" ?>
            <D:propfind xmlns:D="DAV:"><D:prop><D:resourcetype/></D:prop></D:propfind>"#;

        let resp = self
            .client
            .request(nobo_webdav::method("PROPFIND"), self.files_dir_url())
            .header("Depth", "1")
            .header(reqwest::header::CONTENT_TYPE, "application/xml")
            .body(body)
            .send()
            .await
            .map_err(|e| nobo_http::request_failed(&e))?;

        if !resp.status().is_success() {
            return Err(format!("failed to list remote files: HTTP {}", resp.status()));
        }

        let xml = resp.text().await.map_err(|e| e.to_string())?;
        parse_propfind_hrefs(&xml)
    }

    fn label(&self) -> String {
        format!("WebDAV — {}", self.root)
    }

    fn account_label(&self) -> String {
        self.username.clone()
    }

    async fn remote_storage_bytes(&self) -> Result<u64, String> {
        self.ensure_directories().await?;

        let body = r#"<?xml version="1.0" encoding="utf-8" ?>
            <D:propfind xmlns:D="DAV:"><D:prop><D:resourcetype/><D:getcontentlength/></D:prop></D:propfind>"#;

        let resp = self
            .client
            .request(nobo_webdav::method("PROPFIND"), self.files_dir_url())
            .header("Depth", "1")
            .header(reqwest::header::CONTENT_TYPE, "application/xml")
            .body(body)
            .send()
            .await
            .map_err(|e| nobo_http::request_failed(&e))?;

        if !resp.status().is_success() {
            return Err(format!("failed to compute storage usage: HTTP {}", resp.status()));
        }

        let xml = resp.text().await.map_err(|e| e.to_string())?;
        parse_propfind_sizes(&xml)
    }
}

/// Whether this entry is a directory rather than a stored blob.
///
/// Asked of `resourcetype` (both PROPFIND bodies request it) rather than
/// of the href alone: a collection says so about itself, and the basename
/// check below only recognises the one directory this client happens to
/// have named. Both are kept -- a server that omits `resourcetype` on a
/// collection would otherwise let the `files/` listing entry count as a
/// content hash of its own, which is one too many blobs on the settings
/// page and one bogus hash in `list_remote_files`.
fn is_directory_entry(entry: &nobo_webdav::MultiStatusEntry) -> bool {
    entry.has_child_element("resourcetype", "collection")
        || basename(&entry.href).eq_ignore_ascii_case("files")
        || basename(&entry.href).is_empty()
}

/// The last path segment of an href, ignoring a trailing slash.
fn basename(href: &str) -> &str {
    href.trim_end_matches('/').rsplit('/').next().unwrap_or("")
}

/// The content hash of every stored blob in a `207 Multi-Status` body --
/// each file's own basename, with the directory listing entry for `files/`
/// itself skipped.
///
/// Parsed by `nobo_webdav`, which is where every other WebDAV response in
/// this app is parsed (see docs/architecture.md on what the crate split is
/// for). Worth naming, because it is a deliberate behaviour change: that
/// parser sets `check_end_names`, so a TRUNCATED body is now an error
/// instead of a short list. For this caller that is the safer answer by
/// far -- a short list here reads as "the server has fewer blobs than the
/// index says".
fn parse_propfind_hrefs(xml: &str) -> Result<Vec<String>, String> {
    Ok(nobo_webdav::parse_multistatus(xml)?
        .iter()
        .filter(|entry| !is_directory_entry(entry))
        .map(|entry| basename(&entry.href).to_string())
        .collect())
}

/// Sums `getcontentlength` across every stored blob, skipping directories
/// (see `is_directory_entry`).
///
/// A missing or unparsable length counts as zero rather than failing the
/// whole call: this feeds a "how much is stored" line on the settings
/// page, and one odd entry should not turn that into an error message.
fn parse_propfind_sizes(xml: &str) -> Result<u64, String> {
    Ok(nobo_webdav::parse_multistatus(xml)?
        .iter()
        .filter(|entry| !is_directory_entry(entry))
        .filter_map(|entry| entry.prop("getcontentlength"))
        .filter_map(|value| value.trim().parse::<u64>().ok())
        .sum())
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    async fn provider(server: &MockServer) -> WebDavProvider {
        WebDavProvider::new(server.uri(), "alice".into(), "hunter2".into()).unwrap()
    }

    #[test]
    fn parses_multistatus_hrefs_and_skips_the_directory_itself() {
        let xml = r#"<?xml version="1.0"?>
            <D:multistatus xmlns:D="DAV:">
                <D:response><D:href>/dav/nobo/files/</D:href></D:response>
                <D:response><D:href>/dav/nobo/files/abc123</D:href></D:response>
                <D:response><D:href>/dav/nobo/files/def456</D:href></D:response>
            </D:multistatus>"#;

        let mut hashes = parse_propfind_hrefs(xml).unwrap();
        hashes.sort();
        assert_eq!(hashes, vec!["abc123", "def456"]);
    }

    #[test]
    fn sums_contentlength_across_files_and_skips_the_directory_itself() {
        let xml = r#"<?xml version="1.0"?>
            <D:multistatus xmlns:D="DAV:">
                <D:response>
                    <D:href>/dav/nobo/files/</D:href>
                    <D:propstat><D:prop><D:resourcetype><D:collection/></D:resourcetype></D:prop></D:propstat>
                </D:response>
                <D:response>
                    <D:href>/dav/nobo/files/abc123</D:href>
                    <D:propstat><D:prop><D:resourcetype/><D:getcontentlength>100</D:getcontentlength></D:prop></D:propstat>
                </D:response>
                <D:response>
                    <D:href>/dav/nobo/files/def456</D:href>
                    <D:propstat><D:prop><D:resourcetype/><D:getcontentlength>250</D:getcontentlength></D:prop></D:propstat>
                </D:response>
            </D:multistatus>"#;

        assert_eq!(parse_propfind_sizes(xml).unwrap(), 350, "directory's own entry excluded, both files' sizes summed");
    }

    /// The behaviour change that came with parsing through `nobo_webdav`,
    /// and the reason it is wanted here: a body cut short mid-transfer
    /// used to yield the entries it had managed to read. For
    /// `list_remote_files` that is not a short answer, it is a WRONG one
    /// -- the caller compares it against the index to decide what is
    /// missing remotely.
    #[test]
    fn a_truncated_body_is_an_error_not_a_short_list() {
        let truncated = r#"<?xml version="1.0"?>
            <D:multistatus xmlns:D="DAV:">
                <D:response><D:href>/dav/nobo/files/abc123</D:href></D:response>
                <D:response><D:href>/dav/nobo/files/def4"#;

        assert!(parse_propfind_hrefs(truncated).is_err(), "hrefs: {:?}", parse_propfind_hrefs(truncated));
        assert!(parse_propfind_sizes(truncated).is_err(), "sizes: {:?}", parse_propfind_sizes(truncated));
    }

    /// A collection says so about itself; the basename check is only the
    /// fallback for a server that leaves `resourcetype` out.
    #[test]
    fn a_nested_collection_is_skipped_even_when_it_is_not_called_files() {
        let xml = r#"<?xml version="1.0"?>
            <D:multistatus xmlns:D="DAV:">
                <D:response>
                    <D:href>/dav/nobo/files/unterordner/</D:href>
                    <D:propstat><D:prop><D:resourcetype><D:collection/></D:resourcetype><D:getcontentlength>999</D:getcontentlength></D:prop></D:propstat>
                </D:response>
                <D:response>
                    <D:href>/dav/nobo/files/abc123</D:href>
                    <D:propstat><D:prop><D:resourcetype/><D:getcontentlength>100</D:getcontentlength></D:prop></D:propstat>
                </D:response>
            </D:multistatus>"#;

        assert_eq!(parse_propfind_hrefs(xml).unwrap(), vec!["abc123"]);
        assert_eq!(parse_propfind_sizes(xml).unwrap(), 100);
    }

    #[tokio::test]
    async fn download_index_returns_none_on_404() {
        let server = MockServer::start().await;
        Mock::given(method("GET")).and(path("/index.enc")).respond_with(ResponseTemplate::new(404)).mount(&server).await;

        let provider = provider(&server).await;
        assert_eq!(provider.download_index().await.unwrap(), None);
    }

    #[tokio::test]
    async fn download_index_returns_the_body_on_success() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/index.enc"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"encrypted-bytes".to_vec()))
            .mount(&server)
            .await;

        let provider = provider(&server).await;
        // A 200 with neither an ETag nor a Last-Modified is "it exists,
        // but there is nothing to write against" -- NOT the same answer as
        // a 404, which is what the old `Option<String>` shape forced them
        // to share and what made every later upload send `If-None-Match:
        // *` and 412 forever.
        assert_eq!(provider.download_index().await.unwrap(), Some((b"encrypted-bytes".to_vec(), IndexVersion::Unversioned)));
    }

    /// `If-Match` compares strongly, so a weak validator can never match
    /// -- and gzip-transforming proxies and CDNs hand them out routinely.
    /// With no date to fall back on either, there is nothing left.
    #[tokio::test]
    async fn a_weak_etag_counts_as_no_usable_validator() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/index.enc"))
            .respond_with(ResponseTemplate::new(200).insert_header("etag", "W/\"abc123\"").set_body_bytes(b"encrypted-bytes".to_vec()))
            .mount(&server)
            .await;

        let provider = provider(&server).await;
        let (_, version) = provider.download_index().await.unwrap().unwrap();

        assert_eq!(version, IndexVersion::Unversioned);
    }

    /// The wedge itself: an index that exists but has no usable validator
    /// must not be written with "only create if absent".
    #[tokio::test]
    async fn an_unversioned_index_is_written_without_if_none_match() {
        let server = MockServer::start().await;
        Mock::given(method("MKCOL")).respond_with(ResponseTemplate::new(405)).mount(&server).await;
        Mock::given(method("PUT"))
            .and(path("/index.enc"))
            .and(wiremock::matchers::header_exists("if-none-match"))
            .respond_with(ResponseTemplate::new(412))
            .mount(&server)
            .await;
        Mock::given(method("PUT")).and(path("/index.enc")).respond_with(ResponseTemplate::new(204)).mount(&server).await;

        let provider = provider(&server).await;

        provider.upload_index(b"blob", &IndexVersion::Unversioned).await.unwrap();
    }

    /// The gap this closes: a server with no ETag worth having still
    /// dates its files, and a date is enough for the SERVER to refuse a
    /// write -- which is the part that matters. Before this, such a
    /// server got an unconditional PUT and no protection at all.
    #[tokio::test]
    async fn a_weak_etag_falls_back_to_the_date_the_server_gave() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/index.enc"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("etag", "W/\"abc123\"")
                    .insert_header("Last-Modified", "Wed, 03 Sep 2026 10:11:12 GMT")
                    .set_body_bytes(b"encrypted-bytes".to_vec()),
            )
            .mount(&server)
            .await;

        let provider = provider(&server).await;
        let (_, version) = provider.download_index().await.unwrap().unwrap();

        assert_eq!(version, IndexVersion::LastModified("Wed, 03 Sep 2026 10:11:12 GMT".to_string()));
    }

    /// A strong ETag wins wherever there is one: it is exact, where a date
    /// cannot tell two writes in the same second apart.
    #[tokio::test]
    async fn a_strong_etag_wins_over_the_date() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/index.enc"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("ETag", "\"abc123\"")
                    .insert_header("Last-Modified", "Wed, 03 Sep 2026 10:11:12 GMT")
                    .set_body_bytes(b"encrypted-bytes".to_vec()),
            )
            .mount(&server)
            .await;

        let provider = provider(&server).await;
        let (_, version) = provider.download_index().await.unwrap().unwrap();

        assert_eq!(version, IndexVersion::At("\"abc123\"".to_string()));
    }

    /// And the write itself is conditional on it -- the server refuses,
    /// not the client hopes.
    #[tokio::test]
    async fn a_dated_index_is_written_with_if_unmodified_since() {
        let server = MockServer::start().await;
        Mock::given(method("MKCOL")).respond_with(ResponseTemplate::new(405)).mount(&server).await;
        Mock::given(method("PUT"))
            .and(path("/index.enc"))
            .and(wiremock::matchers::header_exists("if-unmodified-since"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;

        let provider = provider(&server).await;
        provider.upload_index(b"new content", &IndexVersion::LastModified("Wed, 03 Sep 2026 10:11:12 GMT".to_string())).await.unwrap();

        // Matched on presence above and on the value here: wiremock's own
        // header matcher reads a comma as a list separator, and every
        // HTTP-date has one in it.
        let put = server
            .received_requests()
            .await
            .unwrap()
            .into_iter()
            .find(|r| r.method == wiremock::http::Method::PUT)
            .expect("the index was written");
        assert_eq!(put.headers.get("if-unmodified-since").unwrap(), "Wed, 03 Sep 2026 10:11:12 GMT");
    }

    /// The other device got there first, and the server says so.
    #[tokio::test]
    async fn a_date_that_no_longer_matches_is_a_conflict() {
        let server = MockServer::start().await;
        Mock::given(method("MKCOL")).respond_with(ResponseTemplate::new(405)).mount(&server).await;
        Mock::given(method("PUT")).and(path("/index.enc")).respond_with(ResponseTemplate::new(412)).mount(&server).await;

        let provider = provider(&server).await;
        let err = provider
            .upload_index(b"new content", &IndexVersion::LastModified("Wed, 03 Sep 2026 10:11:12 GMT".to_string()))
            .await
            .unwrap_err();

        assert!(err.contains("sync-index-conflict"), "got: {err}");
    }

    #[tokio::test]
    async fn download_index_captures_the_etag_as_the_index_version() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/index.enc"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"encrypted-bytes".to_vec()).insert_header("ETag", "\"abc123\""))
            .mount(&server)
            .await;

        let provider = provider(&server).await;
        let (_, version) = provider.download_index().await.unwrap().unwrap();
        assert_eq!(version, IndexVersion::At("\"abc123\"".to_string()));
    }

    #[tokio::test]
    async fn requests_carry_basic_auth() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/index.enc"))
            .and(header("Authorization", nobo_webdav::basic_auth_header("alice", "hunter2").as_str()))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let provider = provider(&server).await;
        // Would fail to match any mock (wiremock 404s with a diagnostic)
        // if the Authorization header were missing or wrong.
        provider.download_index().await.unwrap();
    }

    #[tokio::test]
    async fn upload_index_conflict_is_reported_distinctly() {
        let server = MockServer::start().await;
        Mock::given(method("MKCOL")).respond_with(ResponseTemplate::new(405)).mount(&server).await;
        Mock::given(method("PUT")).and(path("/index.enc")).respond_with(ResponseTemplate::new(412)).mount(&server).await;

        let provider = provider(&server).await;
        let err = provider.upload_index(b"new content", &IndexVersion::Absent).await.unwrap_err();
        assert!(err.contains("sync-index-conflict"), "got: {err}");
    }

    #[tokio::test]
    async fn upload_index_succeeds_when_no_conflict() {
        let server = MockServer::start().await;
        Mock::given(method("MKCOL")).respond_with(ResponseTemplate::new(405)).mount(&server).await;
        Mock::given(method("PUT"))
            .and(path("/index.enc"))
            .and(header("If-None-Match", "*"))
            .respond_with(ResponseTemplate::new(201))
            .mount(&server)
            .await;

        let provider = provider(&server).await;
        provider.upload_index(b"new content", &IndexVersion::Absent).await.unwrap();
    }

    #[tokio::test]
    async fn upload_index_uses_the_originally_captured_version_not_a_fresh_one() {
        // The whole point of this fix: EXPECTED_VERSION is whatever
        // download_index saw at the START of the run, not re-fetched right
        // before the write -- so the PUT here must carry the etag this
        // test passes in directly, with no HEAD request involved at all.
        let server = MockServer::start().await;
        Mock::given(method("MKCOL")).respond_with(ResponseTemplate::new(405)).mount(&server).await;
        Mock::given(method("PUT"))
            .and(path("/index.enc"))
            .and(header("If-Match", "\"original-etag\""))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;

        let provider = provider(&server).await;
        provider.upload_index(b"new content", &IndexVersion::At("\"original-etag\"".to_string())).await.unwrap();
    }

    #[tokio::test]
    async fn delete_of_an_already_missing_file_is_not_an_error() {
        let server = MockServer::start().await;
        Mock::given(method("DELETE")).and(path("/files/abc123")).respond_with(ResponseTemplate::new(404)).mount(&server).await;

        let provider = provider(&server).await;
        provider.delete_file("abc123").await.unwrap();
    }
}
