pub const ORG_EFFORT_PROPERTY: &str = "Effort";

/// Real Org in-buffer setting name for "these are the values the effort
/// stepper/quick-select cycles through" (`C-c C-x e` in Emacs) --
/// `#+PROPERTY: Effort_ALL 0:15 0:30 1:00 2:00 4:00 8:00`. Scanned
/// case-insensitively (both the `#+PROPERTY:` keyword and the `Effort_ALL`
/// name itself -- org-mode treats both that way).
const EFFORT_ALL_DIRECTIVE: &str = "#+property:";
const EFFORT_ALL_PROPERTY_NAME: &str = "effort_all";

/// The default quick-select chips, used when neither a file's own
/// `#+PROPERTY: Effort_ALL` nor the global setting applies. Written in
/// unit shorthand rather than H:MM purely for readability; both normalize
/// to the same minutes.
///
/// The last step is "8h", not "1d": the "1d" chip is an 8-hour workday by
/// GTD convention, while `parse_effort`'s "d" is a real 24h day, matching
/// org-duration.el so hand-authored files round-trip.
pub const DEFAULT_EFFORT_PRESETS_ORG: &str = "15m 30m 1h 2h 4h 8h";

/// Parses a whitespace-separated list of Org EFFORT values (exactly what
/// follows `Effort_ALL` in a `#+PROPERTY:` line, or what's stored in
/// Config::effort_presets_org) into minutes, silently dropping any token
/// that doesn't parse -- one malformed value in a hand-edited list
/// shouldn't take down every other one.
pub fn parse_effort_all(raw: &str) -> Vec<u32> {
    raw.split_whitespace().filter_map(parse_effort).collect()
}

/// If LINE is a `#+PROPERTY: Effort_ALL ...` in-buffer setting, its parsed
/// values; `None` for any other line (including a `#+PROPERTY:` line for a
/// different property name, or a line with no values at all).
fn parse_effort_all_line(line: &str) -> Option<Vec<u32>> {
    let trimmed = line.trim();
    let prefix = trimmed.get(..EFFORT_ALL_DIRECTIVE.len())?;
    if !prefix.eq_ignore_ascii_case(EFFORT_ALL_DIRECTIVE) {
        return None;
    }
    let after_prefix = trimmed[EFFORT_ALL_DIRECTIVE.len()..].trim();
    let (name, values) = after_prefix.split_once(char::is_whitespace)?;
    if !name.eq_ignore_ascii_case(EFFORT_ALL_PROPERTY_NAME) {
        return None;
    }
    let parsed = parse_effort_all(values);
    (!parsed.is_empty()).then_some(parsed)
}

/// Scans CONTENT (a whole file's text) for a `#+PROPERTY: Effort_ALL` line
/// -- real Org allows this in-buffer setting anywhere in the file, not
/// just before the first heading, so this checks every line rather than
/// stopping at the first `*`. `None` means the file doesn't define one at
/// all (the caller falls back to Config::effort_presets_org, then
/// DEFAULT_EFFORT_PRESETS_ORG -- see commands_headings::get_effort_presets).
pub fn scan_effort_all(content: &str) -> Option<Vec<u32>> {
    content.lines().find_map(parse_effort_all_line)
}

/// One real day in minutes -- matches real Emacs `org-duration.el`'s own
/// default `org-duration-units` value for the "d" suffix exactly (a literal
/// 24h calendar day, not this app's own "8h workday" preset VALUE, see
/// DEFAULT_EFFORT_PRESETS_ORG's own doc comment on why those two don't
/// need to agree). Getting this right matters for round-tripping a
/// hand-authored file from a real Emacs user unchanged -- a stock Emacs
/// install computes "1d" as exactly this.
const DAY_MINUTES: u32 = 24 * 60;

/// Parses org ":EFFORT:" value -- every format the real Org manual itself
/// documents for this property (see `org-duration.el`): "3:12" (H:MM),
/// "1:23:45" (H:MM:SS, seconds rounded to the nearest minute), "90" (bare
/// minutes), and compound unit shorthand ("1d3h5min", "2h", "45min", ...).
pub fn parse_effort(raw: &str) -> Option<u32> {
    let s = raw.trim();
    if s.is_empty() { return None; }

    // H:MM / H:MM:SS -- org format. A stray colon in anything else is
    // never valid syntax, so this doesn't fall through to the other
    // formats below on failure -- it returns outright.
    if s.contains(':') {
        let parts: Vec<&str> = s.split(':').collect();
        return match parts.as_slice() {
            [h, m] => {
                let hours: u32 = h.trim().parse().ok()?;
                let mins: u32 = m.trim().parse().ok()?;
                (mins < 60).then_some(hours * 60 + mins)
            }
            [h, m, sec] => {
                let hours: u32 = h.trim().parse().ok()?;
                let mins: u32 = m.trim().parse().ok()?;
                let secs: u32 = sec.trim().parse().ok()?;
                // Rounds to the nearest minute (adding 30 before integer
                // division is the standard round-half-up trick) -- this
                // app's own internal representation, like format_effort's
                // own output, never carries sub-minute precision.
                (mins < 60 && secs < 60).then_some(hours * 60 + mins + (secs + 30) / 60)
            }
            _ => None,
        };
    }

    if let Ok(mins) = s.parse::<u32>() {
        return Some(mins);
    }

    // 1d3h5min / 1h30m / 2h / 45m / 45min
    let lower = s.to_lowercase();
    let mut total = 0u32;
    let mut buf = String::new();
    let mut matched = false;

    for c in lower.chars() {
        if c.is_ascii_digit() {
            buf.push(c);
        } else if c == 'd' {
            total += buf.parse::<u32>().ok()? * DAY_MINUTES;
            buf.clear();
            matched = true;
        } else if c == 'h' {
            total += buf.parse::<u32>().ok()? * 60;
            buf.clear();
            matched = true;
        } else if c == 'm' {
            if !buf.is_empty() {
                total += buf.parse::<u32>().ok()?;
                buf.clear();
            }
            matched = true;
        } else if c == 'i' || c == 'n' {
            continue; // "min"
        } else {
            return None;
        }
    }

    if !buf.is_empty() { return None; }
    matched.then_some(total)
}

/// Serializes minutes in emacs org-format H:MM.
pub fn format_effort(minutes: u32) -> String {
    format!("{}:{:02}", minutes / 60, minutes % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    // The exact three example formats the real Org manual itself
    // documents for the EFFORT property (see org-duration.el): "3:12",
    // "1:23:45", "1d3h5min".

    #[test]
    fn parse_effort_accepts_h_mm() {
        assert_eq!(parse_effort("3:12"), Some(3 * 60 + 12));
    }

    #[test]
    fn parse_effort_accepts_h_mm_ss_rounding_to_the_nearest_minute() {
        // 1h23m45s = 83.75 minutes -> rounds up to 84.
        assert_eq!(parse_effort("1:23:45"), Some(84));
        // Rounds down when under the half-minute mark.
        assert_eq!(parse_effort("1:23:20"), Some(83));
        // Exactly on the boundary rounds up (round-half-up).
        assert_eq!(parse_effort("1:23:30"), Some(84));
    }

    #[test]
    fn parse_effort_accepts_compound_day_hour_minute_shorthand() {
        assert_eq!(parse_effort("1d3h5min"), Some(DAY_MINUTES + 3 * 60 + 5));
    }

    #[test]
    fn parse_effort_d_alone_is_a_real_24_hour_day() {
        // Matches real Emacs org-duration.el's own default -- NOT this
        // app's own "1d = 8h workday" chip convention (see
        // DEFAULT_EFFORT_PRESETS_ORG's own doc comment on why those two
        // are deliberately different).
        assert_eq!(parse_effort("1d"), Some(1440));
        assert_eq!(parse_effort("2d"), Some(2880));
    }

    #[test]
    fn parse_effort_rejects_an_out_of_range_minutes_or_seconds_component() {
        assert_eq!(parse_effort("1:60"), None);
        assert_eq!(parse_effort("1:00:60"), None);
        assert_eq!(parse_effort("1:60:00"), None);
    }

    #[test]
    fn parse_effort_rejects_a_malformed_colon_value_rather_than_falling_through() {
        assert_eq!(parse_effort("1:2:3:4"), None);
        assert_eq!(parse_effort("abc:def"), None);
    }

    #[test]
    fn parse_effort_all_splits_and_parses_every_token() {
        assert_eq!(parse_effort_all("0:15 0:30 1:00 2:00 4:00 8:00"), vec![15, 30, 60, 120, 240, 480]);
    }

    #[test]
    fn parse_effort_all_drops_unparseable_tokens_but_keeps_the_rest() {
        assert_eq!(parse_effort_all("0:15 garbage 1:00"), vec![15, 60]);
    }

    #[test]
    fn parse_effort_all_of_an_empty_string_is_an_empty_list() {
        assert_eq!(parse_effort_all(""), Vec::<u32>::new());
    }

    #[test]
    fn scan_effort_all_finds_the_directive_anywhere_in_the_file_case_insensitively() {
        let content = "#+TITLE: My File\n\n#+Property: effort_ALL 0:10 0:20 0:30\n\n* TODO Task\n";
        assert_eq!(scan_effort_all(content), Some(vec![10, 20, 30]));
    }

    #[test]
    fn scan_effort_all_returns_none_when_the_directive_is_absent() {
        let content = "#+TITLE: My File\n\n* TODO Task\n";
        assert_eq!(scan_effort_all(content), None);
    }

    #[test]
    fn scan_effort_all_ignores_a_property_directive_for_a_different_name() {
        let content = "#+PROPERTY: STYLE habit\n* TODO Task\n";
        assert_eq!(scan_effort_all(content), None);
    }

    #[test]
    fn scan_effort_all_ignores_a_directive_with_no_values_at_all() {
        let content = "#+PROPERTY: Effort_ALL\n* TODO Task\n";
        assert_eq!(scan_effort_all(content), None);
    }

    #[test]
    fn scan_effort_all_uses_the_first_matching_directive_when_several_are_present() {
        let content = "#+PROPERTY: Effort_ALL 0:10 0:20\n#+PROPERTY: Effort_ALL 1:00 2:00\n";
        assert_eq!(scan_effort_all(content), Some(vec![10, 20]));
    }

    #[test]
    fn default_effort_presets_org_parses_to_the_documented_quick_select_steps() {
        assert_eq!(parse_effort_all(DEFAULT_EFFORT_PRESETS_ORG), vec![15, 30, 60, 120, 240, 480]);
    }
}