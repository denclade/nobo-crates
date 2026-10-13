//! Putting a collapsed property drawer back where it belongs.
//!
//! org-chef writes one drawer per recipe -- `:source-url:`, `:servings:`,
//! `:prep-time:`, `:cook-time:`, `:ready-in:`. A file can come back from
//! an import with every recipe's values stacked in the FIRST recipe's
//! drawer instead: twenty `:servings:` lines under the first heading, and
//! nineteen recipes with no properties at all. Org reads one value per
//! key, so nineteen recipes lose their servings and the first one shows
//! somebody else's.
//!
//! What makes this repairable rather than merely detectable is that the
//! values are still in file order: the Nth value of a key belongs to the
//! Nth recipe. That only holds when there are exactly as many values as
//! recipes, so a key with any other count is left exactly where it is.
//! In the file this was written for, `:source-url:` had thirteen values
//! for twenty recipes -- the seven hand-written recipes never carried the
//! key -- and no arithmetic can say which thirteen. A repair that guessed
//! there would write a wrong source under somebody's recipe, which is
//! worse than leaving a drawer untidy.
//!
//! Everything here works on the file's text and hands back new text. The
//! caller writes it, records the history entry, and can therefore undo it
//! like any other edit.

use std::collections::HashMap;

use nobo_core::core::org_syntax;

/// A cookbook file whose first recipe holds everybody's properties.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[cfg_attr(feature = "tauri", derive(specta::Type))]
pub struct CollapsedProperties {
    pub file_path: String,
    /// The recipe whose drawer holds them all -- the first one.
    pub title: String,
    #[cfg_attr(feature = "tauri", specta(type = specta_typescript::Number))]
    pub recipe_count: usize,
    /// The keys with one value per recipe. These are the ones handed
    /// back, in the order the drawer lists them.
    pub keys: Vec<String>,
    /// Keys repeated with any other count. They stay where they are, and
    /// this says so rather than letting them disappear quietly.
    pub left_alone: Vec<String>,
}

/// Whether PATH's text has a collapsed drawer, and what could be given
/// back from it.
pub fn find(file_path: &str, text: &str) -> Option<CollapsedProperties> {
    let plan = Plan::read(file_path, text)?;
    Some(plan.report)
}

/// The same file with every distributable value moved to the recipe it
/// belongs to, or `None` when there is nothing to repair.
pub fn repair(file_path: &str, text: &str) -> Option<(String, CollapsedProperties)> {
    let plan = Plan::read(file_path, text)?;
    Some((plan.apply(text), plan.report))
}

/// What the repair is going to do, worked out before anything is written.
struct Plan {
    report: CollapsedProperties,
    /// 0-based line index of each recipe's heading, in file order.
    headings: Vec<usize>,
    /// The first recipe's drawer, as line indices of `:PROPERTIES:` and
    /// `:END:` and the entries between them, in order.
    drawer: Drawer,
    /// Per recipe, the entries that recipe should end up owning.
    shares: Vec<Vec<(String, String)>>,
}

struct Drawer {
    start: usize,
    end: usize,
    entries: Vec<(String, String)>,
}

impl Plan {
    fn read(file_path: &str, text: &str) -> Option<Plan> {
        let lines: Vec<&str> = text.lines().collect();
        let recipes = crate::parse::recipes_of(file_path, text);
        if recipes.len() < 2 {
            return None;
        }
        let headings: Vec<usize> = recipes.iter().map(|recipe| recipe.line_no - 1).collect();
        let drawer = drawer_of(&lines, *headings.first()?)?;

        // Grouped by key, case-insensitively, keeping the order the
        // drawer lists them in -- which is the order they are handed back
        // in, so a repaired file reads like the file it came from.
        let mut order: Vec<String> = Vec::new();
        let mut values: HashMap<String, Vec<String>> = HashMap::new();
        for (key, value) in &drawer.entries {
            let folded = key.to_lowercase();
            if !values.contains_key(&folded) {
                order.push(key.clone());
            }
            values.entry(folded).or_default().push(value.clone());
        }

        let mut keys = Vec::new();
        let mut left_alone = Vec::new();
        for key in &order {
            let count = values[&key.to_lowercase()].len();
            if count < 2 {
                continue;
            }
            if count == recipes.len() {
                keys.push(key.clone());
            } else {
                left_alone.push(key.clone());
            }
        }
        if keys.is_empty() {
            return None;
        }

        // The Nth value to the Nth recipe, and nothing where the value is
        // empty: org-chef writes an empty key, but adding one to a recipe
        // that never had a drawer is noise, not information.
        let shares: Vec<Vec<(String, String)>> = (0..recipes.len())
            .map(|index| {
                keys.iter()
                    .filter_map(|key| {
                        let value = values[&key.to_lowercase()][index].trim();
                        (!value.is_empty()).then(|| (key.clone(), value.to_string()))
                    })
                    .collect()
            })
            .collect();

        Some(Plan {
            report: CollapsedProperties {
                file_path: file_path.to_string(),
                title: recipes[0].title.clone(),
                recipe_count: recipes.len(),
                keys,
                left_alone,
            },
            headings,
            drawer,
            shares,
        })
    }

    fn apply(&self, text: &str) -> String {
        let lines: Vec<&str> = text.lines().collect();
        let distributed: Vec<String> = self.report.keys.iter().map(|key| key.to_lowercase()).collect();

        // Rebuilt back to front, so an insertion never moves a line index
        // that a later step still needs.
        let mut out: Vec<String> = lines.iter().map(|line| line.to_string()).collect();

        for (index, heading) in self.headings.iter().enumerate().rev() {
            let share = &self.shares[index];
            if index == 0 {
                // The first recipe keeps its own share and everything the
                // repair would not touch -- the CUSTOM_ID, and any key
                // whose count did not match.
                let kept: Vec<(String, String)> =
                    self.drawer.entries.iter().filter(|(key, _)| !distributed.contains(&key.to_lowercase())).cloned().collect();
                let mut entries = share.clone();
                entries.extend(kept);
                out.splice(self.drawer.start..=self.drawer.end, drawer_lines(&entries));
                continue;
            }
            if share.is_empty() {
                continue;
            }
            match drawer_of(&lines, *heading) {
                // Already has one of its own: add what it is missing,
                // never replace what somebody wrote.
                Some(existing) => {
                    let mut entries = existing.entries.clone();
                    for (key, value) in share {
                        if !entries.iter().any(|(had, _)| had.eq_ignore_ascii_case(key)) {
                            entries.push((key.clone(), value.clone()));
                        }
                    }
                    out.splice(existing.start..=existing.end, drawer_lines(&entries));
                }
                None => {
                    let at = drawer_position(&lines, *heading);
                    out.splice(at..at, drawer_lines(share));
                }
            }
        }

        let mut text = out.join("\n");
        text.push('\n');
        text
    }
}

/// A drawer as org writes it (see org_syntax::drawer_lines).
fn drawer_lines(entries: &[(String, String)]) -> Vec<String> {
    let pairs: Vec<(&str, &str)> = entries.iter().map(|(key, value)| (key.as_str(), value.as_str())).collect();
    org_syntax::drawer_lines(&pairs)
}

/// Where a drawer would go for the heading on line HEADING: directly
/// after it, or after its one planning line -- org's own rule.
fn drawer_position(lines: &[&str], heading: usize) -> usize {
    let at = heading + 1;
    if at < lines.len() && org_syntax::is_planning_line(lines[at]) {
        at + 1
    } else {
        at
    }
}

/// The drawer belonging to the heading on line HEADING, if it has one and
/// it holds nothing but properties -- anything else in it would be lost
/// by rewriting it whole.
fn drawer_of(lines: &[&str], heading: usize) -> Option<Drawer> {
    let drawer = org_syntax::property_drawer(lines, heading).filter(|drawer| drawer.clean)?;
    let entries = drawer.properties.into_iter().map(|property| (property.key, property.value)).collect();
    Some(Drawer { start: drawer.start, end: drawer.end, entries })
}
