// SPDX-FileCopyrightText: 2026 Dennis Burgermeister
// SPDX-License-Identifier: GPL-3.0-or-later

use std::collections::HashMap;

use crate::core::models::Heading;

/// Org-mode's standard property for a stable, export/link-anchor-safe
/// heading identifier -- see <https://orgmode.org/manual/Internal-Links.html>.
/// Deliberately not derived from the title: a slug looks tidy but goes stale
/// the moment the heading is renamed or refiled, which is exactly the case
/// this identity is meant to survive. Generated as a plain UUID instead --
/// the same value shape org-id.el itself uses for its own `ID:` property
/// (`org-id-uuid`/`org-id-method` default to 'uuid) -- just stored under
/// CUSTOM_ID rather than ID, since this app has no use for org-id.el's
/// separate link-database machinery, only a stable anchor.
pub const CUSTOM_ID_PROPERTY: &str = "CUSTOM_ID";

/// How a heading should be relocated later -- CUSTOM_ID when the heading
/// had one at capture time, otherwise its title (the same matching this
/// codebase always did before CUSTOM_ID existed). This is the ONLY place
/// that decides between the two; every caller that needs to re-find a
/// heading across a time gap (history restore's delete/update revert cases)
/// goes through `capture_identity`/`resolve` here rather than re-deciding
/// the fallback itself.
pub enum HeadingIdentity {
    CustomId(String),
    Title(String),
}

/// The CUSTOM_ID in PROPERTIES, whatever case its key was written in --
/// see models::property_value, which every property read here goes
/// through.
fn get_custom_id(properties: &HashMap<String, String>) -> Option<&str> {
    crate::core::models::property_value(properties, CUSTOM_ID_PROPERTY)
}

/// Captures HEADING's identity as of right now -- call this at the moment
/// you still have the heading in hand (e.g. parsing a historical `before`
/// snapshot), then `resolve` it later against a freshly parsed `flat` list.
pub fn capture_identity(heading: &Heading) -> HeadingIdentity {
    match get_custom_id(&heading.properties) {
        Some(id) if !id.is_empty() => HeadingIdentity::CustomId(id.to_string()),
        _ => HeadingIdentity::Title(heading.title.clone()),
    }
}

/// Resolves a captured identity against a current flat heading list.
/// `CustomId` matches the property exactly, with a case-insensitive key;
/// `Title` takes the first match.
pub fn resolve<'a>(flat: &'a [Heading], identity: &HeadingIdentity) -> Option<&'a Heading> {
    match identity {
        HeadingIdentity::CustomId(id) => flat.iter().find(|h| get_custom_id(&h.properties) == Some(id.as_str())),
        HeadingIdentity::Title(title) => flat.iter().find(|h| &h.title == title),
    }
}

/// The backend's own counterpart to the frontend's `headingTarget()`:
/// how a heading in hand is addressed in a later write.
pub fn target_of(heading: &Heading) -> crate::core::models::HeadingTarget {
    crate::core::models::HeadingTarget {
        line_no: heading.line_no,
        custom_id: custom_id_of(heading).map(str::to_string),
        title: Some(heading.title.clone()),
    }
}

/// Re-finds the heading a write is meant for, returning its index in the
/// freshly parsed FLAT -- the single gate every heading-mutating command
/// goes through.
///
/// `target.line_no` is treated as a hint, never as identity: it is only
/// believed once the heading actually sitting there proves to be the one
/// the caller meant. Otherwise the heading is re-found by its CUSTOM_ID,
/// which is what makes an edit survive a file that shifted under it (a
/// concurrent write, a sync pull, a subtask added above).
///
/// Fails closed for a heading that HAS a CUSTOM_ID -- every heading this
/// app creates does. It is either found by its id or the write is refused;
/// there is no falling back to "whatever is at that line", which is
/// precisely how one task's description used to overwrite another's. A
/// refused write surfaces as a visible error the user can retry; a
/// misapplied one silently destroys data.
///
/// For a heading WITHOUT one -- a hand-written file, a note never offered
/// for backfill -- the title is the only identity left, and it is held to
/// the same standard: it identifies the heading only while exactly one
/// heading in the file carries it. Then the write lands there, wherever
/// the line has moved to. Two headings sharing a title identify nothing,
/// so the write is refused even when one of them still sits at the
/// caller's line -- a file that shifted could just as well have moved the
/// OTHER one there, and there is no way to tell from here.
///
/// That is a real restriction on hand-written files: two same-named tasks
/// without ids cannot be edited until they can be told apart. The fix is
/// the one the app offers anyway -- stamp CUSTOM_IDs (Settings ->
/// Wartung), which is what every heading it creates itself already has.
///
/// `subject` only names the thing in the error message, and is part of
/// what the user gets to read -- German like every other message here
/// ("Überschrift", "Übergeordnete Überschrift", "Quell-Überschrift",
/// "Ziel-Überschrift"), same wording as core::errors::not_found_at_line's
/// own callers.
pub fn resolve_target(flat: &[Heading], target: &crate::core::models::HeadingTarget, subject: &str) -> Result<usize, String> {
    let at_line = flat.iter().position(|h| h.line_no == target.line_no);

    if let Some(id) = target.custom_id.as_deref().filter(|id| !id.is_empty()) {
        if at_line.is_some_and(|idx| get_custom_id(&flat[idx].properties) == Some(id)) {
            return Ok(at_line.unwrap());
        }
        // The line moved (or holds someone else now) -- the id still says
        // which heading this is, wherever it ended up in the file.
        return flat.iter().position(|h| get_custom_id(&h.properties) == Some(id)).ok_or_else(|| {
            crate::t!(
                "{subject} (CUSTOM_ID {id}) ist nicht mehr in der Datei -- die Änderung wurde nicht gespeichert, bitte neu laden",
                "{subject} (CUSTOM_ID {id}) is no longer in this file -- the change was not saved, please reload"
            )
        });
    }

    // No CUSTOM_ID to go by -- the title, under the rules documented above.
    let Some(expected_title) = target.title.as_deref() else {
        // A caller that offers no identity at all gets the old
        // line-number behaviour -- nothing better is available, and
        // refusing outright would break hand-built call sites for no gain.
        return at_line.ok_or_else(|| crate::core::errors::not_found_at_line(subject, target.line_no));
    };

    let mut matches = flat.iter().enumerate().filter(|(_, h)| h.title == expected_title);
    match (matches.next(), matches.next()) {
        (Some((idx, _)), None) => Ok(idx),
        // Deliberately also when one of them sits on the caller's own
        // line: that is not evidence, see the doc comment.
        (Some(_), Some(_)) => Err(crate::t!(
            "{subject} \"{expected_title}\" kommt in der Datei mehrfach vor und hat keine stabile ID -- die Änderung wurde nicht gespeichert. Unter Einstellungen -> Wartung lassen sich IDs vergeben, danach ist sie eindeutig.", "{subject} \"{expected_title}\" appears more than once in this file and has no stable ID -- the change was not saved. IDs can be assigned under Settings -> Maintenance, after which it is unambiguous."
        )),
        _ => Err(crate::core::errors::not_found_at_line(subject, target.line_no)),
    }
}

pub fn has_custom_id(heading: &Heading) -> bool {
    get_custom_id(&heading.properties).is_some_and(|id| !id.is_empty())
}

/// The heading's own CUSTOM_ID value, if it has one -- same case-
/// insensitive property lookup has_custom_id already uses, just returning
/// the value instead of a bool (needed by callers that actually want to
/// USE the id, e.g. ensure_custom_id_for_heading/find_orphaned_custom_ids).
pub fn custom_id_of(heading: &Heading) -> Option<&str> {
    get_custom_id(&heading.properties).filter(|id| !id.is_empty())
}

/// A fresh CUSTOM_ID: a v4 UUID with an `h:` prefix, matching the anchor
/// shape Org's HTML export gives a heading -- so a heading link can embed
/// the property value verbatim, with no marker to strip or re-add.
///
/// Only new ids get the prefix; an existing bare-UUID one is never
/// rewritten, and every consumer treats a CUSTOM_ID as an opaque string,
/// so both stay equally valid.
pub fn generate_custom_id() -> String {
    format!("h:{}", uuid::Uuid::new_v4())
}

/// Stamps a fresh CUSTOM_ID into PROPERTIES if (and only if) one isn't
/// already there.
pub fn ensure_custom_id(properties: &mut HashMap<String, String>) {
    if get_custom_id(properties).is_none_or(|id| id.is_empty()) {
        crate::core::models::set_property(properties, CUSTOM_ID_PROPERTY, generate_custom_id());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::models::{Heading, OrgTimestamp};

    fn heading(title: &str, custom_id: Option<&str>) -> Heading {
        let mut properties = HashMap::new();
        if let Some(id) = custom_id {
            properties.insert(CUSTOM_ID_PROPERTY.to_string(), id.to_string());
        }
        Heading {
            id: "x:1".into(),
            file_path: "x".into(),
            line_no: 1,
            last_line: 1,
            level: 1,
            title: title.into(),
            scheduled: None::<OrgTimestamp>,
            properties,
            ..Default::default()
        }
    }

    #[test]
    fn generate_custom_id_produces_a_well_formed_uuid_behind_its_h_prefix() {
        let id = generate_custom_id();
        assert!(id.starts_with("h:"), "{id}");
        assert!(uuid::Uuid::parse_str(id.strip_prefix("h:").unwrap()).is_ok(), "{id}");
    }

    #[test]
    fn generate_custom_id_is_not_derived_from_any_input() {
        // Same title, called twice -- must NOT produce related/prefixed
        // ids, since that was exactly the staleness problem (a slug tied to
        // the title breaks once the heading is renamed or refiled) this
        // design replaces.
        let a = generate_custom_id();
        let b = generate_custom_id();
        assert_ne!(a, b);
    }

    #[test]
    fn generate_custom_id_is_practically_unique_across_many_calls() {
        const SAMPLE: usize = 300;
        let ids: std::collections::HashSet<String> = (0..SAMPLE).map(|_| generate_custom_id()).collect();
        assert_eq!(ids.len(), SAMPLE, "no collisions across {SAMPLE} generations");
    }

    #[test]
    fn ensure_custom_id_adds_one_when_missing() {
        let mut props = HashMap::new();
        ensure_custom_id(&mut props);
        assert!(props.contains_key(CUSTOM_ID_PROPERTY));
    }

    #[test]
    fn ensure_custom_id_never_overwrites_an_existing_one() {
        let mut props = HashMap::new();
        props.insert(CUSTOM_ID_PROPERTY.to_string(), "keep-me".to_string());
        ensure_custom_id(&mut props);
        assert_eq!(props.get(CUSTOM_ID_PROPERTY).unwrap(), "keep-me");
    }

    #[test]
    fn ensure_custom_id_recognizes_the_property_case_insensitively() {
        let mut props = HashMap::new();
        props.insert("custom_id".to_string(), "already-there".to_string());
        ensure_custom_id(&mut props);
        assert_eq!(props.len(), 1, "did not add a second, differently-cased key");
    }

    #[test]
    fn capture_identity_prefers_custom_id_when_present() {
        let h = heading("Some Title", Some("stable-id"));
        assert!(matches!(capture_identity(&h), HeadingIdentity::CustomId(id) if id == "stable-id"));
    }

    #[test]
    fn capture_identity_falls_back_to_title_when_no_custom_id() {
        let h = heading("Some Title", None);
        assert!(matches!(capture_identity(&h), HeadingIdentity::Title(t) if t == "Some Title"));
    }

    #[test]
    fn capture_identity_and_resolve_recognize_a_lowercase_property_key() {
        // ensure_custom_id already treats `:custom_id:` as equivalent to
        // `:CUSTOM_ID:` (Emacs property keys are conventionally
        // case-insensitive) -- capture/resolve must agree, or a
        // hand-lowercased key would look present to one and absent to the
        // other.
        let mut props = HashMap::new();
        props.insert("custom_id".to_string(), "stable-id".to_string());
        let h = Heading { properties: props, ..heading("Some Title", None) };
        assert!(matches!(capture_identity(&h), HeadingIdentity::CustomId(id) if id == "stable-id"));

        let flat = vec![h];
        assert!(resolve(&flat, &HeadingIdentity::CustomId("stable-id".into())).is_some());
    }

    #[test]
    fn resolve_finds_by_custom_id_even_if_title_changed() {
        let flat = vec![heading("New Title", Some("stable-id")), heading("Other", Some("other-id"))];
        let found = resolve(&flat, &HeadingIdentity::CustomId("stable-id".into())).unwrap();
        assert_eq!(found.title, "New Title");
    }

    #[test]
    fn resolve_falls_back_to_title_matching_first_match_wins() {
        let flat = vec![heading("Dup", None), heading("Dup", None)];
        let found = resolve(&flat, &HeadingIdentity::Title("Dup".into())).unwrap();
        assert!(std::ptr::eq(found, &flat[0]), "resolves to the first occurrence");
    }

    /* --- resolve_target: the write guard --- */

    fn target(line_no: usize, custom_id: Option<&str>, title: Option<&str>) -> crate::core::models::HeadingTarget {
        crate::core::models::HeadingTarget { line_no, custom_id: custom_id.map(str::to_string), title: title.map(str::to_string) }
    }

    /// Two headings, ids "a" and "b", at lines 1 and 10.
    fn two_headings() -> Vec<Heading> {
        let mut first = heading("Erste", Some("a"));
        first.line_no = 1;
        let mut second = heading("Zweite", Some("b"));
        second.line_no = 10;
        vec![first, second]
    }

    #[test]
    fn resolve_target_takes_the_line_when_the_heading_there_is_the_expected_one() {
        let flat = two_headings();
        assert_eq!(resolve_target(&flat, &target(10, Some("b"), Some("Zweite")), "Heading").unwrap(), 1);
    }

    /// The reported bug, at its root: saving a description shifts every
    /// line below it, so the next write arrives with a number that now
    /// points at a DIFFERENT heading. Writing there would overwrite that
    /// other task; the id says where the intended one actually went.
    #[test]
    fn resolve_target_follows_the_custom_id_when_the_file_shifted_under_it() {
        let flat = two_headings();
        // The caller still thinks "b" sits at line 1, where "a" is now.
        let idx = resolve_target(&flat, &target(1, Some("b"), Some("Zweite")), "Heading").unwrap();
        assert_eq!(idx, 1, "resolved to the heading that owns the id, not to whatever sits at the line");
    }

    /// Fail closed. A heading that is gone (deleted elsewhere, or a file
    /// replaced by a sync pull) must NOT degrade into "write to the line
    /// anyway" -- an error is recoverable, a misapplied write is not.
    #[test]
    fn resolve_target_refuses_rather_than_falling_back_when_the_id_is_gone() {
        let flat = two_headings();
        let err = resolve_target(&flat, &target(1, Some("weg"), Some("Erste")), "Heading").unwrap_err();
        assert!(err.contains("weg"), "{err}");
    }

    #[test]
    fn resolve_target_confirms_an_id_less_heading_by_its_title() {
        let mut h = heading("Nur Titel", None);
        h.line_no = 4;
        let flat = vec![h];
        assert_eq!(resolve_target(&flat, &target(4, None, Some("Nur Titel")), "Heading").unwrap(), 0);
    }

    /// Without a CUSTOM_ID the title is the only identity left. It still
    /// relocates -- but only when it is unambiguous.
    #[test]
    fn resolve_target_relocates_an_id_less_heading_by_a_unique_title() {
        let mut a = heading("Alpha", None);
        a.line_no = 1;
        let mut b = heading("Beta", None);
        b.line_no = 7;
        let flat = vec![a, b];
        assert_eq!(resolve_target(&flat, &target(99, None, Some("Beta")), "Heading").unwrap(), 1);
    }

    /// A matching title ON the caller's own line is not evidence either
    /// once the title is ambiguous: the file may have shifted and moved
    /// the other same-named heading there. Refused, like every other
    /// case where the heading cannot be told apart.
    #[test]
    fn resolve_target_refuses_a_duplicate_title_even_when_one_sits_on_the_line() {
        let mut a = heading("Doppelt", None);
        a.line_no = 1;
        let mut b = heading("Doppelt", None);
        b.line_no = 7;
        let flat = vec![a, b];
        let err = resolve_target(&flat, &target(7, None, Some("Doppelt")), &crate::core::errors::subject::heading()).unwrap_err();
        assert!(err.contains("mehrfach"), "{err}");
        assert!(err.contains("Wartung"), "says how to make it unambiguous: {err}");
    }

    #[test]
    fn resolve_target_refuses_an_ambiguous_title_instead_of_guessing() {
        let mut a = heading("Doppelt", None);
        a.line_no = 1;
        let mut b = heading("Doppelt", None);
        b.line_no = 7;
        let flat = vec![a, b];
        assert!(resolve_target(&flat, &target(99, None, Some("Doppelt")), "Heading").is_err());
    }

    /// The line holds a heading, but not the one the caller meant -- the
    /// exact misfire (one task's edit landing on another) this guards.
    #[test]
    fn resolve_target_refuses_when_the_line_holds_a_different_id_less_heading() {
        let mut a = heading("Alpha", None);
        a.line_no = 1;
        let flat = vec![a];
        assert!(resolve_target(&flat, &target(1, None, Some("Beta")), "Heading").is_err());
    }

    #[test]
    fn resolve_target_without_any_identity_still_accepts_a_plain_line_number() {
        let flat = two_headings();
        assert_eq!(resolve_target(&flat, &target(10, None, None), "Heading").unwrap(), 1);
        assert!(resolve_target(&flat, &target(99, None, None), "Heading").is_err());
    }

    #[test]
    fn resolve_returns_none_when_nothing_matches() {
        let flat = vec![heading("Something", Some("id-1"))];
        assert!(resolve(&flat, &HeadingIdentity::CustomId("missing".into())).is_none());
        assert!(resolve(&flat, &HeadingIdentity::Title("Missing".into())).is_none());
    }
}
