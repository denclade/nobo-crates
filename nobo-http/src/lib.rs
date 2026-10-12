//! What everything that talks HTTP needs, whoever it is.
//!
//! Three features make network requests -- the vault sync, CalDAV and
//! ICS subscriptions -- and all three need the same two things: an error
//! a user can act on, and Android's TLS workaround. Neither belongs to
//! any one of them, and while they lived in `nobo_sync` the other two
//! could not be built without it.
//!
//! Its own crate rather than a corner of the core, because the core does
//! not talk to anything: a purely local build (`--no-default-features`)
//! would otherwise still carry reqwest and a TLS stack for nobody. Whoever
//! needs it says so in their Cargo.toml.

pub mod fetch;
/// Android only: the certificate verifier it needs (see the module's own
/// doc). Everywhere else the platform verifier is right, and the crates
/// this pulls in are not even a dependency.
#[cfg(target_os = "android")]
mod tls;

use std::time::Duration;

/// How long reaching the server itself may take.
///
/// The connection phase only, never the whole request: a client that
/// uploads a file has to allow a long transfer, but opening the socket
/// never legitimately takes long. Without it an unreachable or misrouted
/// server -- the classic emulator mistake, where `localhost` is the
/// emulator's own loopback rather than the host's 10.0.2.2 -- hangs the
/// request forever with no error at all, which is reqwest's default (no
/// timeout whatsoever).
///
/// Applied by `builder()` to every client, because the four callers that
/// used to set it each said the same 15 seconds for the same reason, and
/// one of them said it by pointing at another's comment.
const REACHING_THE_SERVER: Duration = Duration::from_secs(15);

/// A whole request that has to arrive in one piece: a calendar feed, a
/// list of tasks, an index. Long enough for a big feed on a slow line,
/// short enough that a dead server does not leave a spinner forever.
pub const ONE_FETCH: Duration = Duration::from_secs(60);

/// A whole request somebody is waiting for with a screen open -- a login,
/// a registration. Shorter on purpose: here the answer IS the feedback.
pub const SOMEBODY_IS_WAITING: Duration = Duration::from_secs(30);

/// Where every HTTP client in this app starts.
///
/// Not a convenience. On Android the certificate verifier has to be
/// replaced (see `tls`), and a client built without that does not fail
/// politely: `rustls-platform-verifier` is reqwest's default there, it is
/// never initialised in this app, and the request dies mid-handshake
/// inside whatever task made it -- no error, no log, a button that stays
/// spinning. Measured on the emulator, 2026-08-27.
///
/// It brings the connect timeout with it (see `REACHING_THE_SERVER`). The
/// timeout for the whole request stays with the caller, because that one
/// genuinely differs -- `ONE_FETCH` and `SOMEBODY_IS_WAITING` are the two
/// answers this app has, and a transfer client gives neither.
pub fn builder() -> reqwest::ClientBuilder {
    let builder = reqwest::Client::builder().connect_timeout(REACHING_THE_SERVER);
    #[cfg(target_os = "android")]
    let builder = tls::apply(builder);
    builder
}

/// `reqwest::Error`'s Display only shows the top-level wrapper, never the
/// cause buried in `.source()` -- so a wrong URL, a TLS handshake failure
/// and a DNS error all read as "error sending request for url" alone.
pub fn describe_error(e: &dyn std::error::Error) -> String {
    let mut message = e.to_string();
    let mut source = e.source();
    while let Some(err) = source {
        message.push_str(" -- caused by: ");
        message.push_str(&err.to_string());
        source = err.source();
    }
    message
}

/// A request that never came back: described for the user, and written to
/// the log.
///
/// Both, always. The description reaches whoever has the right screen
/// open; the log reaches whoever is looking for why something stopped
/// arriving. Without the second half, a feed that has been dead for
/// weeks looks exactly like a feed with nothing new in it -- the only
/// trace was an error string on a settings page nobody had open.
///
/// This is the one door for it (`describe_request_error` below is
/// private): everything that talks to a foreign machine goes through
/// `builder()` for its client and through here for its failures.
pub fn request_failed(e: &reqwest::Error) -> String {
    let message = describe_request_error(e);
    // The URL is in the message -- reqwest keeps it -- so this says
    // which server, not just that one failed.
    log::warn!("request failed: {message}");
    message
}

/// For `.send()` specifically, where the connection itself can fail --
/// unlike reading an already-received response.
///
/// `reqwest`'s classification methods only work on the typed error, not
/// the string `describe_error` collapses it to, so this runs first. It
/// prefixes what the failure means -- wrong URL, no internet, server
/// down -- since "caused by: dns error" is accurate but not actionable.
fn describe_request_error(e: &reqwest::Error) -> String {
    // Localised, and `nobo_sync::orchestrator::is_recoverable_sync_error`
    // matches on these words to decide whether to retry -- so it knows
    // both languages. A classifier keyed on prose is fragile either way;
    // it is called out there rather than left to be discovered.
    let prefix = if e.is_timeout() {
        nobo_core::t!("Zeitüberschreitung -- der Server antwortet nicht", "Timed out -- the server is not responding")
    } else if e.is_connect() {
        nobo_core::t!("Server nicht erreichbar -- URL und Internetverbindung prüfen", "Server unreachable -- check the URL and your connection")
    } else {
        nobo_core::t!("Netzwerkfehler", "Network error")
    };
    format!("{prefix} ({})", describe_error(e))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A chain of causes, the way reqwest hands one over: the outer error
    /// says nothing, everything useful is one or two `source()` calls
    /// down.
    #[derive(Debug)]
    struct Layered(&'static str, Option<Box<Layered>>);

    impl std::fmt::Display for Layered {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "{}", self.0)
        }
    }

    impl std::error::Error for Layered {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            self.1.as_deref().map(|inner| inner as &(dyn std::error::Error + 'static))
        }
    }

    /// The whole reason this function exists: `Display` on a reqwest error
    /// says "error sending request for url" whether the name did not
    /// resolve, the certificate was refused or the port was closed. A log
    /// line that cannot tell those apart is a log line nobody can act on.
    #[test]
    fn a_description_carries_every_cause_not_just_the_wrapper() {
        let error = Layered(
            "error sending request",
            Some(Box::new(Layered("dns error", Some(Box::new(Layered("no such host", None)))))),
        );

        assert_eq!(
            describe_error(&error),
            "error sending request -- caused by: dns error -- caused by: no such host",
        );
    }

    #[test]
    fn an_error_with_no_cause_is_left_as_it_is() {
        assert_eq!(describe_error(&Layered("kaputt", None)), "kaputt");
    }

    /// Every client in the app comes from here (enforced by
    /// tools/check-layers-rust.mjs), so what `builder()` sets is what
    /// every request gets. The connect timeout is the one that used to be
    /// missing: without it an unreachable server hangs the request
    /// forever, with no error anywhere.
    #[tokio::test]
    async fn a_client_from_the_door_gives_up_on_an_unreachable_server() {
        let client = builder().build().expect("a client");

        // Port 1 is reserved and nothing listens there, so this fails at
        // the connection -- the same shape as a server that is down.
        let error = client.get("http://127.0.0.1:1/").send().await.unwrap_err();

        assert!(error.is_connect(), "expected a connection failure, got {error}");
    }

    /// What the caller is told, per kind of failure. The prefix is the
    /// part somebody can act on; the chain behind it is for the log.
    #[tokio::test]
    async fn a_failure_says_what_kind_it_was_before_the_technical_detail() {
        let client = builder().build().expect("a client");
        let error = client.get("http://127.0.0.1:1/").send().await.unwrap_err();

        let described = request_failed(&error);

        assert!(described.starts_with("Server nicht erreichbar"), "{described}");
        assert!(described.contains("127.0.0.1:1"), "the message must name the server: {described}");
    }

    /// A whole-request timeout is the caller's decision, and the two
    /// answers this app has are these. Pinned because they are the
    /// difference between a spinner that ends and one that does not: a
    /// login screen may not wait as long as a calendar feed.
    #[test]
    fn the_two_request_timeouts_are_what_they_say_they_are() {
        assert_eq!(ONE_FETCH, Duration::from_secs(60));
        assert_eq!(SOMEBODY_IS_WAITING, Duration::from_secs(30));
        assert!(SOMEBODY_IS_WAITING < ONE_FETCH, "somebody watching a screen waits less than a background fetch");
        assert!(REACHING_THE_SERVER < SOMEBODY_IS_WAITING, "opening a socket is not where the time goes");
    }
}
