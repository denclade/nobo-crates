//! Fetching one web page or one picture for a person who is waiting --
//! the recipe import, a bookmark's preview.
//!
//! What both need and neither should write twice: a size limit that holds
//! before the body is in memory, the page decoded in the charset it was
//! written in, and a picture only when it is one a phone can show.

use crate::{builder, describe_error, request_failed, ONE_FETCH};
use nobo_core::t;

/// A page, scripts and inline styles included, is well under a few
/// megabytes. Past this it is not a page anyone meant to send.
pub const PAGE_LIMIT: usize = 10 * 1024 * 1024;

/// A photo is a few hundred kilobytes; anything past this is not one, or
/// not one worth a phone's storage.
pub const IMAGE_LIMIT: usize = 15 * 1024 * 1024;

fn client(user_agent: &str) -> Result<reqwest::Client, String> {
    builder().timeout(ONE_FETCH).user_agent(user_agent).build().map_err(|e| describe_error(&e))
}

/// The page at URL as text, decoded in its own charset. USER_AGENT says
/// who is asking: some sites turn away a request that names no client,
/// and this says what it is rather than posing as a browser.
pub async fn page(url: &str, user_agent: &str) -> Result<String, String> {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err(t!("Das ist keine Webadresse: {url}", "That is not a web address: {url}"));
    }
    let response = client(user_agent)?.get(url).send().await.map_err(|e| request_failed(&e))?;
    let status = response.status();
    if !status.is_success() {
        return Err(t!("Die Seite antwortete mit {status}", "The page answered {status}"));
    }
    let content_type = header(&response, "content-type");
    let body = body_within(response, PAGE_LIMIT).await?.ok_or_else(|| t!("Die Seite ist zu groß", "The page is too large"))?;
    Ok(decode_page(&body, content_type.as_deref()))
}

/// The picture at URL, with the file extension its type calls for.
///
/// Asks for a JPEG or PNG first: image CDNs pick the format from the
/// request, and what a phone displays everywhere is those two, not AVIF.
pub async fn image(url: &str, user_agent: &str) -> Result<(Vec<u8>, &'static str), String> {
    let response = client(user_agent)?
        .get(url)
        .header("Accept", "image/jpeg,image/png;q=0.9,image/webp;q=0.5,image/*;q=0.1")
        .send()
        .await
        .map_err(|e| request_failed(&e))?;
    let status = response.status();
    if !status.is_success() {
        return Err(t!("Das Bild antwortete mit {status}", "The picture answered {status}"));
    }
    let content_type = header(&response, "content-type").unwrap_or_default().to_ascii_lowercase();
    let extension = match content_type.split(';').next().unwrap_or("").trim() {
        "image/jpeg" | "image/jpg" => "jpg",
        "image/png" => "png",
        "image/webp" => "webp",
        "image/gif" => "gif",
        other => return Err(t!("Kein Bild, das sich anzeigen ließe ({other})", "Not a picture that can be shown ({other})")),
    };
    let bytes = body_within(response, IMAGE_LIMIT).await?.ok_or_else(|| t!("Das Bild ist zu groß", "The picture is too large"))?;
    Ok((bytes, extension))
}

fn header(response: &reqwest::Response, name: &str) -> Option<String> {
    response.headers().get(name).and_then(|value| value.to_str().ok()).map(str::to_string)
}

/// The body, or `None` as soon as it is known to be longer than LIMIT --
/// from the announced length before anything is read, or while reading,
/// for a server that announces none (or the wrong one).
async fn body_within(mut response: reqwest::Response, limit: usize) -> Result<Option<Vec<u8>>, String> {
    if response.content_length().is_some_and(|announced| announced > limit as u64) {
        return Ok(None);
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|e| request_failed(&e))? {
        if !append_within(&mut body, &chunk, limit) {
            return Ok(None);
        }
    }
    Ok(Some(body))
}

/// Adds CHUNK to BODY unless that would take it past LIMIT.
fn append_within(body: &mut Vec<u8>, chunk: &[u8], limit: usize) -> bool {
    if body.len() + chunk.len() > limit {
        return false;
    }
    body.extend_from_slice(chunk);
    true
}

/// BODY as text, in the charset the page says it is in: the
/// `Content-Type` header, else a `<meta charset>` (or its `http-equiv`
/// spelling) near the top, else UTF-8. A byte-order mark wins over all of
/// them, as in a browser.
///
/// Reading every page as UTF-8 turned a Latin-1 page's "Müller" into
/// "M\u{fffd}ller" in a recipe title or a bookmark's.
pub fn decode_page(body: &[u8], content_type: Option<&str>) -> String {
    let declared = content_type.and_then(charset_param).or_else(|| meta_charset(&body[..body.len().min(4096)]));
    let encoding = declared.and_then(|label| encoding_rs::Encoding::for_label(label.trim().as_bytes())).unwrap_or(encoding_rs::UTF_8);
    let (text, _, _) = encoding.decode(body);
    text.into_owned()
}

/// `text/html; charset=ISO-8859-1` -> `ISO-8859-1`.
fn charset_param(content_type: &str) -> Option<String> {
    content_type.split(';').skip(1).find_map(|part| {
        let (key, value) = part.split_once('=')?;
        key.trim().eq_ignore_ascii_case("charset").then(|| value.trim().trim_matches(['"', '\'']).to_string())
    })
}

/// The charset a page's own head declares, read as ASCII -- which every
/// charset a web page can declare agrees with for these few bytes.
fn meta_charset(head: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(head).to_ascii_lowercase();
    let at = text.find("charset=")? + "charset=".len();
    let value: String =
        text[at..].trim_start_matches(['"', '\'']).chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').collect();
    (!value.is_empty()).then_some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_body_stops_at_its_limit_not_after_it() {
        let mut body = Vec::new();
        assert!(append_within(&mut body, b"12345", 8));
        assert!(append_within(&mut body, b"678", 8), "exactly the limit is still fine");
        assert!(!append_within(&mut body, b"9", 8));
        assert_eq!(body, b"12345678", "nothing past the limit is kept");
    }

    #[test]
    fn a_page_is_read_in_the_charset_it_declares() {
        let latin1 = b"<html><head><title>M\xfcller</title></head></html>";
        assert!(decode_page(latin1, Some("text/html; charset=ISO-8859-1")).contains("Müller"));
        let meta = b"<html><head><meta charset=\"windows-1252\"><title>M\xfcller</title>";
        assert!(decode_page(meta, Some("text/html")).contains("Müller"));
        let equiv = b"<meta http-equiv=\"Content-Type\" content=\"text/html; charset=iso-8859-1\"><p>\xe4</p>";
        assert!(decode_page(equiv, None).contains('ä'));
    }

    #[test]
    fn without_a_declaration_it_is_utf8() {
        assert!(decode_page("<title>Müller</title>".as_bytes(), None).contains("Müller"));
    }
}
