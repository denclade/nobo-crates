//! PocketBase's realtime API (Server-Sent Events over `/api/realtime`) --
//! lets this device learn about another device's sync almost immediately,
//! instead of waiting for the periodic interval (see App.tsx's
//! AUTO_SYNC_INTERVAL_MS) to eventually notice. Subscribes to the
//! `sync_index` collection -- the SAME API rules that already gate which
//! records a user can VIEW (see docs/sync.md) also gate which realtime
//! events they receive for it, so a bare collection-level subscription is
//! both simpler and correctly scoped without needing this device to know
//! its own record id up front.
//!
//! Self-healing by construction: on any stream error or disconnect
//! (network change, the OS suspending the app in the background, ...) this
//! reconnects with exponential backoff rather than giving up -- there's no
//! separate "app resumed" hook wired in (this app has none; see
//! rescan.rs's own doc comment on why mobile has no persistent file
//! watcher either, for the same underlying reason), so a live push simply
//! doesn't fire while genuinely backgrounded, and picks back up on its own
//! whenever the process next gets scheduled network time.

use futures_util::StreamExt;
use reqwest::Client;
use std::time::Duration;

const INITIAL_BACKOFF: Duration = Duration::from_secs(2);
const MAX_BACKOFF: Duration = Duration::from_secs(60);

/// Runs forever (until the returned JoinHandle is aborted) -- connects,
/// subscribes to `sync_index`, calls ON_CHANGE for every subsequent change
/// event, and transparently reconnects (with backoff) on any error.
/// Spawned once per active PocketBase session (see
/// PocketBaseProvider::spawn_realtime_listener/nobo_sync::commands), aborted
/// on logout or when switching to a different account/provider.
pub fn spawn(client: Client, base_url: String, on_change: impl Fn() + Send + Sync + 'static) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut backoff = INITIAL_BACKOFF;
        loop {
            match run_once(&client, &base_url, &on_change).await {
                // The stream ended cleanly (server closed it) -- not a
                // failure, reconnect right away.
                Ok(()) => backoff = INITIAL_BACKOFF,
                Err(e) => {
                    log::warn!("sync: realtime listener disconnected, reconnecting in {}s: {e}", backoff.as_secs());
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                }
            }
        }
    })
}

async fn run_once(client: &Client, base_url: &str, on_change: &(impl Fn() + Send + Sync + 'static)) -> Result<(), String> {
    let resp = client
        .get(format!("{base_url}/api/realtime"))
        .header(reqwest::header::ACCEPT, "text/event-stream")
        .send()
        .await
        .map_err(|e| nobo_http::request_failed(&e))?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }

    let mut stream = resp.bytes_stream();
    let mut buf = String::new();
    let mut subscribed = false;

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| e.to_string())?;
        buf.push_str(&String::from_utf8_lossy(&chunk));

        // SSE events are separated by a blank line -- process every
        // complete one currently buffered, keep any trailing partial event
        // (split across chunk boundaries) for the next chunk.
        while let Some(pos) = buf.find("\n\n") {
            let raw_event: String = buf.drain(..pos + 2).collect();
            let (event_name, data) = parse_sse_event(&raw_event);

            if event_name == "PB_CONNECT" && !subscribed {
                let Some(client_id) = data.as_deref().and_then(extract_client_id) else {
                    return Err("PB_CONNECT event had no clientId".to_string());
                };
                subscribe(client, base_url, &client_id).await?;
                subscribed = true;
            } else if event_name == "sync_index" {
                on_change();
            }
        }
    }

    // `Ok(())` means "the server closed a working stream", and the caller
    // reconnects immediately on it -- deliberately, so a normal server-side
    // close costs no delay at all.
    //
    // A 2xx whose body ends without a PB_CONNECT ever arriving is not
    // that. It is a middlebox answering with a short non-SSE body, a load
    // balancer closing the response, a misrouted URL that still returns
    // 200 -- and taking the no-delay path on it makes this task reconnect
    // as fast as the round trip allows, forever, and silently, because the
    // warning lives in the Err arm. Reported as the failure it is, so it
    // lands in the backoff that already exists.
    //
    // Deliberately NOT also rejecting short-but-subscribed streams: any
    // duration floor is a guess, and it would break the reconnect-without-
    // delay behaviour this loop is explicitly built for.
    if !subscribed {
        return Err("realtime stream ended before PB_CONNECT -- not an event stream".to_string());
    }
    Ok(())
}

async fn subscribe(client: &Client, base_url: &str, client_id: &str) -> Result<(), String> {
    let resp = client
        .post(format!("{base_url}/api/realtime"))
        .json(&serde_json::json!({ "clientId": client_id, "subscriptions": ["sync_index"] }))
        .send()
        .await
        .map_err(|e| nobo_http::request_failed(&e))?;
    if !resp.status().is_success() {
        return Err(format!("failed to subscribe: HTTP {}", resp.status()));
    }
    Ok(())
}

/// Splits one raw (still `\n\n`-terminated) SSE event block into its
/// `event:` name (defaults to "message", the SSE spec's own default, if
/// absent) and its `data:` payload (concatenated if split across multiple
/// `data:` lines, per the spec) -- deliberately hand-rolled rather than
/// pulling in a dedicated SSE crate for what's a handful of lines, since
/// this is the only place in the app that needs it.
fn parse_sse_event(raw: &str) -> (String, Option<String>) {
    let mut event_name = String::from("message");
    let mut data_lines = Vec::new();
    for line in raw.lines() {
        if let Some(rest) = line.strip_prefix("event:") {
            event_name = rest.trim().to_string();
        } else if let Some(rest) = line.strip_prefix("data:") {
            data_lines.push(rest.trim().to_string());
        }
    }
    (event_name, if data_lines.is_empty() { None } else { Some(data_lines.join("\n")) })
}

/// Pulls `clientId` out of a `PB_CONNECT` event's JSON payload
/// (`{"clientId":"..."}`).
fn extract_client_id(json: &str) -> Option<String> {
    let parsed: serde_json::Value = serde_json::from_str(json).ok()?;
    parsed.get("clientId")?.as_str().map(String::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_event_name_and_single_line_data() {
        let raw = "event: PB_CONNECT\ndata: {\"clientId\":\"abc123\"}\n\n";
        let (name, data) = parse_sse_event(raw);
        assert_eq!(name, "PB_CONNECT");
        assert_eq!(data.as_deref(), Some("{\"clientId\":\"abc123\"}"));
    }

    #[test]
    fn defaults_to_message_when_no_event_name_given() {
        let (name, _) = parse_sse_event("data: hello\n\n");
        assert_eq!(name, "message");
    }

    #[test]
    fn concatenates_multiple_data_lines_per_the_sse_spec() {
        let raw = "event: sync_index\ndata: line one\ndata: line two\n\n";
        let (_, data) = parse_sse_event(raw);
        assert_eq!(data.as_deref(), Some("line one\nline two"));
    }

    #[test]
    fn ignores_id_and_other_unknown_fields() {
        let raw = "id: 42\nevent: sync_index\ndata: {}\nretry: 3000\n\n";
        let (name, data) = parse_sse_event(raw);
        assert_eq!(name, "sync_index");
        assert_eq!(data.as_deref(), Some("{}"));
    }

    #[test]
    fn extract_client_id_reads_the_field() {
        assert_eq!(extract_client_id(r#"{"clientId":"xyz"}"#), Some("xyz".to_string()));
    }

    #[test]
    fn extract_client_id_is_none_for_missing_field_or_invalid_json() {
        assert_eq!(extract_client_id(r#"{"other":"value"}"#), None);
        assert_eq!(extract_client_id("not json"), None);
    }

    /* --- run_once / spawn: end-to-end against a mock server --- */

    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use wiremock::matchers::{body_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn sse_stream(events: &str) -> ResponseTemplate {
        ResponseTemplate::new(200).insert_header("Content-Type", "text/event-stream").set_body_bytes(events.as_bytes().to_vec())
    }

    #[tokio::test]
    async fn run_once_subscribes_after_pb_connect_and_calls_on_change_for_sync_index_events() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/realtime"))
            .respond_with(sse_stream(
                "event: PB_CONNECT\ndata: {\"clientId\":\"client-abc\"}\n\n\
                 event: sync_index\ndata: {\"action\":\"update\"}\n\n",
            ))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/realtime"))
            .and(body_json(serde_json::json!({ "clientId": "client-abc", "subscriptions": ["sync_index"] })))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;

        let calls = Arc::new(AtomicUsize::new(0));
        let calls_clone = calls.clone();
        run_once(&Client::new(), &server.uri(), &move || {
            calls_clone.fetch_add(1, Ordering::SeqCst);
        })
        .await
        .unwrap();

        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn run_once_calls_on_change_once_per_sync_index_event_not_just_once_overall() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/realtime"))
            .respond_with(sse_stream(
                "event: PB_CONNECT\ndata: {\"clientId\":\"client-abc\"}\n\n\
                 event: sync_index\ndata: {}\n\n\
                 event: sync_index\ndata: {}\n\n\
                 event: sync_index\ndata: {}\n\n",
            ))
            .mount(&server)
            .await;
        Mock::given(method("POST")).and(path("/api/realtime")).respond_with(ResponseTemplate::new(204)).mount(&server).await;

        let calls = Arc::new(AtomicUsize::new(0));
        let calls_clone = calls.clone();
        run_once(&Client::new(), &server.uri(), &move || {
            calls_clone.fetch_add(1, Ordering::SeqCst);
        })
        .await
        .unwrap();

        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn run_once_errors_when_the_initial_connection_fails() {
        let server = MockServer::start().await;
        Mock::given(method("GET")).and(path("/api/realtime")).respond_with(ResponseTemplate::new(500)).mount(&server).await;

        let err = run_once(&Client::new(), &server.uri(), &|| {}).await.unwrap_err();
        assert!(err.contains("500"), "got: {err}");
    }

    #[tokio::test]
    async fn run_once_errors_when_subscribing_fails() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/realtime"))
            .respond_with(sse_stream("event: PB_CONNECT\ndata: {\"clientId\":\"client-abc\"}\n\n"))
            .mount(&server)
            .await;
        Mock::given(method("POST")).and(path("/api/realtime")).respond_with(ResponseTemplate::new(500)).mount(&server).await;

        let err = run_once(&Client::new(), &server.uri(), &|| {}).await.unwrap_err();
        assert!(err.contains("failed to subscribe"), "got: {err}");
    }

    #[tokio::test]
    async fn run_once_errors_when_pb_connect_has_no_client_id() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/realtime"))
            .respond_with(sse_stream("event: PB_CONNECT\ndata: {}\n\n"))
            .mount(&server)
            .await;

        let err = run_once(&Client::new(), &server.uri(), &|| {}).await.unwrap_err();
        assert!(err.contains("clientId"), "got: {err}");
    }

    /// End-to-end confirmation of the RECONNECT behavior `spawn` adds on
    /// top of `run_once` -- a stream that ends (server closes it, as a real
    /// PocketBase connection eventually does) must not be treated as a
    /// permanent failure: the listener reconnects and keeps delivering
    /// events indefinitely, not just for the first connection's lifetime.
    #[tokio::test]
    async fn spawn_reconnects_after_the_stream_ends_and_keeps_delivering_events() {
        let server = MockServer::start().await;
        // Each GET only ever returns ONE sync_index event before the
        // (mocked) stream ends -- a real listener would have to reconnect
        // to see a second one.
        Mock::given(method("GET"))
            .and(path("/api/realtime"))
            .respond_with(sse_stream(
                "event: PB_CONNECT\ndata: {\"clientId\":\"client-abc\"}\n\n\
                 event: sync_index\ndata: {}\n\n",
            ))
            .mount(&server)
            .await;
        Mock::given(method("POST")).and(path("/api/realtime")).respond_with(ResponseTemplate::new(204)).mount(&server).await;

        let calls = Arc::new(AtomicUsize::new(0));
        let calls_clone = calls.clone();
        let handle = spawn(Client::new(), server.uri(), move || {
            calls_clone.fetch_add(1, Ordering::SeqCst);
        });

        // Reconnects happen immediately (INITIAL_BACKOFF only applies
        // after an actual ERROR, not a clean stream end) -- a short
        // real-time budget is enough to observe several full cycles.
        tokio::time::sleep(Duration::from_millis(200)).await;
        handle.abort();

        assert!(calls.load(Ordering::SeqCst) >= 2, "expected multiple reconnect cycles, got {}", calls.load(Ordering::SeqCst));
    }
}
