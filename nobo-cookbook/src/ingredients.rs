//! Reading a quantity out of an ingredient line, for display only.
//!
//! Ingredients in an org-chef file are plain list items -- "200 g Mehl",
//! "Eine Prise Salz", "2-3 Zwiebeln". Nothing about that is structured, so
//! this is a heuristic, and it is allowed to fail: a line it cannot read
//! is simply shown as written. What it must never do is guess wrong and
//! then be used to rewrite the file -- the parsed amount exists so the
//! reading view can offer "double the recipe", applied to the rendered
//! text and never to the org file.

use super::model::{Amount, Ingredient};


/// Vulgar fractions, which recipes use freely and `str::parse` does not
/// know. What actually turns up in kitchens; anything else falls through
/// to "no amount", which is a safe outcome.
const FRACTIONS: &[(char, f64)] = &[
    ('½', 0.5), ('⅓', 1.0 / 3.0), ('⅔', 2.0 / 3.0),
    ('¼', 0.25), ('¾', 0.75), ('⅛', 0.125),
];

/// Words that are not units, however much they look like one standing
/// between a number and the ingredient. Without this, "3 sehr reife
/// Bananen" would call "sehr" the unit.
///
/// A stop-list rather than an allow-list of real units: an allow-list has
/// no end (g, kg, ml, EL, TL, Msp., Prise, cups, tbsp, Stück, Zehen,
/// Dosen, ...) and being wrong there loses the amount entirely, while
/// being wrong here only mislabels one word.
fn is_unit_like(word: &str) -> bool {
    let word = word.trim_end_matches('.');
    !word.is_empty()
        && word.chars().all(char::is_alphabetic)
        && word.chars().count() <= 5
        && !matches!(
            word.to_lowercase().as_str(),
            "sehr" | "ganz" | "gut" | "frisch" | "reife" | "reifen" | "große" | "kleine"
                | "very" | "ripe" | "large" | "small"
        )
}

pub fn parse(text: &str) -> Ingredient {
    let text = text.trim().to_string();
    Ingredient { amount: parse_amount(&text), text }
}

fn parse_amount(text: &str) -> Option<Amount> {
    let (from, mut rest) = read_number(text.trim_start())?;

    // A range: "2-3 Zwiebeln", "2 - 3". An en dash is what a text editor
    // makes of the hyphen and what a recipe site pastes in, and "--" is
    // how org itself writes one, so all three count. The double hyphen is
    // tried first, or its first half would be taken for the whole
    // separator and the rest read as an unusable item.
    let mut to = None;
    let dashed = rest.trim_start();
    if let Some(after_dash) = dashed
        .strip_prefix("--")
        .or_else(|| dashed.strip_prefix('-'))
        .or_else(|| dashed.strip_prefix('–'))
    {
        if let Some((upper, after_upper)) = read_number(after_dash) {
            to = Some(upper);
            rest = after_upper;
        }
    }

    let rest = rest.trim_start();
    let (unit, item) = match rest.split_once(char::is_whitespace) {
        Some((first, tail)) if is_unit_like(first) => (first.to_string(), tail.trim().to_string()),
        // No separate unit: either the number counts the item itself
        // ("2 Eier") or the unit was glued to it ("200g Mehl"), which
        // read_number has already split off.
        _ => (String::new(), rest.to_string()),
    };

    (!item.is_empty() || !unit.is_empty()).then_some(Amount { from, to, unit, item })
}

/// A leading number and what follows it: `2`, `1,5`, `1.5`, `1/2`,
/// `1 3/4`, `½`, or a number with its unit glued on (`200g`).
fn read_number(input: &str) -> Option<(f64, &str)> {
    let input = input.trim_start();
    if let Some((value, rest)) = read_fraction_char(input) {
        return Some((value, rest));
    }

    let digits: String = input
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.' || *c == ',')
        .collect();
    let digits = digits.trim_end_matches(['.', ',']).to_string();
    if digits.is_empty() {
        return None;
    }
    let after_digits = &input[digits.len()..];
    let whole: f64 = digits.replace(',', ".").parse().ok()?;

    // "1/2 TL"
    if let Some(after_slash) = after_digits.strip_prefix('/') {
        if let Some((denominator, tail)) = read_integer(after_slash) {
            if denominator != 0.0 {
                return Some((whole / denominator, tail));
            }
        }
    }
    // "1 3/4 cups" -- the whole and its fraction, not two amounts.
    let spaced = after_digits.trim_start();
    if spaced.len() < after_digits.len() {
        if let Some((fraction, tail)) = read_fraction(spaced) {
            return Some((whole + fraction, tail));
        }
    }

    Some((whole, after_digits))
}

fn read_fraction_char(input: &str) -> Option<(f64, &str)> {
    let first = input.chars().next()?;
    FRACTIONS
        .iter()
        .find(|(c, _)| *c == first)
        .map(|(_, value)| (*value, &input[first.len_utf8()..]))
}

fn read_fraction(input: &str) -> Option<(f64, &str)> {
    if let Some(found) = read_fraction_char(input) {
        return Some(found);
    }
    let (numerator, after) = read_integer(input)?;
    let (denominator, tail) = read_integer(after.strip_prefix('/')?)?;
    (denominator != 0.0).then_some((numerator / denominator, tail))
}

fn read_integer(input: &str) -> Option<(f64, &str)> {
    let digits: String = input.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() {
        return None;
    }
    let rest = &input[digits.len()..];
    digits.parse().ok().map(|value| (value, rest))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn amount(line: &str) -> Amount {
        parse(line).amount.unwrap_or_else(|| panic!("no amount read from {line:?}"))
    }

    #[test]
    fn reads_a_plain_metric_amount() {
        let a = amount("200 g Spaghetti");
        assert_eq!((a.from, a.unit.as_str(), a.item.as_str()), (200.0, "g", "Spaghetti"));
        assert_eq!(a.to, None);
    }

    /// Written without the space, which is how half of every recipe on the
    /// internet does it.
    #[test]
    fn reads_a_unit_glued_to_the_number() {
        let a = amount("100g Guanciale");
        assert_eq!((a.from, a.unit.as_str(), a.item.as_str()), (100.0, "g", "Guanciale"));
    }

    #[test]
    fn a_bare_count_has_no_unit() {
        let a = amount("2 Eier");
        assert_eq!((a.from, a.unit.as_str(), a.item.as_str()), (2.0, "", "Eier"));
    }

    #[test]
    fn reads_the_units_german_recipes_actually_use() {
        for (line, unit) in [("2 EL Olivenöl", "EL"), ("½ TL Zucker", "TL"), ("1 Msp. Piment", "Msp.")] {
            assert_eq!(amount(line).unit, unit, "{line}");
        }
    }

    #[test]
    fn reads_a_decimal_comma_as_a_decimal_point() {
        assert_eq!(amount("1,5 kg reife Tomaten").from, 1.5);
    }

    #[test]
    fn reads_vulgar_fractions() {
        assert_eq!(amount("½ TL Zucker").from, 0.5);
        assert_eq!(amount("¼ TL Natron").from, 0.25);
    }

    #[test]
    fn reads_written_out_fractions_and_mixed_numbers() {
        assert_eq!(amount("1/2 tsp salt").from, 0.5);
        assert_eq!(amount("1 3/4 cups milk").from, 1.75);
        assert_eq!(amount("1 3/4 cups milk").unit, "cups");
    }

    /// Both ends are kept, so doubling the recipe moves them together
    /// instead of turning "2-3 Zwiebeln" into "4-3".
    #[test]
    fn reads_a_range_as_a_range() {
        let a = amount("2-3 Zwiebeln");
        assert_eq!((a.from, a.to), (2.0, Some(3.0)));
        assert_eq!(a.item, "Zwiebeln");
    }

    /// The point of the stop-list: an adjective is not a unit.
    #[test]
    fn an_adjective_after_the_number_is_not_mistaken_for_a_unit() {
        let a = amount("3 sehr reife Bananen");
        assert_eq!((a.unit.as_str(), a.item.as_str()), ("", "sehr reife Bananen"));
    }

    /// The failure mode that has to stay harmless: no number, no amount,
    /// and the line is still there in full.
    #[test]
    fn a_line_without_a_number_keeps_its_text_and_offers_no_amount() {
        for line in ["Eine Prise Salz", "frisch gemahlener Pfeffer", "Etwas Basilikum", "A pinch of salt"] {
            let parsed = parse(line);
            assert_eq!(parsed.amount, None, "{line}");
            assert_eq!(parsed.text, line);
        }
    }

    #[test]
    fn the_original_text_is_always_kept_alongside_the_amount() {
        assert_eq!(parse("  200 g Spaghetti  ").text, "200 g Spaghetti");
    }
}
