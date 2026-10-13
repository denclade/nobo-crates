use crate::core::models::{has_project_tag, Heading, RefilePosition};

/// Converts `flat_headings` into a hierarchical tree, based on their levels.
///
/// # Visualization of the Stack (`path`)
/// The stack maintains a strict lineage of parents. If a new heading arrives with a level
/// equal to or shallower than the top of the stack (e.g., Level 2 follows Level 3),
/// the stack unwinds: elements are popped and attached to their parents until the
/// correct hierarchy depth is reached.
///
/// **Input Document:**
/// ```text
/// * A   (Level 1)
/// ** B  (Level 2)
/// *** C (Level 3)
/// ** D  (Level 2)
/// ```
///
/// **Step-by-Step Execution:**
/// ```text
/// 1. Read `* A`   | Stack: [A]
/// 2. Read `** B`  | Stack: [A, B]
/// 3. Read `*** C` | Stack: [A, B, C]
///
/// 4. Read `** D`  | Level 2 triggers the while-loop to clean up elements >= Level 2:
///
///    -> Pop `C`   | Stack becomes: [A, B]
///                 | Attach C to new top (B).
///                 | Tree state:  B
///                 |              └── C
///
///    -> Pop `B`   | Stack becomes: [A]
///                 | Attach B to new top (A).
///                 | Tree state:  A
///                 |              └── B
///                 |                  └── C
///
///    -> Push `D`  | Stack becomes: [A, D]
///
/// 5. End of list  | Flush the remaining stack:
///
///    -> Pop `D`   | Stack becomes: [A]
///                 | Attach D to new top (A).
///                 | Tree state:  A
///                 |              ├── B
///                 |              │   └── C
///                 |              └── D
///
///    -> Pop `A`   | Stack becomes: []
///                 | Stack is empty, move A to `roots`.
/// ```
pub fn build_heading_tree(flat_headings: Vec<Heading>) -> Vec<Heading> {
    let mut roots = Vec::new();
    let mut path: Vec<Heading> = Vec::new();

    for mut heading in flat_headings {
        heading.children = Vec::new();
        while !path.is_empty() && path.last().unwrap().level >= heading.level {
            let child = path.pop().unwrap();
            if let Some(parent) = path.last_mut() {
                parent.children.push(child);
            } else {
                roots.push(child);
            }
        }
        path.push(heading);
    }

    while let Some(child) = path.pop() {
        if let Some(parent) = path.last_mut() {
            parent.children.push(child);
        } else {
            roots.push(child);
        }
    }

    roots
}

pub fn projects_tree(flat_headings: Vec<Heading>) -> Vec<Heading> {
    build_heading_tree(flat_headings).into_iter().filter(|node| has_project_tag(&node.tags)).collect()
}

pub fn find_subtree_end_line(flat_headings: &[Heading], parent_idx: usize) -> usize {
    let parent = &flat_headings[parent_idx];
    let mut end_line = parent.last_line;

    let mut j = parent_idx + 1;
    while j < flat_headings.len() && flat_headings[j].level > parent.level {
        end_line = flat_headings[j].last_line;
        j += 1;
    }
    end_line
}

pub fn subtree_line_range(flat_headings: &[Heading], idx: usize) -> (usize, usize) {
    (flat_headings[idx].line_no, find_subtree_end_line(flat_headings, idx))
}

/// Checks if LINE is inside `line` [start_line, end_line] (inclusive)
/// Used to prevent Refile-in-own-Subtree
pub fn is_within_range(line: usize, start_line: usize, end_line: usize) -> bool {
    line >= start_line && line <= end_line
}

pub fn shift_line_no(line_no: usize, end_line: usize, removed_count: usize) -> usize {
    if line_no > end_line {
        line_no - removed_count
    } else {
        line_no
    }
}

pub fn compute_insert_index(
    flat: &[Heading],
    target_parent_line: Option<usize>,
    position: RefilePosition,
    total_lines: usize,
) -> Result<usize, String> {
    let Some(p_line) = target_parent_line else {
        return Ok(total_lines);
    };

    let p_idx =
        flat.iter().position(|h| h.line_no == p_line).ok_or_else(|| crate::core::errors::not_found_at_line("Ziel-Überschrift", p_line))?;

    Ok(match position {
        RefilePosition::Into | RefilePosition::After => find_subtree_end_line(flat, p_idx),
        RefilePosition::Before => flat[p_idx].line_no - 1,
    })
}

/// The chain of ancestor headings enclosing FLAT[IDX], nearest (immediate
/// parent) first, root last -- e.g. a level-3 subtask's chain is [level-2
/// parent, level-1 project]. Doesn't include FLAT[IDX] itself. Empty for a
/// top-level (level 1) heading.
pub fn ancestors_of(flat: &[Heading], idx: usize) -> Vec<Heading> {
    let mut out = Vec::new();
    let mut min_level = flat[idx].level;
    for h in flat[..idx].iter().rev() {
        if h.level < min_level {
            min_level = h.level;
            out.push(h.clone());
        }
    }
    out
}
