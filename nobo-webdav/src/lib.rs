// SPDX-FileCopyrightText: 2026 Dennis Burgermeister
// SPDX-License-Identifier: GPL-3.0-or-later

//! WebDAV plumbing shared by the vault sync backend (nobo_sync::providers)
//! and the CalDAV task sync (nobo_caldav).
//!
//! Both speak the same protocol at this level: HTTP Basic auth, methods
//! HTTP itself does not define, and `207 Multi-Status` XML bodies. Only
//! the properties they ask for differ.
//!
//! Two of them is exactly why this is a crate of its own and not part of
//! either: one would own what the other depends on. It is not the core's
//! either -- the core never speaks a protocol (see `nobo-http` for the
//! same reasoning about the client itself).

use quick_xml::events::Event;
use quick_xml::reader::Reader;
use std::collections::HashMap;

pub fn basic_auth_header(username: &str, password: &str) -> String {
    use base64ct::{Base64, Encoding};
    let raw = format!("{username}:{password}");
    format!("Basic {}", Base64::encode_string(raw.as_bytes()))
}

/// PROPFIND, REPORT, MKCOL -- none of which `reqwest::Method` has a
/// constant for. The names are fixed ASCII, so this cannot fail in
/// practice; an unparseable one would be a typo in a literal.
pub fn method(name: &str) -> reqwest::Method {
    reqwest::Method::from_bytes(name.as_bytes()).expect("WebDAV method name must be valid ASCII")
}

/// One `<response>` of a `207 Multi-Status` body.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MultiStatusEntry {
    /// The resource's path, unescaped.
    pub href: String,
    /// Text-valued properties, keyed by local name without namespace
    /// prefix -- servers disagree about the prefix (`D:`, `d:`, none) but
    /// never about the local name.
    pub props: HashMap<String, String>,
    /// Child ELEMENT names of a property, for the ones whose value is
    /// structure rather than text: `resourcetype` -> ["collection",
    /// "calendar"].
    pub child_elements: HashMap<String, Vec<String>>,
    /// `name` attributes of a property's children:
    /// `supported-calendar-component-set` -> ["VEVENT", "VTODO"].
    pub child_names: HashMap<String, Vec<String>>,
    /// `<href>` values found INSIDE a property, which is how DAV points
    /// from one resource to another: `current-user-principal` ->
    /// ["/principals/dennis/"]. Kept apart from the response's own `href`
    /// above, which they would otherwise overwrite -- they answer a
    /// different question ("where is the thing I asked about" vs. "what
    /// resource is this").
    pub prop_hrefs: HashMap<String, Vec<String>>,
}

impl MultiStatusEntry {
    pub fn prop(&self, name: &str) -> Option<&str> {
        self.props.get(name).map(String::as_str)
    }

    /// The first href a property points at, if any.
    pub fn prop_href(&self, prop: &str) -> Option<&str> {
        self.prop_hrefs.get(prop)?.first().map(String::as_str)
    }

    pub fn has_child_element(&self, prop: &str, child: &str) -> bool {
        self.child_elements.get(prop).is_some_and(|children| children.iter().any(|c| c == child))
    }

    /// Whether this collection holds components of the given type.
    ///
    /// RFC 4791 makes the property OPTIONAL, and absence means the
    /// collection supports EVERY type -- not none. Requiring it explicitly
    /// silently hides every list on a server that leaves it out.
    pub fn supports_component(&self, component: &str) -> bool {
        match self.child_names.get("supported-calendar-component-set") {
            Some(names) => names.iter().any(|n| n.eq_ignore_ascii_case(component)),
            None => true,
        }
    }
}

/// Parses a `207 Multi-Status` body into one entry per `<response>`.
///
/// One parser for every property rather than one function per property:
/// the two callers ask for six different properties between them, and the
/// element-name-and-nesting handling is identical for all of them.
pub fn parse_multistatus(xml: &str) -> Result<Vec<MultiStatusEntry>, String> {
    let mut reader = Reader::from_str(xml);
    // A truncated body must be an error, not an empty list: silently
    // reporting "this list has no tasks" would let the sync delete them
    // all locally.
    reader.config_mut().check_end_names = true;
    let mut buf = Vec::new();
    let mut entries: Vec<MultiStatusEntry> = Vec::new();

    let mut current: Option<MultiStatusEntry> = None;
    let mut in_href = false;
    // Element names below `<prop>`, outermost first. A stack rather than a
    // depth counter because self-closing elements (`<collection/>`) raise
    // no End event -- counting would leave the nesting permanently off by
    // one from the first `<resourcetype>` onwards.
    let mut prop_stack: Vec<String> = Vec::new();
    let mut in_prop = false;
    // Accumulated, not assigned: an XML entity splits a value across
    // several Text events ("Haus ", "&", " Garten"), and taking the last
    // one would silently truncate every value containing an entity.
    let mut text = String::new();

    loop {
        match reader.read_event_into(&mut buf) {
            // Start and Empty carry the same information; only Empty ends
            // immediately, which is why the stack is popped right back.
            Ok(Event::Start(e)) => {
                if let Some(name) = open_element(&e, &mut current, &mut in_href, &mut in_prop, &prop_stack, false) {
                    prop_stack.push(name);
                }
            }
            // Self-closing: nothing nests inside it, and no End event will
            // arrive to undo anything it set -- so it must not latch any
            // state. `<d:prop/>` on a server with nothing to report used to
            // leave the parser inside a property for the rest of the body.
            Ok(Event::Empty(e)) => {
                open_element(&e, &mut current, &mut in_href, &mut in_prop, &prop_stack, true);
            }
            Ok(Event::End(e)) => {
                let name = local_name(e.local_name().as_ref());
                match name.as_str() {
                    "response" if !in_prop => {
                        if let Some(entry) = current.take() {
                            entries.push(entry);
                        }
                        text.clear();
                    }
                    "href" => {
                        let value = std::mem::take(&mut text);
                        // Pushed by open_element when it nests in a
                        // property; the property itself is then below it.
                        if prop_stack.last().is_some_and(|last| last == "href") {
                            prop_stack.pop();
                        }
                        if let Some(entry) = current.as_mut() {
                            match prop_stack.first() {
                                // Inside a property: it points somewhere
                                // else and must not replace the response's
                                // own href.
                                Some(prop) => entry.prop_hrefs.entry(prop.clone()).or_default().push(value),
                                None => entry.href = value,
                            }
                        }
                        in_href = false;
                    }
                    "prop" if prop_stack.is_empty() => in_prop = false,
                    _ if in_prop => {
                        // Only a top-level property carries a text value;
                        // deeper elements are structure.
                        if prop_stack.len() == 1 {
                            let value = std::mem::take(&mut text);
                            if !value.is_empty() {
                                if let Some(entry) = current.as_mut() {
                                    entry.props.insert(prop_stack[0].clone(), value);
                                }
                            }
                        }
                        prop_stack.pop();
                    }
                    _ => {}
                }
            }
            Ok(Event::Text(t)) => {
                if !in_href && prop_stack.len() != 1 {
                    buf.clear();
                    continue;
                }
                let decoded = t.decode().map_err(|e| format!("failed to parse WebDAV response: {e}"))?;
                let unescaped = quick_xml::escape::unescape(&decoded).map_err(|e| format!("failed to parse WebDAV response: {e}"))?;
                // Trimmed only at the ends of the whole value, not per
                // fragment: "Haus " + "& " + "Garten" must keep its spaces.
                if text.is_empty() {
                    text.push_str(unescaped.trim_start());
                } else {
                    text.push_str(&unescaped);
                }
            }
            // quick-xml reports an entity as its own event rather than
            // inside the surrounding Text, so ignoring it drops the
            // character entirely ("Haus & Garten" -> "Haus  Garten").
            Ok(Event::GeneralRef(r)) => {
                if in_href || prop_stack.len() == 1 {
                    let raw = String::from_utf8_lossy(&r).to_string();
                    let resolved = match r.resolve_char_ref() {
                        Ok(Some(c)) => c.to_string(),
                        _ => match raw.as_str() {
                            "amp" => "&".to_string(),
                            "lt" => "<".to_string(),
                            "gt" => ">".to_string(),
                            "quot" => "\"".to_string(),
                            "apos" => "'".to_string(),
                            // An entity nobody defined: kept verbatim
                            // rather than silently swallowed.
                            other => format!("&{other};"),
                        },
                    };
                    text.push_str(&resolved);
                }
            }
            Ok(Event::Eof) => {
                // A truncated body must not read as "this list is empty":
                // the sync would take that as every task having been
                // deleted remotely.
                if current.is_some() || in_prop {
                    return Err("failed to parse WebDAV response: unexpected end of document".to_string());
                }
                break;
            }
            Err(e) => return Err(format!("failed to parse WebDAV response: {e}")),
            _ => {}
        }
        buf.clear();
    }

    Ok(entries)
}

/// Escapes text for an XML element body.
///
/// Hrefs are the only user-controlled text this crate puts into a request,
/// and a `&` in one is enough to make the whole document unparseable.
pub fn escape_text(value: &str) -> String {
    value.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// Handles one opening element. Returns the local name when the element
/// is a property (or a child of one) that the caller must push onto the
/// nesting stack; `None` for the structural elements handled here.
fn open_element(
    element: &quick_xml::events::BytesStart,
    current: &mut Option<MultiStatusEntry>,
    in_href: &mut bool,
    in_prop: &mut bool,
    prop_stack: &[String],
    self_closing: bool,
) -> Option<String> {
    let name = local_name(element.local_name().as_ref());
    match name.as_str() {
        "response" if !*in_prop => {
            if !self_closing {
                *current = Some(MultiStatusEntry::default());
            }
            None
        }
        // Inside a property too (`current-user-principal` and friends
        // point at another resource that way) -- the End handler tells the
        // two apart by the property stack, and the name is pushed onto it
        // so the text below is read as this element's, not the
        // property's own value.
        "href" if current.is_some() => {
            *in_href = !self_closing;
            (*in_prop && !self_closing).then_some(name)
        }
        "prop" if current.is_some() => {
            *in_prop = !self_closing;
            None
        }
        _ if *in_prop => {
            if let (Some(entry), Some(parent)) = (current.as_mut(), prop_stack.first()) {
                entry.child_elements.entry(parent.clone()).or_default().push(name.clone());
                if let Some(attr) = attribute(element, b"name") {
                    entry.child_names.entry(parent.clone()).or_default().push(attr);
                }
            }
            Some(name)
        }
        _ => None,
    }
}

fn local_name(raw: &[u8]) -> String {
    String::from_utf8_lossy(raw).to_ascii_lowercase()
}

fn attribute(element: &quick_xml::events::BytesStart, name: &[u8]) -> Option<String> {
    element.attributes().flatten().find(|a| a.key.local_name().as_ref() == name).map(|a| String::from_utf8_lossy(&a.value).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A Nextcloud-shaped PROPFIND answer: two task lists and the
    /// collection itself, with namespace prefixes servers actually use.
    const CALENDAR_HOME: &str = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:cs="http://calendarserver.org/ns/" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:response>
    <d:href>/remote.php/dav/calendars/dennis/</d:href>
    <d:propstat><d:prop>
      <d:resourcetype><d:collection/></d:resourcetype>
    </d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
  </d:response>
  <d:response>
    <d:href>/remote.php/dav/calendars/dennis/einkauf/</d:href>
    <d:propstat><d:prop>
      <d:displayname>Einkauf</d:displayname>
      <d:resourcetype><d:collection/><c:calendar/></d:resourcetype>
      <c:supported-calendar-component-set><c:comp name="VTODO"/></c:supported-calendar-component-set>
      <cs:getctag>http://sabre.io/ns/sync/42</cs:getctag>
    </d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
  </d:response>
  <d:response>
    <d:href>/remote.php/dav/calendars/dennis/termine/</d:href>
    <d:propstat><d:prop>
      <d:displayname>Termine</d:displayname>
      <d:resourcetype><d:collection/><c:calendar/></d:resourcetype>
      <c:supported-calendar-component-set><c:comp name="VEVENT"/></c:supported-calendar-component-set>
    </d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
  </d:response>
</d:multistatus>"#;

    #[test]
    fn every_response_becomes_one_entry() {
        let entries = parse_multistatus(CALENDAR_HOME).unwrap();

        assert_eq!(entries.len(), 3);
        assert_eq!(entries[1].href, "/remote.php/dav/calendars/dennis/einkauf/");
        assert_eq!(entries[1].prop("displayname"), Some("Einkauf"));
        assert_eq!(entries[1].prop("getctag"), Some("http://sabre.io/ns/sync/42"));
    }

    /// The whole reason for a stack rather than a depth counter:
    /// `<d:collection/>` raises no End event, and counting would leave the
    /// nesting off by one for every response after the first.
    #[test]
    fn self_closing_children_do_not_desynchronize_the_nesting() {
        let entries = parse_multistatus(CALENDAR_HOME).unwrap();

        // Would fail if the stack were left unbalanced by the first entry's
        // <resourcetype><d:collection/></resourcetype>.
        assert_eq!(entries[2].prop("displayname"), Some("Termine"));
        assert!(entries[2].has_child_element("resourcetype", "calendar"));
    }

    #[test]
    fn structure_valued_properties_are_read_as_structure_not_text() {
        let entries = parse_multistatus(CALENDAR_HOME).unwrap();

        assert!(entries[0].has_child_element("resourcetype", "collection"));
        assert!(!entries[0].has_child_element("resourcetype", "calendar"));
        assert!(entries[1].has_child_element("resourcetype", "calendar"));
    }

    /// Which lists hold tasks at all -- a calendar of appointments has no
    /// business showing up as a task list to subscribe to.
    #[test]
    fn a_component_set_says_which_collections_hold_tasks() {
        let entries = parse_multistatus(CALENDAR_HOME).unwrap();

        assert!(entries[1].supports_component("VTODO"));
        assert!(!entries[2].supports_component("VTODO"));
        assert!(entries[2].supports_component("VEVENT"));
    }

    /// RFC 4791 makes the property optional, and absent means "every
    /// type", not "none" -- reading it the other way hides every list on a
    /// server that leaves it out.
    #[test]
    fn a_collection_that_does_not_say_what_it_holds_is_assumed_to_hold_everything() {
        let xml = r#"<d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
          <d:response><d:href>/cal/x/</d:href><d:propstat><d:prop>
            <d:displayname>Ohne Angabe</d:displayname>
            <d:resourcetype><d:collection/><c:calendar/></d:resourcetype>
          </d:prop></d:propstat></d:response></d:multistatus>"#;

        let entries = parse_multistatus(xml).unwrap();

        assert!(entries[0].supports_component("VTODO"));
    }

    /// Servers disagree about the prefix (`D:`, `d:`, none) and never
    /// about the local name -- matching raw tags would silently miss
    /// every server that picked a different one than the test did.
    #[test]
    fn namespace_prefixes_are_ignored() {
        let xml = r#"<multistatus xmlns="DAV:"><response><href>/a</href>
          <propstat><prop><getetag>"v1"</getetag></prop></propstat></response></multistatus>"#;

        let entries = parse_multistatus(xml).unwrap();

        assert_eq!(entries[0].prop("getetag"), Some("\"v1\""));
    }

    #[test]
    fn xml_entities_in_a_value_are_resolved() {
        let xml = r#"<d:multistatus xmlns:d="DAV:"><d:response><d:href>/a%20b</d:href>
          <d:propstat><d:prop><d:displayname>Haus &amp; Garten</d:displayname></d:prop></d:propstat>
        </d:response></d:multistatus>"#;

        let entries = parse_multistatus(xml).unwrap();

        assert_eq!(entries[0].prop("displayname"), Some("Haus & Garten"));
    }

    /// A server with nothing to report answers `<d:prop/>`. No End event
    /// follows it, so latching "we are inside a property" there left the
    /// parser lost for the rest of the body -- and the whole response
    /// unreadable.
    #[test]
    fn a_self_closing_prop_does_not_strand_the_parser() {
        let xml = r#"<d:multistatus xmlns:d="DAV:">
          <d:response><d:href>/a/</d:href><d:propstat><d:prop/></d:propstat></d:response>
          <d:response><d:href>/a/x.ics</d:href><d:propstat><d:prop><d:getetag>"v1"</d:getetag></d:prop></d:propstat></d:response>
        </d:multistatus>"#;

        let entries = parse_multistatus(xml).unwrap();

        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].prop("getetag"), None);
        assert_eq!(entries[1].prop("getetag"), Some("\"v1\""));
    }

    #[test]
    fn an_empty_multistatus_is_not_an_error() {
        assert!(parse_multistatus(r#"<d:multistatus xmlns:d="DAV:"/>"#).unwrap().is_empty());
    }

    #[test]
    fn malformed_xml_is_an_error_not_a_silent_empty_list() {
        assert!(parse_multistatus("<d:multistatus><d:response>").is_err());
    }

    #[test]
    fn basic_auth_is_the_standard_base64_of_user_and_password() {
        assert_eq!(basic_auth_header("aladdin", "opensesame"), "Basic YWxhZGRpbjpvcGVuc2VzYW1l");
    }
}
