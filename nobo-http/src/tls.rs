// SPDX-FileCopyrightText: 2026 Dennis Burgermeister
// SPDX-License-Identifier: GPL-3.0-or-later

//! Android-only TLS certificate verification override.
//!
//! Certificate verification normally goes through the platform verifier,
//! which on Android delegates to the OS `PKIXRevocationChecker`. That
//! checker is meant to soft-fail when revocation status is undeterminable,
//! but hard-fails with a false "Revoked" verdict when an issuer omits an
//! OCSP responder URL -- now common with short-lived certificate profiles.
//! Known upstream issue:
//! <https://github.com/rustls/rustls-platform-verifier/pull/179>
//!
//! Desktop keeps the OS-integrated verifier, which also respects
//! user-installed and corporate CAs. Android gets a pure-Rust verifier
//! against the bundled Mozilla roots, which does no live revocation
//! checking at all -- the tradeoff, and what mobile browsers largely do
//! already.
//!
//! Because this applies to every client (see `crate::builder`, the only
//! place that makes one), the platform verifier is never used on Android
//! -- which is why the JNI call that used to initialise it is gone.
//! Measured on the emulator, 2026-08-27: HTTPS against two public hosts
//! works without it. The flip side is that a client built past
//! `builder()` gets the uninitialised platform verifier and dies
//! mid-handshake with no error at all, so `tools/check-layers-rust.mjs`
//! does not allow one.

pub fn apply(builder: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
    let root_store = rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config = rustls::ClientConfig::builder().with_root_certificates(root_store).with_no_client_auth();
    // NOT `Some(config)` -- `use_preconfigured_tls` wraps its argument in
    // `Some` internally before downcasting to `Option<rustls::ClientConfig>`,
    // so passing an already-wrapped value here mismatches the downcast and
    // silently falls through to "Unknown TLS backend passed to
    // `use_preconfigured_tls`" at `.build()` time instead of using it.
    builder.use_preconfigured_tls(config)
}
