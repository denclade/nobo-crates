//! The page and its picture, over HTTP.
//!
//! The only part of the import that talks to anything (see the module
//! doc), and therefore the only part behind the `import` feature. The
//! fetching itself -- limits, charsets -- is nobo-http's (see its `fetch`
//! module), shared with the bookmarks.

/// Who is asking. Some recipe sites turn away a request that names no
/// client at all; this one says what it is rather than posing as a
/// browser.
const USER_AGENT: &str = "Mozilla/5.0 (compatible; nobo recipe import)";

/// The page at URL, as text.
pub async fn page(url: &str) -> Result<String, String> {
    nobo_http::fetch::page(url, USER_AGENT).await
}

/// The picture at URL, with the file extension its type calls for.
pub async fn image(url: &str) -> Result<(Vec<u8>, &'static str), String> {
    nobo_http::fetch::image(url, USER_AGENT).await
}
