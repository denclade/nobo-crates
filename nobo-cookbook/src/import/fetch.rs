//! The page and its picture, over HTTP.
//!
//! The only part of the import that talks to anything (see the module
//! doc), and therefore the only part behind the `import` feature.

use nobo_core::t;

/// Who is asking. Some recipe sites turn away a request that names no
/// client at all; this one says what it is rather than posing as a
/// browser.
const USER_AGENT: &str = "Mozilla/5.0 (compatible; nobo recipe import)";

/// A recipe photo is a few hundred kilobytes; anything past this is not
/// one, or not one worth a phone's storage.
const IMAGE_LIMIT: usize = 15 * 1024 * 1024;

fn client() -> Result<reqwest::Client, String> {
    nobo_http::builder()
        .timeout(nobo_http::ONE_FETCH)
        .user_agent(USER_AGENT)
        .build()
        .map_err(|e| nobo_http::describe_error(&e))
}

/// The page at URL, as text.
pub async fn page(url: &str) -> Result<String, String> {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err(t!("Das ist keine Webadresse: {url}", "That is not a web address: {url}"));
    }
    let response = client()?.get(url).send().await.map_err(|e| nobo_http::request_failed(&e))?;
    let status = response.status();
    if !status.is_success() {
        return Err(t!("Die Seite antwortete mit {status}", "The page answered {status}"));
    }
    response.text().await.map_err(|e| nobo_http::request_failed(&e))
}

/// The picture at URL, with the file extension its type calls for.
///
/// Asks for a JPEG or PNG first: image CDNs pick the format from the
/// request (zuckerjagdwurst's says `format=auto`), and what a phone
/// displays everywhere is those two, not AVIF.
pub async fn image(url: &str) -> Result<(Vec<u8>, &'static str), String> {
    let response = client()?
        .get(url)
        .header("Accept", "image/jpeg,image/png;q=0.9,image/webp;q=0.5,image/*;q=0.1")
        .send()
        .await
        .map_err(|e| nobo_http::request_failed(&e))?;
    let status = response.status();
    if !status.is_success() {
        return Err(t!("Das Bild antwortete mit {status}", "The picture answered {status}"));
    }
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    let extension = match content_type.split(';').next().unwrap_or("").trim() {
        "image/jpeg" | "image/jpg" => "jpg",
        "image/png" => "png",
        "image/webp" => "webp",
        "image/gif" => "gif",
        other => return Err(t!("Kein Bild, das sich anzeigen ließe ({other})", "Not a picture that can be shown ({other})")),
    };
    let bytes = response.bytes().await.map_err(|e| nobo_http::request_failed(&e))?;
    if bytes.len() > IMAGE_LIMIT {
        return Err(t!("Das Bild ist zu groß", "The picture is too large"));
    }
    Ok((bytes.to_vec(), extension))
}
