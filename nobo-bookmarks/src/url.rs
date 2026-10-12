//! Addresses, as the bookmarks compare them.

/// Query parameters that only say where a click came from. Two links that
/// differ only in these lead to the same page.
const TRACKING: &[&str] = &["fbclid", "gclid", "dclid", "msclkid", "mc_cid", "mc_eid", "igshid", "ref_src", "_hsenc", "_hsmi"];

/// URL in the form two addresses are compared in, to find a bookmark that
/// is already there: scheme and host in lower case, `http` and `https`
/// alike, no `www.`, no default port, no
/// `#fragment`, no tracking parameters (`utm_*` and the like), and a
/// trailing `/` makes no difference. What is SAVED is always the address
/// as it was given -- this is only the key.
pub fn normalize(url: &str) -> String {
    let url = url.trim();
    let without_fragment = url.split('#').next().unwrap_or(url);
    let (scheme, rest) = match without_fragment.split_once("://") {
        Some((scheme, rest)) => (scheme.to_ascii_lowercase(), rest),
        None => return without_fragment.to_string(),
    };
    let (authority, path_and_query) = match rest.find(['/', '?']) {
        Some(at) => (&rest[..at], &rest[at..]),
        None => (rest, ""),
    };
    let mut host = authority.to_ascii_lowercase();
    for default in [":80", ":443"] {
        if (scheme == "http" && default == ":80") || (scheme == "https" && default == ":443") {
            if let Some(stripped) = host.strip_suffix(default) {
                host = stripped.to_string();
            }
        }
    }
    let (path, query) = match path_and_query.split_once('?') {
        Some((path, query)) => (path, Some(query)),
        None => (path_and_query, None),
    };
    let path = path.trim_end_matches('/');
    // The same site over http and https, with and without `www.`, is the
    // same page for any page a bookmark is kept for.
    let scheme = if scheme == "http" { "https".to_string() } else { scheme };
    let host = host.strip_prefix("www.").map(str::to_string).unwrap_or(host);
    let kept: Vec<&str> = query
        .map(|query| {
            query
                .split('&')
                .filter(|pair| {
                    let key = pair.split('=').next().unwrap_or("").to_ascii_lowercase();
                    !pair.is_empty() && !key.starts_with("utm_") && !TRACKING.contains(&key.as_str())
                })
                .collect()
        })
        .unwrap_or_default();
    let query = if kept.is_empty() { String::new() } else { format!("?{}", kept.join("&")) };
    format!("{scheme}://{host}{path}{query}")
}

/// The site: `docs.rs` for `https://docs.rs/reqwest`, without `www.`.
pub fn host(url: &str) -> String {
    let rest = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = host.rsplit_once('@').map(|(_, host)| host).unwrap_or(host);
    let host = host.split(':').next().unwrap_or(host).to_ascii_lowercase();
    host.strip_prefix("www.").map(str::to_string).unwrap_or(host)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_same_page_is_the_same_address() {
        let a = normalize("HTTPS://Docs.RS:443/reqwest/?utm_source=x&fbclid=1#top");
        assert_eq!(a, "https://docs.rs/reqwest");
        assert_eq!(normalize("https://docs.rs/reqwest"), a);
        assert_eq!(normalize("https://example.org/a?id=3&utm_medium=mail"), "https://example.org/a?id=3");
        assert_eq!(normalize("http://www.heise.de/x"), normalize("https://heise.de/x"));
    }

    #[test]
    fn a_different_page_stays_different() {
        assert_ne!(normalize("https://example.org/a?id=3"), normalize("https://example.org/a?id=4"));
        assert_ne!(normalize("https://example.org/A"), normalize("https://example.org/a"));
    }

    #[test]
    fn the_host_is_the_site() {
        assert_eq!(host("https://www.Heise.de/news/x"), "heise.de");
        assert_eq!(host("http://user:pw@10.0.0.5:8080/x"), "10.0.0.5");
    }
}
