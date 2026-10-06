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

/// A recipe page, scripts and inline styles included, is well under a
/// few megabytes. Past this it is not a page anyone meant to send -- and
/// reading on would hold all of it in memory before `parse_page` ever saw
/// a byte.
const PAGE_LIMIT: usize = 10 * 1024 * 1024;

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
    let body = body_within(response, PAGE_LIMIT)
        .await?
        .ok_or_else(|| t!("Die Seite ist zu groß für ein Rezept", "The page is too large for a recipe"))?;
    // What `Response::text` does without reqwest's `charset` feature, which
    // this workspace does not enable: UTF-8, with anything else replaced.
    Ok(String::from_utf8_lossy(&body).into_owned())
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
    let bytes = body_within(response, IMAGE_LIMIT)
        .await?
        .ok_or_else(|| t!("Das Bild ist zu groß", "The picture is too large"))?;
    Ok((bytes, extension))
}

/// The body, or `None` as soon as it is known to be longer than LIMIT --
/// from the announced length before anything is read, or while reading,
/// for a server that announces none (or the wrong one). The size was
/// checked only once the whole body was in memory, which for a link to
/// something huge is the very thing a limit is there to prevent.
async fn body_within(mut response: reqwest::Response, limit: usize) -> Result<Option<Vec<u8>>, String> {
    if response.content_length().is_some_and(|announced| announced > limit as u64) {
        return Ok(None);
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|e| nobo_http::request_failed(&e))? {
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

#[cfg(test)]
mod tests {
    use super::append_within;

    #[test]
    fn a_body_stops_at_its_limit_not_after_it() {
        let mut body = Vec::new();
        assert!(append_within(&mut body, b"12345", 8));
        assert!(append_within(&mut body, b"678", 8), "exactly the limit is still fine");
        assert!(!append_within(&mut body, b"9", 8));
        assert_eq!(body, b"12345678", "nothing past the limit is kept");
    }
}
