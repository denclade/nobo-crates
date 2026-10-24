// SPDX-FileCopyrightText: 2026 Dennis Burgermeister
// SPDX-License-Identifier: GPL-3.0-or-later

//! Registration/login against PocketBase's built-in `users` auth
//! collection. Deliberately never sends the user's actual vault password
//! anywhere -- only `derive_auth_secret`'s output (see crypto.rs) ever
//! crosses the network, both here and in every other call site.

use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone)]
pub struct AuthSession {
    pub token: String,
    pub user_id: String,
    pub verified: bool,
}

#[derive(Serialize)]
struct RegisterBody<'a> {
    email: &'a str,
    password: &'a str,
    #[serde(rename = "passwordConfirm")]
    password_confirm: &'a str,
}

#[derive(Serialize)]
struct AuthBody<'a> {
    identity: &'a str,
    password: &'a str,
}

#[derive(Deserialize)]
struct AuthResponse {
    token: String,
    record: AuthRecord,
}

#[derive(Deserialize)]
struct AuthRecord {
    id: String,
    verified: bool,
}

#[derive(Deserialize)]
struct ApiError {
    message: String,
}

async fn error_message(resp: reqwest::Response) -> String {
    let status = resp.status();
    match resp.json::<ApiError>().await {
        Ok(err) => format!("{} ({status})", err.message),
        Err(_) => format!("request failed ({status})"),
    }
}

/// Creates the account. `auth_secret` is what `derive_auth_secret` produced
/// locally from the vault password -- PocketBase then bcrypt-hashes THAT
/// server-side, same as it would any other password, but it's not the
/// vault password or master key, so a compromised server never learns
/// either. Does not verify the email -- call `request_verification`
/// immediately after (see below).
pub async fn register(client: &Client, base_url: &str, email: &str, auth_secret: &str) -> Result<(), String> {
    let resp = client
        .post(format!("{base_url}/api/collections/users/records"))
        .json(&RegisterBody { email, password: auth_secret, password_confirm: auth_secret })
        .send()
        .await
        .map_err(|e| nobo_http::request_failed(&e))?;

    if resp.status().is_success() {
        Ok(())
    } else {
        Err(error_message(resp).await)
    }
}

/// Triggers PocketBase's own verification email -- the click-through link
/// it contains is handled by PocketBase itself (or a custom page you point
/// it at), not by nobo; the app only needs to know the *result*
/// (`AuthSession.verified`), checked again after the user comes back and
/// logs in.
pub async fn request_verification(client: &Client, base_url: &str, email: &str) -> Result<(), String> {
    #[derive(Serialize)]
    struct Body<'a> {
        email: &'a str,
    }

    let resp = client
        .post(format!("{base_url}/api/collections/users/request-verification"))
        .json(&Body { email })
        .send()
        .await
        .map_err(|e| nobo_http::request_failed(&e))?;

    if resp.status().is_success() {
        Ok(())
    } else {
        Err(error_message(resp).await)
    }
}

pub async fn login(client: &Client, base_url: &str, email: &str, auth_secret: &str) -> Result<AuthSession, String> {
    let resp = client
        .post(format!("{base_url}/api/collections/users/auth-with-password"))
        .json(&AuthBody { identity: email, password: auth_secret })
        .send()
        .await
        .map_err(|e| nobo_http::request_failed(&e))?;

    if resp.status() == StatusCode::TOO_MANY_REQUESTS {
        return Err("rate-limited: too many login attempts, try again later".to_string());
    }
    // PocketBase returns this exact generic message whenever the users
    // collection's own auth rule rejects the attempt -- almost always
    // "Require email verification" (see docs/sync.md's setup guide) plus
    // an account that hasn't clicked its verification email yet, since
    // that's the one auth rule this setup actually asks the admin to
    // enable. Wrong credentials get a distinct 400 from PocketBase, not
    // this 403, so it's worth naming the likely cause instead of just
    // relaying PocketBase's own wording, which gives no hint what to do
    // next.
    if resp.status() == StatusCode::FORBIDDEN {
        return Err(
            nobo_core::t!("Anmeldung nicht möglich: Das Konto ist wahrscheinlich noch nicht per E-Mail bestätigt (falls „Require email verification“ auf dem Server aktiviert ist). Bestätigungs-Mail prüfen (auch Spam-Ordner) und den Link darin öffnen; danach erneut versuchen.", "Cannot sign in: the account has probably not been confirmed by email yet (if \"Require email verification\" is enabled on the server). Check your inbox and spam folder, open the link, then try again."),
        );
    }
    // PocketBase's own message here ("Failed to authenticate.") is
    // already reasonably clear, but showing it bare with just an HTTP
    // status code reads as a technical/internal error rather than "you
    // typed something wrong" -- worth being explicit and direct instead.
    if resp.status() == StatusCode::BAD_REQUEST {
        return Err(nobo_core::t!(
            "Anmeldung fehlgeschlagen: E-Mail oder Passwort ist falsch.",
            "Sign-in failed: wrong email or password."
        ));
    }
    if !resp.status().is_success() {
        return Err(error_message(resp).await);
    }

    let parsed: AuthResponse = resp.json().await.map_err(|e| format!("unexpected login response: {e}"))?;
    Ok(AuthSession { token: parsed.token, user_id: parsed.record.id, verified: parsed.record.verified })
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn register_sends_the_auth_secret_not_a_plain_password_field_by_name() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/collections/users/records"))
            .and(body_json(serde_json::json!({
                "email": "alice@example.com",
                "password": "deadbeef",
                "passwordConfirm": "deadbeef"
            })))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        register(&Client::new(), &server.uri(), "alice@example.com", "deadbeef").await.unwrap();
    }

    #[tokio::test]
    async fn register_surfaces_the_server_error_message() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/collections/users/records"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({"message": "email already in use"})))
            .mount(&server)
            .await;

        let err = register(&Client::new(), &server.uri(), "alice@example.com", "deadbeef").await.unwrap_err();
        assert!(err.contains("email already in use"), "got: {err}");
    }

    #[tokio::test]
    async fn login_parses_the_token_and_verified_flag() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/collections/users/auth-with-password"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "token": "eyJ.fake.token",
                "record": { "id": "user123", "collectionName": "users", "verified": false }
            })))
            .mount(&server)
            .await;

        let session = login(&Client::new(), &server.uri(), "alice@example.com", "deadbeef").await.unwrap();
        assert_eq!(session.token, "eyJ.fake.token");
        assert_eq!(session.user_id, "user123");
        assert!(!session.verified);
    }

    #[tokio::test]
    async fn login_rate_limit_is_reported_distinctly() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/collections/users/auth-with-password"))
            .respond_with(ResponseTemplate::new(429))
            .mount(&server)
            .await;

        let err = login(&Client::new(), &server.uri(), "alice@example.com", "deadbeef").await.unwrap_err();
        assert!(err.contains("rate-limited"), "got: {err}");
    }

    /// PocketBase's own 403 message for this ("The request doesn't satisfy
    /// the collection requirements to authenticate.") gives no hint what to
    /// actually do -- this is what a real user hit after following
    /// docs/sync.md's own "Require email verification" setup step and
    /// trying to log in before clicking the verification link.
    #[tokio::test]
    async fn login_forbidden_names_the_likely_unverified_account_cause() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/collections/users/auth-with-password"))
            .respond_with(ResponseTemplate::new(403).set_body_json(serde_json::json!({
                "message": "The request doesn't satisfy the collection requirements to authenticate."
            })))
            .mount(&server)
            .await;

        let err = login(&Client::new(), &server.uri(), "alice@example.com", "deadbeef").await.unwrap_err();
        assert!(err.contains("bestätigt"), "got: {err}");
    }
}
