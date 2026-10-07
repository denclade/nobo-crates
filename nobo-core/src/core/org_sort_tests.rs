use super::*;

/// The caret at the first occurrence of NEEDLE, as a UTF-16 offset.
fn at(text: &str, needle: &str) -> u32 {
    let byte = text.find(needle).expect("needle in text");
    text[..byte].encode_utf16().count() as u32
}

/// TEXT with the sort applied, the way a host applies it.
fn sorted(text: &str, caret: &str, key: SortKey, descending: bool) -> String {
    let result = sort(text, at(text, caret), key, descending).expect("something to sort");
    let mut lines: Vec<String> = text.split('\n').map(str::to_string).collect();
    lines.splice(result.first..=result.last, result.lines);
    lines.join("\n")
}

// --- tables ---------------------------------------------------------------

const PRICES: &str = "| Artikel | Preis |\n|---+---|\n| Milch | 1,29 € |\n| Butter | 2,49 € |\n| Brot | 3,10 € |\n| Ei |  |";

#[test]
fn a_column_of_prices_offers_numbers_and_suggests_them() {
    let target = target(PRICES, at(PRICES, "2,49")).expect("a table");
    assert_eq!(target.scope, SortScope::Table);
    assert_eq!(target.label.as_deref(), Some("Preis"));
    assert_eq!(target.column, Some(1));
    assert_eq!(target.count, 4);
    assert_eq!(target.keys, vec![SortKey::Alpha, SortKey::Numeric]);
    assert_eq!(target.suggested, SortKey::Numeric);
}

#[test]
fn rows_sort_by_the_caret_column_and_the_header_stays() {
    let out = sorted(PRICES, "2,49", SortKey::Numeric, true);
    let lines: Vec<&str> = out.split('\n').collect();
    assert!(lines[0].contains("Artikel"), "{out}");
    assert!(lines[2].contains("Brot") && lines[3].contains("Butter") && lines[4].contains("Milch"), "{out}");
    // Empty goes last in either direction.
    assert!(lines[5].contains("Ei"), "{out}");
}

#[test]
fn on_the_header_the_column_below_is_sorted() {
    let out = sorted(PRICES, "Artikel", SortKey::Alpha, false);
    let names: Vec<&str> = out.split('\n').skip(2).map(|line| line.split('|').nth(1).unwrap().trim()).collect();
    assert_eq!(names, ["Brot", "Butter", "Ei", "Milch"]);
}

#[test]
fn a_rule_ends_the_group_like_in_org() {
    let text = "| b |\n| a |\n|---|\n| d |\n| c |";
    let out = sorted(text, "| d", SortKey::Alpha, false);
    assert_eq!(out, "| b |\n| a |\n|---|\n| c |\n| d |");
}

#[test]
fn a_column_of_dates_sorts_as_dates() {
    let text = "| Wann |\n|---|\n| <2026-10-07 Mi> |\n| 01.01.2025 |\n| [2026-01-15 Do 09:30] |";
    let target = target(text, at(text, "01.01")).unwrap();
    assert_eq!(target.suggested, SortKey::Time);
    let out = sorted(text, "01.01", SortKey::Time, false);
    let order: Vec<&str> = out.split('\n').skip(2).collect();
    assert!(order[0].contains("2025") && order[1].contains("2026-01-15") && order[2].contains("2026-10-07"), "{out}");
}

#[test]
fn words_among_numbers_mean_no_numeric_sort() {
    let text = "| 3 |\n| auf Anfrage |\n| 1 |";
    assert_eq!(target(text, 0).unwrap().keys, vec![SortKey::Alpha]);
    assert!(sort(text, 0, SortKey::Numeric, false).is_none());
}

#[test]
fn a_single_row_has_nothing_to_sort() {
    assert!(target("| a | b |\n|---+---|\n| 1 | 2 |", 0).is_none());
}

#[test]
fn a_table_inside_a_code_block_is_text() {
    let text = "#+BEGIN_SRC org\n| b |\n| a |\n#+END_SRC";
    assert!(target(text, at(text, "| b")).is_none());
}

#[test]
fn the_grid_api_sorts_the_same_way() {
    let lines: Vec<&str> = PRICES.split('\n').collect();
    let mut table = org_table::find_at(&lines, 0).unwrap();
    assert!(sort_table(&mut table, 3, 0, SortKey::Alpha, true));
    let rendered = org_table::render(&table);
    assert!(rendered[2].contains("Milch") && rendered[5].contains("Brot"), "{rendered:?}");
}

// --- natural order and values ----------------------------------------------

#[test]
fn letters_ignore_case_and_umlauts_and_numbers_compare_as_numbers() {
    let text = "- Raum 10\n- raum 2\n- Äpfel\n- Zebra";
    assert_eq!(sorted(text, "Raum 10", SortKey::Alpha, false), "- Äpfel\n- raum 2\n- Raum 10\n- Zebra");
}

#[test]
fn numbers_are_read_the_way_people_write_them() {
    assert_eq!(number_of("12,50 €"), Some(12.5));
    assert_eq!(number_of("1.234,5"), Some(1234.5));
    assert_eq!(number_of("1,234.5"), Some(1234.5));
    assert_eq!(number_of("1.000.000"), Some(1_000_000.0));
    assert_eq!(number_of("€ 3"), Some(3.0));
    assert_eq!(number_of("-2 h"), Some(-2.0));
    assert_eq!(number_of("zwei"), None);
}

#[test]
fn dates_come_out_comparable() {
    assert_eq!(time_of("<2026-10-07 Mi 09:30>").as_deref(), Some("2026-10-07 09:30"));
    assert_eq!(time_of("am 07.10.2026").as_deref(), Some("2026-10-07"));
    assert_eq!(time_of("keine"), None);
}

// --- lists ------------------------------------------------------------------

#[test]
fn list_items_take_their_sub_items_along() {
    let text = "Einkauf:\n- Zwiebeln\n  - rote\n- Äpfel\n  noch eine Zeile\n- Mehl\n\nDanach.";
    let out = sorted(text, "Mehl", SortKey::Alpha, false);
    assert_eq!(out, "Einkauf:\n- Äpfel\n  noch eine Zeile\n- Mehl\n- Zwiebeln\n  - rote\n\nDanach.");
}

#[test]
fn a_sub_list_is_sorted_on_its_own() {
    let text = "- A\n  - z\n  - y\n- B";
    assert_eq!(sorted(text, "z", SortKey::Alpha, false), "- A\n  - y\n  - z\n- B");
}

#[test]
fn numbered_items_are_renumbered() {
    let text = "1. Drei\n2. Eins\n3. Zwei";
    assert_eq!(sorted(text, "Eins", SortKey::Alpha, false), "1. Drei\n2. Eins\n3. Zwei");
    assert_eq!(sorted(text, "Eins", SortKey::Alpha, true), "1. Zwei\n2. Eins\n3. Drei");
}

#[test]
fn checkboxes_open_first() {
    let text = "- [X] erledigt\n- [ ] offen\n- [-] halb";
    let target = target(text, 0).unwrap();
    assert!(target.keys.contains(&SortKey::Checkbox));
    assert_eq!(sorted(text, "erledigt", SortKey::Checkbox, false), "- [ ] offen\n- [-] halb\n- [X] erledigt");
}

#[test]
fn blank_lines_between_items_stay_where_they_were() {
    let text = "- b\n\n- a\n- c\n\nText";
    assert_eq!(sorted(text, "- c", SortKey::Alpha, false), "- a\n\n- b\n- c\n\nText");
}

#[test]
fn a_continuation_line_belongs_to_its_list() {
    let text = "- b\n  mehr zu b\n- a";
    let target = target(text, at(text, "mehr")).unwrap();
    assert_eq!(target.scope, SortScope::List);
}

// --- headings ----------------------------------------------------------------

const PROJECT: &str = "* Projekt\nText bleibt oben.\n** TODO Zweites\nSCHEDULED: <2026-11-01 So>\n** DONE Erstes\n*** Kind von Erstes\n** NEXT [#A] Drittes\n* Anderes\n** bleibt";

#[test]
fn a_headings_children_move_with_their_subtrees() {
    let out = sorted(PROJECT, "Projekt", SortKey::Alpha, false);
    assert_eq!(
        out,
        "* Projekt\nText bleibt oben.\n** NEXT [#A] Drittes\n** DONE Erstes\n*** Kind von Erstes\n** TODO Zweites\nSCHEDULED: <2026-11-01 So>\n* Anderes\n** bleibt"
    );
}

#[test]
fn headings_offer_state_priority_and_date_when_they_have_them() {
    let target = target(PROJECT, at(PROJECT, "Projekt")).unwrap();
    assert_eq!(target.scope, SortScope::Headings);
    assert_eq!(target.label.as_deref(), Some("Projekt"));
    assert_eq!(target.keys, vec![SortKey::Alpha, SortKey::Todo, SortKey::Priority, SortKey::Time]);
    let by_state = sorted(PROJECT, "Projekt", SortKey::Todo, false);
    let order: Vec<&str> = by_state.lines().filter(|line| line.starts_with("** ")).collect();
    assert_eq!(order, ["** TODO Zweites", "** NEXT [#A] Drittes", "** DONE Erstes", "** bleibt"]);
}

#[test]
fn above_the_first_heading_the_top_level_is_sorted() {
    let text = "#+TITLE: Notiz\n\n* b\ntext b\n* a";
    assert_eq!(sorted(text, "TITLE", SortKey::Alpha, false), "#+TITLE: Notiz\n\n* a\n* b\ntext b");
}

#[test]
fn a_heading_without_children_has_nothing_to_sort() {
    assert!(target("* allein\nText", 0).is_none());
}

#[test]
fn a_star_line_in_a_code_block_is_no_child() {
    let text = "* P\n** b\n#+BEGIN_SRC org\n** z\n#+END_SRC\n** a";
    let out = sorted(text, "* P", SortKey::Alpha, false);
    assert_eq!(out, "* P\n** a\n** b\n#+BEGIN_SRC org\n** z\n#+END_SRC");
}


/// A `- x` inside an item's code block is code. Taken for an item, it
/// tore the block apart: #+BEGIN_SRC under one item, #+END_SRC under
/// another.
#[test]
fn a_code_block_in_an_item_moves_with_it_whole() {
    let text = "- m\n  #+BEGIN_SRC sh\n- a in the block\n  #+END_SRC\n- z\n- b";
    let out = sorted(text, "- z", SortKey::Alpha, false);
    assert_eq!(out, "- b\n- m\n  #+BEGIN_SRC sh\n- a in the block\n  #+END_SRC\n- z");
}

#[test]
fn inside_a_code_block_there_is_no_list() {
    let text = "#+BEGIN_SRC org\n- b\n- a\n#+END_SRC";
    assert!(target(text, at(text, "- b")).is_none());
}

/// org-sort with point in an entry's text sorts that entry's children.
#[test]
fn in_an_entrys_text_its_children_are_sorted() {
    let text = "* P\nText\n** b\n** a";
    let target = target(text, at(text, "Text")).expect("the children of P");
    assert_eq!(target.scope, SortScope::Headings);
    assert_eq!(target.label.as_deref(), Some("P"));
    assert_eq!(sorted(text, "Text", SortKey::Alpha, false), "* P\nText\n** a\n** b");
}
