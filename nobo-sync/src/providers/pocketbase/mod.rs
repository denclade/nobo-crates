pub mod auth;
pub mod realtime;

use async_trait::async_trait;
use reqwest::{Client, StatusCode};
use serde::Deserialize;
use std::path::Path;

use super::{IndexVersion, SyncProvider};
use auth::AuthSession;

/// Content-addressed like the WebDAV layout: `user_files` records are
/// keyed by content hash, never by path or title, so the server only sees
/// opaque blobs plus one encrypted index record per user mapping them back
/// to real paths. See docs/sync.md for the schema.
///
/// The index is a PocketBase file field rather than an inline base64 text
/// field: text fields have an enforced max length -- setting it to 0 falls
/// back to 5000 characters rather than meaning unlimited -- which a real
/// vault's index exceeds immediately. A file field has no ceiling and is
/// not returned inline in ordinary
/// list/view responses, so `find_index_record` uses the exact same
/// find-by-id-then-fetch-file pattern `find_file_record` already does.
pub struct PocketBaseProvider {
    client: Client,
    base_url: String,
    user_id: String,
    /// Purely for `account_label()`'s display purposes -- empty unless
    /// `with_email` was called. Kept separate from `new()` itself (rather
    /// than an extra required param there) so the many existing tests that
    /// only care about this provider's auth/request-shaping side don't all
    /// need to supply an email they never look at.
    email: String,
}

#[derive(Deserialize)]
struct ListResponse<T> {
    page: u32,
    #[serde(rename = "totalPages")]
    total_pages: u32,
    items: Vec<T>,
}

/// Shape shared by `sync_index` and `user_files` records alike -- both are
/// just "a record with a file field on it" from this client's point of
/// view; only the collection name and the record's other fields differ.
/// `updated` is a PocketBase system field present on every record by
/// default (an RFC3339 timestamp, bumped automatically on every write) --
/// used as `sync_index`'s own IndexVersion marker, since PocketBase's REST
/// API has no ETag/If-Match equivalent to build real conditional writes on
/// top of (see `upload_index`'s own doc comment).
#[derive(Deserialize)]
struct RecordWithFile {
    id: String,
    file: String,
    updated: String,
}

/// A 403 on any of these List/Search endpoints (as opposed to login, which
/// already succeeded by the time this runs) is almost always the
/// PocketBase collection's "List/Search" API rule specifically -- distinct
/// from "View" even though the two look interchangeable at a glance, see
/// docs/sync.md's setup guide -- left on its default (superusers-only)
/// because it was overlooked while copying the same rule into all five
/// rule fields. Names that directly instead of just relaying a bare
/// "HTTP 403 Forbidden", which gives no hint what to actually check.
fn list_forbidden_hint(collection: &str, status: StatusCode) -> String {
    nobo_core::t!(
        "Zugriff auf „{collection}“ verweigert (HTTP {status}). Vermutlich fehlt die „List/Search“-API-Regel dieser Collection in PocketBase (Collections → {collection} → API Rules) -- das ist ein eigenes Feld, getrennt von „View“, und wird deshalb leicht übersehen. Siehe docs/sync.md.",
        "Access to \"{collection}\" was denied (HTTP {status}). Most likely this collection's \"List/Search\" API rule is still on its default in PocketBase (Collections -> {collection} -> API Rules) -- a field of its own, separate from \"View\", which is why it is easy to miss. See docs/sync.md."
    )
}

impl PocketBaseProvider {
    pub fn new(base_url: String, session: &AuthSession) -> Result<Self, String> {
        use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION};

        let mut headers = HeaderMap::new();
        // Raw token, no "Bearer " prefix -- PocketBase's own convention,
        // not the more common OAuth-style scheme.
        let mut auth_value = HeaderValue::from_str(&session.token).map_err(|e| format!("invalid token: {e}"))?;
        auth_value.set_sensitive(true);
        headers.insert(AUTHORIZATION, auth_value);

        // No whole-request timeout: file uploads and downloads go
        // through this same client. What catches an unreachable server is
        // the connect timeout nobo_http::builder() brings.
        let builder = nobo_http::builder().default_headers(headers);
        let client = builder.build().map_err(|e| nobo_http::describe_error(&e))?;

        Ok(Self { client, base_url: base_url.trim_end_matches('/').to_string(), user_id: session.user_id.clone(), email: String::new() })
    }

    pub fn with_email(mut self, email: String) -> Self {
        self.email = email;
        self
    }

    fn owner_filter(&self) -> String {
        format!("(owner='{}')", self.user_id)
    }

    async fn find_index_record(&self) -> Result<Option<RecordWithFile>, String> {
        let resp = self
            .client
            .get(format!("{}/api/collections/sync_index/records", self.base_url))
            // Two, not one: asking for one cannot tell "there is an index"
            // apart from "there are two", and two is a state that leads
            // somewhere very bad. The unique index that would rule it out
            // has to be created by hand (see docs/sync.md), so a server
            // without it lets two devices doing their first sync at the
            // same moment each POST their own record. From then on a
            // device may read the OTHER one -- and against a foreign index
            // every local file is `(tracked, absent remotely)`, which
            // `diff.rs` turns into DeleteLocal. The absent-index guard in
            // `run_sync` does not help: an index IS present.
            .query(&[("filter", self.owner_filter().as_str()), ("perPage", "2")])
            .send()
            .await
            .map_err(|e| nobo_http::request_failed(&e))?;

        if resp.status() == StatusCode::FORBIDDEN {
            return Err(list_forbidden_hint("sync_index", resp.status()));
        }
        if !resp.status().is_success() {
            return Err(format!("failed to look up sync index: HTTP {}", resp.status()));
        }
        let list: ListResponse<RecordWithFile> = resp.json().await.map_err(|e| e.to_string())?;
        if list.items.len() > 1 || list.total_pages > 1 {
            return Err(nobo_core::t!(
                "Dieses Konto hat mehr als einen Sync-Index auf dem Server. Die Synchronisierung wurde abgebrochen, damit nicht gegen den falschen abgeglichen wird. \
                 Auf dem Server muss der eindeutige Index über sync_index(owner) angelegt und der überzählige Eintrag entfernt werden -- siehe docs/sync.md.",
                "This account has more than one sync index on the server. Syncing was stopped so that it is not compared against the wrong one. \
                 Create the unique index on sync_index(owner) on the server and remove the surplus record -- see docs/sync.md."
            ));
        }
        match list.items.into_iter().next() {
            Some(record) => Ok(Some(record)),
            // An empty list is NOT proof that this account has no index.
            //
            // PocketBase does not reject an expired or invalid bearer
            // token -- it treats the request as a guest, and the
            // collection's documented list rule
            // (`@request.auth.id != "" && ...`, docs/sync.md) is appended
            // as a filter. So an expired session returns a perfectly
            // ordinary 200 with `items: []`, indistinguishable from a
            // fresh account.
            //
            // Read as "no index", that is the input to the worst deletion
            // path in this codebase (see run_sync's own guard on an absent
            // index). So the ambiguity is resolved here, where it arises,
            // and only in the one case where it matters -- one extra round
            // trip on a genuinely empty account, none otherwise.
            None => {
                self.assert_authenticated().await?;
                Ok(None)
            }
        }
    }

    /// Fails with a distinguishable error when this session's token is no
    /// longer valid.
    ///
    /// `auth-refresh` is the canonical probe: it needs a still-valid token
    /// and says so plainly. Deliberately used only to ASK -- adopting the
    /// refreshed token would mean rebuilding the client, whose
    /// Authorization header is baked into its default headers, and the
    /// keychain re-login that already exists is the better answer to an
    /// expired session anyway.
    async fn assert_authenticated(&self) -> Result<(), String> {
        let resp = self
            .client
            .post(format!("{}/api/collections/users/auth-refresh", self.base_url))
            .send()
            .await
            .map_err(|e| nobo_http::request_failed(&e))?;

        if resp.status() == StatusCode::UNAUTHORIZED || resp.status() == StatusCode::FORBIDDEN {
            return Err(
                nobo_core::t!("sync-auth-expired: Die Anmeldung am Sync-Konto ist abgelaufen -- bitte unter Einstellungen -> Synchronisierung neu verbinden.", "sync-auth-expired: The sync account session has expired -- please reconnect under Settings -> Sync.")
                    .to_string(),
            );
        }
        Ok(())
    }

    async fn find_file_record(&self, content_hash: &str) -> Result<Option<RecordWithFile>, String> {
        let filter = format!("(owner='{}' && contentHash='{}')", self.user_id, content_hash);
        let resp = self
            .client
            .get(format!("{}/api/collections/user_files/records", self.base_url))
            .query(&[("filter", filter.as_str()), ("perPage", "1")])
            .send()
            .await
            .map_err(|e| nobo_http::request_failed(&e))?;

        if resp.status() == StatusCode::FORBIDDEN {
            return Err(list_forbidden_hint("user_files", resp.status()));
        }
        if !resp.status().is_success() {
            return Err(format!("failed to look up file record: HTTP {}", resp.status()));
        }
        let list: ListResponse<RecordWithFile> = resp.json().await.map_err(|e| e.to_string())?;
        Ok(list.items.into_iter().next())
    }

    /// Spawns a background listener (see realtime.rs) that calls ON_CHANGE
    /// whenever another device's own sync updates the shared index --
    /// lets a caller (nobo_sync::commands) trigger an immediate resync
    /// instead of waiting for the periodic interval. Reuses this
    /// provider's own already-authenticated `client` (its Authorization
    /// header, set once in `new`, is exactly what the realtime endpoint
    /// needs too). Runs until the returned handle is aborted.
    pub fn spawn_realtime_listener(&self, on_change: impl Fn() + Send + Sync + 'static) -> tokio::task::JoinHandle<()> {
        realtime::spawn(self.client.clone(), self.base_url.clone(), on_change)
    }
}

#[async_trait]
impl SyncProvider for PocketBaseProvider {
    async fn download_index(&self) -> Result<Option<(Vec<u8>, IndexVersion)>, String> {
        let Some(record) = self.find_index_record().await? else { return Ok(None) };
        let url = format!("{}/api/files/sync_index/{}/{}", self.base_url, record.id, record.file);
        let resp = self.client.get(url).send().await.map_err(|e| nobo_http::request_failed(&e))?;
        if !resp.status().is_success() {
            return Err(format!("failed to download index: HTTP {}", resp.status()));
        }
        // Not streamed to disk like upload_file/download_file -- the index
        // is metadata (paths, hashes, wrapped keys), not file content, so
        // it stays orders of magnitude smaller than the large-attachment
        // case those two are built to avoid buffering in memory for.
        let bytes = resp.bytes().await.map_err(|e| e.to_string())?.to_vec();
        Ok(Some((bytes, IndexVersion::At(record.updated))))
    }

    async fn upload_index(&self, encrypted_blob: &[u8], expected_version: &IndexVersion) -> Result<(), String> {
        let existing = self.find_index_record().await?;

        // Conditional write without an If-Match equivalent: PocketBase's
        // REST API has no atomic "write only if unchanged". This re-fetches
        // the record -- needed anyway for its id -- and compares its
        // current `updated` against the version this run started from, not
        // one re-derived here (see IndexVersion for why that matters).
        //
        // Not a true compare-and-swap: a competing write can still land in
        // the gap before the PATCH. That window is unavoidable without a
        // server-side hook.
        // `Unversioned` cannot arise here: PocketBase always stamps
        // `updated`, so a record either exists with a version or does not
        // exist at all.
        let current_version = match existing.as_ref() {
            Some(record) => IndexVersion::At(record.updated.clone()),
            None => IndexVersion::Absent,
        };
        if &current_version != expected_version {
            return Err("sync-index-conflict: another device updated the index first, retry the sync".to_string());
        }

        let part = reqwest::multipart::Part::bytes(encrypted_blob.to_vec())
            .file_name("index.enc")
            .mime_str("application/octet-stream")
            .map_err(|e| e.to_string())?;

        let resp = match existing {
            Some(record) => {
                let form = reqwest::multipart::Form::new().part("file", part);
                self.client
                    .patch(format!("{}/api/collections/sync_index/records/{}", self.base_url, record.id))
                    .multipart(form)
                    .send()
                    .await
                    .map_err(|e| nobo_http::request_failed(&e))?
            }
            None => {
                let form = reqwest::multipart::Form::new().text("owner", self.user_id.clone()).part("file", part);
                self.client
                    .post(format!("{}/api/collections/sync_index/records", self.base_url))
                    .multipart(form)
                    .send()
                    .await
                    .map_err(|e| nobo_http::request_failed(&e))?
            }
        };

        if resp.status().is_success() {
            Ok(())
        } else {
            Err(format!("failed to upload index: HTTP {}", resp.status()))
        }
    }

    async fn upload_file(&self, content_hash: &str, local_path: &Path) -> Result<(), String> {
        // Content-addressed: an existing record for this hash already has
        // identical bytes by construction, so this is a safe, cheap no-op.
        if self.find_file_record(content_hash).await?.is_some() {
            return Ok(());
        }

        let file_name = format!("{content_hash}.bin");
        let file_bytes = tokio::fs::read(local_path).await.map_err(|e| format!("failed to read {}: {e}", local_path.display()))?;
        let size = file_bytes.len();

        let part = reqwest::multipart::Part::bytes(file_bytes).file_name(file_name).mime_str("application/octet-stream").map_err(|e| e.to_string())?;
        let form = reqwest::multipart::Form::new()
            .text("owner", self.user_id.clone())
            .text("contentHash", content_hash.to_string())
            .text("size", size.to_string())
            .part("file", part);

        let resp = self
            .client
            .post(format!("{}/api/collections/user_files/records", self.base_url))
            .multipart(form)
            .send()
            .await
            .map_err(|e| nobo_http::request_failed(&e))?;

        if resp.status().is_success() {
            Ok(())
        } else {
            Err(format!("failed to upload file {content_hash}: HTTP {}", resp.status()))
        }
    }

    async fn download_file(&self, content_hash: &str, local_path: &Path) -> Result<(), String> {
        let Some(record) = self.find_file_record(content_hash).await? else {
            return Err(format!("file {content_hash} not found remotely"));
        };

        let url = format!("{}/api/files/user_files/{}/{}", self.base_url, record.id, record.file);
        let resp = self.client.get(url).send().await.map_err(|e| nobo_http::request_failed(&e))?;
        if !resp.status().is_success() {
            return Err(format!("failed to download {content_hash}: HTTP {}", resp.status()));
        }

        use futures_util::StreamExt;
        use tokio::io::AsyncWriteExt;
        let mut out = tokio::fs::File::create(local_path).await.map_err(|e| format!("failed to create {}: {e}", local_path.display()))?;
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            out.write_all(&chunk.map_err(|e| e.to_string())?).await.map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    async fn delete_file(&self, content_hash: &str) -> Result<(), String> {
        let Some(record) = self.find_file_record(content_hash).await? else {
            return Ok(()); // Already gone -- deletion is idempotent.
        };

        let resp = self
            .client
            .delete(format!("{}/api/collections/user_files/records/{}", self.base_url, record.id))
            .send()
            .await
            .map_err(|e| nobo_http::request_failed(&e))?;

        if resp.status().is_success() || resp.status() == StatusCode::NOT_FOUND {
            Ok(())
        } else {
            Err(format!("failed to delete {content_hash}: HTTP {}", resp.status()))
        }
    }

    async fn list_remote_files(&self) -> Result<Vec<String>, String> {
        let mut hashes = Vec::new();
        let mut page = 1u32;

        loop {
            let resp = self
                .client
                .get(format!("{}/api/collections/user_files/records", self.base_url))
                .query(&[
                    ("filter", self.owner_filter()),
                    ("fields", "contentHash".to_string()),
                    ("perPage", "500".to_string()),
                    ("page", page.to_string()),
                ])
                .send()
                .await
                .map_err(|e| nobo_http::request_failed(&e))?;

            if resp.status() == StatusCode::FORBIDDEN {
                return Err(list_forbidden_hint("user_files", resp.status()));
            }
            if !resp.status().is_success() {
                return Err(format!("failed to list remote files: HTTP {}", resp.status()));
            }

            #[derive(Deserialize)]
            struct HashOnly {
                #[serde(rename = "contentHash")]
                content_hash: String,
            }
            let list: ListResponse<HashOnly> = resp.json().await.map_err(|e| e.to_string())?;
            let is_last_page = list.page >= list.total_pages;
            hashes.extend(list.items.into_iter().map(|h| h.content_hash));

            if is_last_page {
                break;
            }
            page += 1;
        }

        Ok(hashes)
    }

    fn label(&self) -> String {
        format!("PocketBase — {}", self.base_url)
    }

    fn account_label(&self) -> String {
        self.email.clone()
    }

    async fn remote_storage_bytes(&self) -> Result<u64, String> {
        let mut total = 0u64;
        let mut page = 1u32;

        loop {
            let resp = self
                .client
                .get(format!("{}/api/collections/user_files/records", self.base_url))
                .query(&[
                    ("filter", self.owner_filter()),
                    ("fields", "size".to_string()),
                    ("perPage", "500".to_string()),
                    ("page", page.to_string()),
                ])
                .send()
                .await
                .map_err(|e| nobo_http::request_failed(&e))?;

            if resp.status() == StatusCode::FORBIDDEN {
                return Err(list_forbidden_hint("user_files", resp.status()));
            }
            if !resp.status().is_success() {
                return Err(format!("failed to compute storage usage: HTTP {}", resp.status()));
            }

            #[derive(Deserialize)]
            struct SizeOnly {
                size: u64,
            }
            let list: ListResponse<SizeOnly> = resp.json().await.map_err(|e| e.to_string())?;
            let is_last_page = list.page >= list.total_pages;
            total += list.items.iter().map(|r| r.size).sum::<u64>();

            if is_last_page {
                break;
            }
            page += 1;
        }

        Ok(total)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn session(user_id: &str) -> AuthSession {
        AuthSession { token: "test-token".into(), user_id: user_id.into(), verified: true }
    }

    #[tokio::test]
    async fn download_index_returns_none_when_no_record_exists() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/collections/sync_index/records"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "page": 1, "perPage": 1, "totalItems": 0, "totalPages": 0, "items": []
            })))
            .mount(&server)
            .await;

        let provider = PocketBaseProvider::new(server.uri(), &session("u1")).unwrap();
        assert_eq!(provider.download_index().await.unwrap(), None);
    }

    #[tokio::test]
    async fn download_index_fetches_the_record_then_its_file() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/collections/sync_index/records"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "page": 1, "perPage": 1, "totalItems": 1, "totalPages": 1,
                "items": [{ "id": "idx1", "file": "index_abc123.enc", "updated": "2026-01-01 00:00:00.000Z" }]
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/files/sync_index/idx1/index_abc123.enc"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"secret bytes".to_vec()))
            .mount(&server)
            .await;

        let provider = PocketBaseProvider::new(server.uri(), &session("u1")).unwrap();
        let (bytes, version) = provider.download_index().await.unwrap().unwrap();
        assert_eq!(bytes, b"secret bytes".to_vec());
        assert_eq!(version, IndexVersion::At("2026-01-01 00:00:00.000Z".to_string()));
    }

    #[tokio::test]
    async fn upload_index_creates_a_new_record_when_none_exists() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/collections/sync_index/records"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "page": 1, "perPage": 1, "totalItems": 0, "totalPages": 0, "items": []
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/collections/sync_index/records"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let provider = PocketBaseProvider::new(server.uri(), &session("u1")).unwrap();
        provider.upload_index(b"blob", &IndexVersion::Absent).await.unwrap();
    }

    #[tokio::test]
    async fn upload_index_patches_an_existing_record() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/collections/sync_index/records"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "page": 1, "perPage": 1, "totalItems": 1, "totalPages": 1,
                "items": [{ "id": "idx1", "file": "index_old.enc", "updated": "2026-01-01 00:00:00.000Z" }]
            })))
            .mount(&server)
            .await;
        Mock::given(method("PATCH"))
            .and(path("/api/collections/sync_index/records/idx1"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let provider = PocketBaseProvider::new(server.uri(), &session("u1")).unwrap();
        provider.upload_index(b"new blob", &IndexVersion::At("2026-01-01 00:00:00.000Z".to_string())).await.unwrap();
    }

    /// The record's current `updated` no longer matches the version this
    /// run started from: another device's sync landed in between, so the
    /// write is refused rather than overwriting it.
    #[tokio::test]
    async fn upload_index_refuses_when_the_record_changed_since_expected_version_was_captured() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/collections/sync_index/records"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "page": 1, "perPage": 1, "totalItems": 1, "totalPages": 1,
                "items": [{ "id": "idx1", "file": "index_new.enc", "updated": "2026-01-01 00:05:00.000Z" }]
            })))
            .mount(&server)
            .await;
        // No PATCH mock registered -- the test fails if upload_index tries
        // to write anyway instead of refusing first.

        let provider = PocketBaseProvider::new(server.uri(), &session("u1")).unwrap();
        let err = provider.upload_index(b"new blob", &IndexVersion::At("2026-01-01 00:00:00.000Z".to_string())).await.unwrap_err();
        assert!(err.contains("sync-index-conflict"), "got: {err}");
    }

    /// EXPECTED_VERSION is `None` (this run's own download_index saw no
    /// index at all -- first-ever sync) but a record exists now -- another
    /// device's own first-ever sync raced this one. Creating a second
    /// record (or patching over theirs blind) would be just as wrong as
    /// the mismatched-timestamp case above.
    #[tokio::test]
    async fn upload_index_refuses_when_a_record_appeared_but_none_was_expected() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/collections/sync_index/records"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "page": 1, "perPage": 1, "totalItems": 1, "totalPages": 1,
                "items": [{ "id": "idx1", "file": "index_new.enc", "updated": "2026-01-01 00:05:00.000Z" }]
            })))
            .mount(&server)
            .await;

        let provider = PocketBaseProvider::new(server.uri(), &session("u1")).unwrap();
        let err = provider.upload_index(b"new blob", &IndexVersion::Absent).await.unwrap_err();
        assert!(err.contains("sync-index-conflict"), "got: {err}");
    }

    /// The mirror case: a specific prior revision is expected, but no
    /// record exists at all.
    #[tokio::test]
    async fn upload_index_refuses_when_the_expected_record_is_gone() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/collections/sync_index/records"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "page": 1, "perPage": 1, "totalItems": 0, "totalPages": 0, "items": []
            })))
            .mount(&server)
            .await;

        let provider = PocketBaseProvider::new(server.uri(), &session("u1")).unwrap();
        let err = provider.upload_index(b"new blob", &IndexVersion::At("2026-01-01 00:00:00.000Z".to_string())).await.unwrap_err();
        assert!(err.contains("sync-index-conflict"), "got: {err}");
    }

    #[tokio::test]
    async fn upload_file_skips_when_content_hash_already_exists() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/collections/user_files/records"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "page": 1, "perPage": 1, "totalItems": 1, "totalPages": 1,
                "items": [{ "id": "f1", "contentHash": "abc123", "file": "abc123.bin", "updated": "2026-01-01 00:00:00.000Z" }]
            })))
            .mount(&server)
            .await;
        // No POST mock registered at all -- the test fails if upload_file
        // tries to upload anyway instead of short-circuiting.

        let dir = tempfile::TempDir::new().unwrap();
        let local = dir.path().join("f.bin");
        std::fs::write(&local, b"content").unwrap();

        let provider = PocketBaseProvider::new(server.uri(), &session("u1")).unwrap();
        provider.upload_file("abc123", &local).await.unwrap();
    }

    #[tokio::test]
    async fn list_remote_files_paginates_through_every_page() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/collections/user_files/records"))
            .and(wiremock::matchers::query_param("page", "1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "page": 1, "perPage": 500, "totalItems": 2, "totalPages": 2,
                "items": [{ "contentHash": "hash1" }]
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/collections/user_files/records"))
            .and(wiremock::matchers::query_param("page", "2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "page": 2, "perPage": 500, "totalItems": 2, "totalPages": 2,
                "items": [{ "contentHash": "hash2" }]
            })))
            .mount(&server)
            .await;

        let provider = PocketBaseProvider::new(server.uri(), &session("u1")).unwrap();
        let mut hashes = provider.list_remote_files().await.unwrap();
        hashes.sort();
        assert_eq!(hashes, vec!["hash1", "hash2"]);
    }

    #[tokio::test]
    async fn remote_storage_bytes_sums_the_size_field_across_every_page() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/collections/user_files/records"))
            .and(wiremock::matchers::query_param("page", "1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "page": 1, "perPage": 500, "totalItems": 2, "totalPages": 2,
                "items": [{ "size": 100 }]
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/collections/user_files/records"))
            .and(wiremock::matchers::query_param("page", "2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "page": 2, "perPage": 500, "totalItems": 2, "totalPages": 2,
                "items": [{ "size": 250 }]
            })))
            .mount(&server)
            .await;

        let provider = PocketBaseProvider::new(server.uri(), &session("u1")).unwrap();
        assert_eq!(provider.remote_storage_bytes().await.unwrap(), 350);
    }

    #[test]
    fn account_label_reflects_with_email() {
        let provider = PocketBaseProvider::new("https://example.com".to_string(), &session("u1")).unwrap();
        assert_eq!(provider.account_label(), "", "no email attached yet");
        let provider = provider.with_email("me@example.com".to_string());
        assert_eq!(provider.account_label(), "me@example.com");
    }

    /// The exact failure a real user hit: List/Search left on PocketBase's
    /// default (superusers-only) while every other rule field was set --
    /// login and index download/upload work, but the very first file
    /// lookup 403s with a message that gave no hint what to check.
    #[tokio::test]
    async fn file_lookup_forbidden_names_the_likely_missing_list_rule() {
        let server = MockServer::start().await;
        Mock::given(method("GET")).and(path("/api/collections/user_files/records")).respond_with(ResponseTemplate::new(403)).mount(&server).await;

        let provider = PocketBaseProvider::new(server.uri(), &session("u1")).unwrap();
        let err = provider.upload_file("abc123", Path::new("/nonexistent")).await.unwrap_err();
        assert!(err.contains("List/Search"), "got: {err}");
        assert!(err.contains("user_files"), "got: {err}");
    }

    #[tokio::test]
    async fn delete_of_a_missing_file_is_not_an_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/collections/user_files/records"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "page": 1, "perPage": 1, "totalItems": 0, "totalPages": 0, "items": []
            })))
            .mount(&server)
            .await;

        let provider = PocketBaseProvider::new(server.uri(), &session("u1")).unwrap();
        provider.delete_file("nonexistent").await.unwrap();
    }
}
