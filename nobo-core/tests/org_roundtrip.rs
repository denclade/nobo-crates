//! What happens to org syntax this build does not model.
//!
//! A vault is shared with Emacs, with other org tools, and with the user's
//! own hands. Everything in here is legal org that nobo has no feature
//! for -- and the rule is that not understanding something is never a
//! reason to delete it.
//!
//! The test is deliberately blunt: parse a heading, rewrite its block with
//! nothing changed, and compare. `build_org_heading_block_unaltered` is
//! the same code path every state change, clock-in, ancestor touch and
//! merge goes through, so anything this drops is dropped by all of them --
//! without the user editing the heading at all.
//!
//! Cases that are known NOT to round-trip are listed at the bottom, each
//! with what actually happens and why. See `docs/org-support.md`.

use nobo_core::core::heading_utils::{build_org_heading_block, build_org_heading_block_unaltered};
use nobo_core::core::models::HeadingUpdatePayload;
use nobo_core::core::org_parser::parse_headings_from_lines;
use std::path::Path;

/// Parses TEXT as a single heading and writes it straight back out.
fn round_trip(text: &str) -> String {
    let headings = parse_headings_from_lines(Path::new("wild.org"), text.lines());
    assert!(!headings.is_empty(), "fixture did not parse as a heading:\n{text}");
    let mut out = build_org_heading_block_unaltered(&headings[0]).join("\n");
    out.push('\n');
    out
}

/// Asserts every line of TEXT is still there afterwards, in order.
///
/// Not byte equality: the writer legitimately normalises some things
/// (property drawer key order, the LOGBOOK wrapper it regenerates). What
/// it must never do is lose a line.
fn assert_nothing_lost(text: &str) {
    let out = round_trip(text);
    let mut haystack = out.as_str();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        match haystack.find(line.trim_end()) {
            Some(at) => haystack = &haystack[at + line.trim_end().len()..],
            None => panic!("line was lost or reordered:\n  {line}\n\nin:\n{out}"),
        }
    }
}

const HEADER: &str = "* TODO Aufgabe\n:PROPERTIES:\n:CUSTOM_ID: h:wild\n:END:\n";

/// Everything in a heading's BODY is content as far as this app is
/// concerned, and content is written back verbatim. This pins that for
/// the constructs most likely to tempt a future parser into touching
/// them.
#[test]
fn body_level_org_constructs_survive_a_rewrite() {
    for (name, body) in [
        ("plain list", "- eins\n- zwei\n  - verschachtelt\n"),
        ("ordered list with counter", "1. eins\n2. [@5] fünf\n"),
        ("checkbox items", "- [ ] offen\n- [X] erledigt\n- [-] teilweise\n"),
        ("description list", "- Begriff :: Erklärung\n"),
        ("table", "| a | b |\n|---+---|\n| 1 | 2 |\n"),
        ("table.el table", "+---+---+\n| a | b |\n+---+---+\n"),
        ("src block", "#+begin_src python\nprint('hi')\n#+end_src\n"),
        ("example block", "#+begin_example\n* not a heading\n#+end_example\n"),
        ("quote block", "#+begin_quote\nZitat\n#+end_quote\n"),
        ("verse block", "#+begin_verse\nZeile\n  eingerückt\n#+end_verse\n"),
        ("export block", "#+begin_export html\n<b>x</b>\n#+end_export\n"),
        ("dynamic block", "#+begin: clocktable :scope file\n| x |\n#+end:\n"),
        ("latex environment", "\\begin{equation}\nx = y\n\\end{equation}\n"),
        ("footnote definition", "[fn:1] Die Fußnote.\n"),
        ("footnote reference", "Text mit Fußnote[fn:1].\n"),
        ("horizontal rule", "-----\n"),
        ("fixed width area", ": fester Text\n: zweite Zeile\n"),
        ("comment line", "# ein Kommentar\n"),
        ("affiliated keyword", "#+NAME: tabelle\n#+CAPTION: Eine Tabelle\n| a |\n"),
        ("diary sexp", "%%(diary-float t 4 2)\n"),
        ("target and radio target", "<<ziel>> und <<<radio>>>\n"),
        ("macro", "{{{titel}}} und {{{autor(x)}}}\n"),
        ("export snippet", "@@html:<br>@@\n"),
        ("inline babel call", "call_summe(1,2)\n"),
        ("inline src", "src_python{1+1}\n"),
        ("citation", "[cite/t:@doe2020]\n"),
        ("entities and latex fragments", "\\alpha und \\(x^2\\) und $$y$$\n"),
        ("text markup", "*fett* /kursiv/ _unterstrichen_ =verbatim= ~code~ +durch+\n"),
        ("subscript and superscript", "H_2O und E^2\n"),
        ("line break", "erste Zeile \\\\\nzweite Zeile\n"),
        ("all link forms", "[[https://x/y][Text]] <https://x> https://x [[file:a.org::*Kopf]]\n"),
        ("inactive timestamp in body", "Notiz vom [2026-08-05 Wed 09:00]\n"),
        ("timestamp range in body", "<2026-08-05 Wed>--<2026-08-07 Fri>\n"),
        ("diary timestamp", "<%%(diary-float t 4 2)>\n"),
    ] {
        let text = format!("{HEADER}{body}");
        let out = round_trip(&text);
        for line in body.lines().filter(|l| !l.trim().is_empty()) {
            assert!(out.contains(line), "{name}: lost {line:?}\n\ngot:\n{out}");
        }
    }
}

/// An inlinetask is a heading deep enough that org treats it as a block
/// inside the section rather than as structure. nobo's own rule for
/// absorbing a `*`-line into the body (see org_parser's
/// `is_real_structure`) has to leave it intact either way.
#[test]
fn an_inlinetask_is_not_flattened() {
    let text = format!("{HEADER}*************** TODO Kleinkram\n*************** END\n");
    let out = round_trip(&text);

    assert!(out.contains("*************** TODO Kleinkram"), "{out}");
    assert!(out.contains("*************** END"), "{out}");
}

/// Drawers this build has no feature for -- see Heading::raw_drawers.
#[test]
fn foreign_drawers_survive_with_their_markers() {
    let text = format!("{HEADER}:NOTES:\ngeheim\n:END:\n:CUSTOM_THING:\n| a | b |\n:END:\nBody\n");

    assert_nothing_lost(&text);
}

/// The LOGBOOK is regenerated from typed fields, so anything in it that
/// is neither a CLOCK nor a state log needs its own bucket -- see
/// Heading::logbook_extra.
#[test]
fn logbook_notes_and_their_continuations_survive() {
    let text = format!(
        "{HEADER}:LOGBOOK:\nCLOCK: [2026-08-05 Wed 08:00]--[2026-08-05 Wed 09:00] =>  1:00\n\
         - Note taken on [2026-08-05 Wed 09:00] \\\\\n  weil der Kunde abgesagt hat\n\
         - State \"DONE\" from \"TODO\" [2026-08-05 Wed 09:00]\n:END:\n"
    );

    assert_nothing_lost(&text);
}

/// A write that DOES change the log has to leave the rest of the drawer
/// where it was -- the note next to the state change it explains, the
/// clock where the file had it -- with the new entry on top, which is
/// where org puts one.
///
/// The plain round-trip above cannot see this: it goes through the
/// untouched-log path, which writes `raw_logbook` back verbatim.
#[test]
fn a_state_change_leaves_the_rest_of_the_logbook_in_place() {
    let text = format!(
        "{HEADER}:LOGBOOK:\n\
         - State \"NEXT\"       from \"TODO\"       [2026-08-05 Wed 09:00]\n\
         - Note taken on [2026-08-05 Wed 09:00] \\\\\n  weil der Kunde abgesagt hat\n\
         CLOCK: [2026-08-05 Wed 08:00]--[2026-08-05 Wed 09:00] =>  1:00\n\
         :END:\n"
    );
    let heading = parse_headings_from_lines(Path::new("wild.org"), text.lines())[0].clone();

    let payload = HeadingUpdatePayload { state: Some("DONE".to_string()), ..Default::default() };
    let out = build_org_heading_block(&heading, &payload).join("\n");

    let at = |needle: &str| out.find(needle).unwrap_or_else(|| panic!("missing {needle}\n{out}"));
    assert!(at("- State \"DONE") < at("- State \"NEXT"), "the new entry goes on top: {out}");
    assert!(at("- State \"NEXT") < at("- Note taken on"), "the note stays with its state change: {out}");
    assert!(at("- Note taken on") < at("  weil der Kunde"), "and keeps its continuation: {out}");
    assert!(at("  weil der Kunde") < at("CLOCK:"), "the clock stays where the file had it: {out}");
}

/// Property drawers carry more than `:KEY: VALUE`.
#[test]
fn unusual_property_drawer_contents_survive() {
    let text = "* TODO Aufgabe\n:PROPERTIES:\n:CUSTOM_ID: h:wild\n:UNBEKANNT: irgendwas\n:LEER:\n:MIT_DOPPELPUNKT: a:b:c\n:END:\n";
    let out = round_trip(text);

    for line in [":UNBEKANNT: irgendwas", ":MIT_DOPPELPUNKT: a:b:c"] {
        assert!(out.contains(line), "lost {line:?}\n\ngot:\n{out}");
    }
}

/// Every timestamp shape org allows in a planning line.
#[test]
fn planning_timestamps_survive_in_every_shape() {
    for planning in [
        "SCHEDULED: <2026-08-05 Wed>",
        "SCHEDULED: <2026-08-05 Wed 09:00>",
        "SCHEDULED: <2026-08-05 Wed 09:00-10:30>",
        "SCHEDULED: <2026-08-05 Wed +1w>",
        "SCHEDULED: <2026-08-05 Wed ++1w>",
        "SCHEDULED: <2026-08-05 Wed .+1d>",
        "SCHEDULED: <2026-08-05 Wed .+1d/3d>",
        "SCHEDULED: <2026-08-05 Wed -5d>",
        "SCHEDULED: <2026-08-05 Wed +1w -2d>",
        "DEADLINE: <2026-08-05 Wed -5d>",
        "DEADLINE: <2026-08-05 Wed +1w --2d>",
    ] {
        let text = format!("* TODO Aufgabe\n{planning}\n:PROPERTIES:\n:CUSTOM_ID: h:wild\n:END:\n");
        let out = round_trip(&text);
        assert!(out.contains(planning), "lost {planning:?}\n\ngot:\n{out}");
    }
}

/// Statistics cookies are the one thing on the heading line this app
/// rewrites, so everything else on it has to be left alone.
#[test]
fn a_link_in_the_heading_title_survives() {
    let text = "* TODO Siehe [[https://example.com/doku][Doku]]\n:PROPERTIES:\n:CUSTOM_ID: h:wild\n:END:\n";
    let out = round_trip(text);

    assert!(out.contains("[[https://example.com/doku][Doku]]"), "{out}");
}

/* ---------------------------------------------------------------
Known gaps. Each of these asserts what CURRENTLY happens, so the
day one is fixed the test fails and says so. Documented in
docs/org-support.md.
--------------------------------------------------------------- */

/// The priority cookie is its own field now, so it comes back where org
/// puts it -- between the keyword and the title -- rather than as three
/// characters of title text that sorting and search could not see.
#[test]
fn a_priority_cookie_is_read_as_a_priority() {
    use nobo_core::core::org_parser::parse_headings_from_lines;

    let text = "* TODO [#A] Wichtig\n:PROPERTIES:\n:CUSTOM_ID: h:wild\n:END:\n";
    let heading = &parse_headings_from_lines(Path::new("wild.org"), text.lines())[0];

    assert_eq!(heading.priority.as_deref(), Some("A"));
    assert_eq!(heading.title, "Wichtig", "and not part of the title any more");
    assert_eq!(round_trip(text), text);
}

/// Org's priority range is configurable, so nothing here polices it.
#[test]
fn an_unusual_priority_still_round_trips() {
    for cookie in ["[#B]", "[#C]", "[#1]", "[#42]"] {
        let text = format!("* TODO {cookie} Etwas\n:PROPERTIES:\n:CUSTOM_ID: h:wild\n:END:\n");
        assert_eq!(round_trip(&text), text, "{cookie}");
    }
}

/// A bracketed thing that is not a priority stays in the title.
#[test]
fn a_bracketed_non_priority_is_left_in_the_title() {
    use nobo_core::core::org_parser::parse_headings_from_lines;

    let text = "* TODO [#hier ein Leerzeichen] Etwas\n:PROPERTIES:\n:CUSTOM_ID: h:wild\n:END:\n";
    let heading = &parse_headings_from_lines(Path::new("wild.org"), text.lines())[0];

    assert!(heading.priority.is_none());
    assert!(heading.title.starts_with("[#hier"), "{}", heading.title);
}

/// Org's `:KEY+:` accumulation: both lines survive, and a reader asking
/// for the property gets both halves.
///
/// They used to round-trip as two unrelated keys, so anything reading
/// `ZUTAT` saw only "Mehl".
#[test]
fn accumulating_properties_round_trip_and_read_as_one_value() {
    use nobo_core::core::models::accumulated_property_value;
    use nobo_core::core::org_parser::parse_headings_from_lines;

    let text = "* TODO Aufgabe\n:PROPERTIES:\n:CUSTOM_ID: h:wild\n:ZUTAT: Mehl\n:ZUTAT+: Wasser\n:END:\n";
    let out = round_trip(text);

    assert!(out.contains(":ZUTAT: Mehl"), "{out}");
    assert!(out.contains(":ZUTAT+: Wasser"), "{out}");

    let heading = &parse_headings_from_lines(Path::new("wild.org"), text.lines())[0];
    assert_eq!(accumulated_property_value(&heading.properties, "ZUTAT").as_deref(), Some("Mehl Wasser"));
}

/* ---------------------------------------------------------------
Whole files, through the real write path.
--------------------------------------------------------------- */

/// The block-level tests above exercise the serializer. This exercises
/// what a command actually does: open the file, change one heading, write
/// the whole thing back. Anything the parser silently drops shows up here
/// as a line missing from a file the user never edited.
fn file_round_trip(original: &str) -> String {
    use nobo_core::core::heading_utils;
    use nobo_core::core::models::HeadingUpdatePayload;

    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("wild.org");
    std::fs::write(&path, original).unwrap();

    let flat = nobo_core::core::heading_utils::parse_from_disk(&path);
    if let Some(first) = flat.first() {
        let target = nobo_core::core::heading_identity::target_of(first);
        let mut edit = heading_utils::open_edit(&path, &target, &nobo_core::core::errors::subject::heading()).unwrap();
        // A state change: the most ordinary edit there is, and the one
        // that rewrites the most of a block.
        edit.write(&HeadingUpdatePayload { state: Some("NEXT".into()), ..Default::default() }).unwrap();
        edit.commit().unwrap();
    }
    std::fs::read_to_string(&path).unwrap()
}

/// Everything above the first heading -- `#+TITLE:`, a file-level
/// property drawer, `#+FILETAGS:` -- belongs to nobody and must not move.
#[test]
fn the_zeroth_section_survives_an_edit_further_down() {
    let original = concat!(
        "#+TITLE: Wildes Dokument\n",
        "#+AUTHOR: Jemand\n",
        "#+FILETAGS: :arbeit:privat:\n",
        "#+STARTUP: overview\n",
        "#+OPTIONS: toc:nil\n",
        ":PROPERTIES:\n",
        ":ID: datei-eins\n",
        ":END:\n",
        "\n",
        "Ein Absatz vor der ersten Überschrift.\n",
        "\n",
        "* TODO Aufgabe\n",
        ":PROPERTIES:\n",
        ":CUSTOM_ID: h:wild\n",
        ":END:\n",
    );

    let out = file_round_trip(original);

    for line in [
        "#+TITLE: Wildes Dokument",
        "#+AUTHOR: Jemand",
        "#+FILETAGS: :arbeit:privat:",
        "#+STARTUP: overview",
        "#+OPTIONS: toc:nil",
        ":ID: datei-eins",
        "Ein Absatz vor der ersten Überschrift.",
    ] {
        assert!(out.contains(line), "lost {line:?}\n\ngot:\n{out}");
    }
}

/// Headings that are not the one being edited must come back byte for
/// byte -- including everything about them this build has no idea about.
#[test]
fn untouched_siblings_come_back_byte_for_byte() {
    let sibling = concat!(
        "* TODO Nachbar [#B] :arbeit:dringend:\n",
        "DEADLINE: <2026-08-05 Wed -3d> SCHEDULED: <2026-08-01 Sat +1m>\n",
        ":PROPERTIES:\n",
        ":CUSTOM_ID: h:nachbar\n",
        ":ORDERED: t\n",
        ":ZUTAT: Mehl\n",
        ":ZUTAT+: Wasser\n",
        ":END:\n",
        ":LOGBOOK:\n",
        "CLOCK: [2026-08-01 Sat 08:00]--[2026-08-01 Sat 09:00] =>  1:00\n",
        "- Note taken on [2026-08-01 Sat 09:00] \\\\\n",
        "  eine Notiz mit Folgezeile\n",
        ":END:\n",
        ":BRAINSTORM:\n",
        "irgendwas\n",
        ":END:\n",
        "#+begin_src emacs-lisp :results none\n",
        "(message \"* nicht überschrift\")\n",
        "#+end_src\n",
        "| Spalte | Wert |\n",
        "|--------+------|\n",
        "| a      | 1    |\n",
        "\n",
        "- [ ] Punkt eins\n",
        "- [X] Punkt zwei\n",
    );
    let original = format!("* TODO Erste\n:PROPERTIES:\n:CUSTOM_ID: h:erste\n:END:\n{sibling}");

    let out = file_round_trip(&original);

    assert!(out.contains(sibling), "the untouched sibling changed:\n--- expected ---\n{sibling}\n--- got ---\n{out}");
}

/// A `:PROPERTIES:` line inside a source block is source code, not a
/// drawer -- org's own rule, and the reason blocks are opaque.
#[test]
fn org_syntax_inside_a_source_block_stays_source_code() {
    let original = concat!(
        "* TODO Aufgabe\n",
        ":PROPERTIES:\n",
        ":CUSTOM_ID: h:wild\n",
        ":END:\n",
        "#+begin_src org\n",
        "* TODO Beispielaufgabe\n",
        ":PROPERTIES:\n",
        ":CUSTOM_ID: nur-ein-beispiel\n",
        ":END:\n",
        "SCHEDULED: <2026-01-01 Thu>\n",
        ":LOGBOOK:\n",
        "CLOCK: [2026-01-01 Thu 10:00]\n",
        ":END:\n",
        "#+end_src\n",
    );

    let out = file_round_trip(original);

    assert!(out.contains("* TODO Beispielaufgabe"), "{out}");
    assert!(out.contains(":CUSTOM_ID: nur-ein-beispiel"), "{out}");
    assert!(out.contains("CLOCK: [2026-01-01 Thu 10:00]"), "{out}");
    assert_eq!(out.matches("#+end_src").count(), 1, "{out}");
}

/// Text that merely looks structural.
#[test]
fn prose_that_looks_like_org_markup_is_left_alone() {
    let body = concat!(
        ":shrug:\n",
        ":das ist eine Notiz:\n",
        ":END:\n",
        "* nicht eingerückt, aber auch keine Überschrift laut unserer Regel\n",
        "SCHEDULED: das ist nur Text\n",
        "CLOCK: auch nur Text\n",
        "- State ohne Anführungszeichen\n",
        "#+FALSCH_GESCHRIEBEN\n",
    );
    let original = format!("* TODO Aufgabe\n:PROPERTIES:\n:CUSTOM_ID: h:wild\n:END:\n{body}");

    let out = file_round_trip(&original);

    for line in body.lines() {
        assert!(out.contains(line), "lost {line:?}\n\ngot:\n{out}");
    }
}

/// Non-ASCII in every position that gets parsed by byte offsets somewhere.
#[test]
fn unicode_everywhere_does_not_panic_or_truncate() {
    let original = concat!(
        "#+TITLE: Größenübersicht — 日本語 — العربية\n",
        "* TODO Aufgabe mit Ümläuten und 🎉 :straße:日本:\n",
        ":PROPERTIES:\n",
        ":CUSTOM_ID: h:größe\n",
        ":BESCHRÄNKUNG: höchstens fünf\n",
        ":END:\n",
        "Fließtext mit ß, é, 中文 und einem Emoji 🚀.\n",
    );

    let out = file_round_trip(original);

    for line in ["Größenübersicht — 日本語 — العربية", ":BESCHRÄNKUNG: höchstens fünf", "Fließtext mit ß, é, 中文 und einem Emoji 🚀."]
    {
        assert!(out.contains(line), "lost {line:?}\n\ngot:\n{out}");
    }
    assert!(out.contains(":straße:") && out.contains("日本"), "tags survived: {out}");
}

/// A hand-arranged property drawer keeps its arrangement.
///
/// Everything used to come back alphabetical, because `properties` is a
/// `HashMap` whose iteration order Rust randomises per process and the
/// writer sorted to get a deterministic one. Determinism is genuinely
/// required (the merge contract rests on it) -- but remembering the file's
/// own order gives both, and stops every edit churning a drawer nobody
/// touched.
#[test]
fn a_property_drawer_keeps_the_order_the_file_had() {
    let text = "* TODO Aufgabe\n:PROPERTIES:\n:ZWEITENS: b\n:ERSTENS: a\n:CUSTOM_ID: h:wild\n:END:\n";

    assert_eq!(round_trip(text), text);
}

/// A key the file did not have goes after the ones it did, in a
/// deterministic place.
#[test]
fn a_new_property_sorts_in_after_the_remembered_ones() {
    use nobo_core::core::org_parser::parse_headings_from_lines;

    let text = "* TODO Aufgabe\n:PROPERTIES:\n:ZWEITENS: b\n:ERSTENS: a\n:END:\n";
    let mut heading = parse_headings_from_lines(Path::new("wild.org"), text.lines())[0].clone();
    heading.properties.insert("AAA_NEU".to_string(), "x".to_string());

    let block = nobo_core::core::heading_utils::build_org_heading_block_unaltered(&heading).join("\n");
    let at = |needle: &str| block.find(needle).unwrap_or_else(|| panic!("missing {needle}\n{block}"));

    assert!(at(":ZWEITENS:") < at(":ERSTENS:"), "the file's own order is kept: {block}");
    assert!(at(":ERSTENS:") < at(":AAA_NEU:"), "and a new key follows them: {block}");
}

/// A key written twice comes back written twice, in place. The map behind
/// `properties` can only hold one of them, so the rest are kept verbatim
/// -- see Heading::repeated_properties. (Org's own answer to wanting two
/// values is `:KEY+:`, which is modelled; see the accumulation test
/// above. This is about a file that repeats the plain key anyway.)
#[test]
fn a_literally_repeated_property_key_survives_a_rewrite() {
    let text = "* TODO Aufgabe\n:PROPERTIES:\n:CUSTOM_ID: h:wild\n:ZUTAT: Mehl\n:ZUTAT: Wasser\n:ZUTAT: Salz\n:END:\n";
    let out = round_trip(text);

    assert_eq!(out.matches(":ZUTAT:").count(), 3, "{out}");
    let at = |needle: &str| out.find(needle).unwrap_or_else(|| panic!("missing {needle}\n{out}"));
    assert!(at("Mehl") < at("Wasser") && at("Wasser") < at("Salz"), "in the file's own order: {out}");
    assert!(at(":CUSTOM_ID:") < at("Mehl"), "and after the key that came before them: {out}");
}

/// The first is the one the app reads, which is what org does for a
/// repeated key.
#[test]
fn a_repeated_key_reads_as_its_first_value() {
    let text = "* TODO Aufgabe\n:PROPERTIES:\n:ZUTAT: Mehl\n:ZUTAT: Wasser\n:END:\n";
    let heading = parse_headings_from_lines(Path::new("wild.org"), text.lines())[0].clone();

    assert_eq!(heading.properties.get("ZUTAT").map(String::as_str), Some("Mehl"));
    assert_eq!(heading.repeated_properties, vec![("ZUTAT".to_string(), "Wasser".to_string())]);
}

/// Tag alignment is nobo's, not the file's.
#[test]
fn known_gap_heading_tag_alignment_is_normalised() {
    let text = "* TODO Aufgabe    :arbeit:\n:PROPERTIES:\n:CUSTOM_ID: h:wild\n:END:\n";
    let out = round_trip(text);

    assert!(out.contains(":arbeit:"), "the tags themselves survive: {out}");
    assert!(!out.contains("Aufgabe    :arbeit:"), "but not the spacing: {out}");
}
