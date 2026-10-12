//! Bookmarks out of a Linkwarden backup (`backup.json`, from Linkwarden's
//! settings: "Export data").
//!
//! The backup is the user object, and under it every collection with its
//! links:
//!
//! ```json
//! { "collections": [ { "id": 1, "name": "Schule", "parentId": null,
//!     "description": "", "links": [ { "name": "…", "url": "https://…",
//!       "description": "", "metaDescription": null,
//!       "createdAt": "2025-08-25T18:09:25.759Z", "updatedAt": "…",
//!       "tags": [ { "name": "farbe" } ] } ] } ] }
//! ```
//!
//! What it becomes:
//!
//! - a collection is a heading, a sub-collection (`parentId`) a heading
//!   under it; a collection's own description is text under its heading;
//! - a link is a bookmark under its collection: `name` the title, `url`
//!   the `:URL:`, `createdAt` and `updatedAt` as `:CREATED:` and
//!   `:UPDATED:` in local time, tags as org tags, the user's own
//!   `description` the note, the page's `metaDescription` the quote;
//! - a link already saved -- here, or twice in the backup -- is skipped,
//!   and so is one without an address (an uploaded PDF or image: the file
//!   is not in the backup).
//!
//! Read leniently: every field may be missing, as across Linkwarden
//! versions some are, and fields this does not know are ignored.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Local};
use nobo_core::core::org_insert::insert_under;
use nobo_core::t;
use nobo_core::utils::note_utils::inactive_timestamp;
use serde::Deserialize;

use crate::model::Draft;
use crate::{url, write};

#[derive(Deserialize, Default)]
#[serde(default)]
struct Backup {
    collections: Vec<Collection>,
    #[serde(rename = "collectionOrder")]
    collection_order: Vec<i64>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Collection {
    id: i64,
    name: String,
    description: Option<String>,
    #[serde(rename = "parentId")]
    parent_id: Option<i64>,
    links: Vec<Link>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Link {
    name: Option<String>,
    url: Option<String>,
    description: Option<String>,
    #[serde(rename = "metaDescription")]
    meta_description: Option<String>,
    #[serde(rename = "createdAt")]
    created_at: Option<String>,
    #[serde(rename = "updatedAt")]
    updated_at: Option<String>,
    tags: Vec<Tag>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Tag {
    name: String,
}

/// What an import found, or did.
#[derive(Debug, Clone, PartialEq, Default, serde::Serialize)]
#[cfg_attr(feature = "tauri", derive(specta::Type))]
pub struct ImportReport {
    /// Links in the backup.
    #[cfg_attr(feature = "tauri", specta(type = specta_typescript::Number))]
    pub links: usize,
    /// Of those, the ones that are (or would be) saved.
    #[cfg_attr(feature = "tauri", specta(type = specta_typescript::Number))]
    pub imported: usize,
    /// Already a bookmark, or twice in the backup.
    #[cfg_attr(feature = "tauri", specta(type = specta_typescript::Number))]
    pub duplicates: usize,
    /// No address -- an uploaded file.
    #[cfg_attr(feature = "tauri", specta(type = specta_typescript::Number))]
    pub without_url: usize,
    #[cfg_attr(feature = "tauri", specta(type = specta_typescript::Number))]
    pub collections: usize,
    #[cfg_attr(feature = "tauri", specta(type = specta_typescript::Number))]
    pub tags: usize,
}

/// A collection to create: its heading path, and its own description.
type CollectionToCreate = (Vec<String>, Option<String>);

/// One bookmark to write, where it goes.
struct Planned {
    collection: Vec<String>,
    draft: Draft,
    created: String,
    updated: Option<String>,
}

/// Linkwarden's ISO time, `2025-08-25T18:09:25.759Z`, as an org timestamp
/// in local time -- what every other date in the vault is.
fn org_time(iso: Option<&str>) -> Option<String> {
    let parsed = DateTime::parse_from_rfc3339(iso?.trim()).ok()?;
    Some(inactive_timestamp(parsed.with_timezone(&Local)))
}

fn text(value: &Option<String>) -> String {
    value.as_deref().map(str::trim).unwrap_or("").to_string()
}

/// Reads JSON as a Linkwarden backup.
fn parse(json: &str) -> Result<Backup, String> {
    let backup: Backup = serde_json::from_str(json).map_err(|e| {
        let reason = e.to_string();
        t!("Das ist kein Linkwarden-Backup: {reason}", "That is not a Linkwarden backup: {reason}")
    })?;
    if backup.collections.is_empty() {
        return Err(t!(
            "In diesem Backup sind keine Sammlungen -- ist es ein Linkwarden-Backup (Einstellungen → Daten exportieren)?",
            "This backup has no collections -- is it a Linkwarden backup (Settings → Export data)?"
        ));
    }
    Ok(backup)
}

/// The collections in the order they are to be written: top-level ones in
/// the backup's own order (`collectionOrder`, the user's), every
/// collection followed by its sub-collections.
fn ordered(backup: &Backup) -> Vec<&Collection> {
    let by_id: HashMap<i64, &Collection> = backup.collections.iter().map(|c| (c.id, c)).collect();
    let rank = |id: i64| backup.collection_order.iter().position(|&o| o == id).unwrap_or(usize::MAX);
    let is_root = |c: &Collection| c.parent_id.is_none_or(|parent| !by_id.contains_key(&parent));
    let mut roots: Vec<&Collection> = backup.collections.iter().filter(|c| is_root(c)).collect();
    roots.sort_by_key(|c| (rank(c.id), c.id));

    let mut out = Vec::new();
    let mut seen = HashSet::new();
    fn visit<'a>(c: &'a Collection, all: &'a [Collection], seen: &mut HashSet<i64>, out: &mut Vec<&'a Collection>) {
        if !seen.insert(c.id) {
            return;
        }
        out.push(c);
        let mut children: Vec<&Collection> = all.iter().filter(|child| child.parent_id == Some(c.id)).collect();
        children.sort_by_key(|child| child.id);
        for child in children {
            visit(child, all, seen, out);
        }
    }
    for root in roots {
        visit(root, &backup.collections, &mut seen, &mut out);
    }
    // A cycle of parents (no root to start from) is still imported, flat.
    for c in &backup.collections {
        visit(c, &backup.collections, &mut seen, &mut out);
    }
    out
}

/// A collection's heading path: its name under its parents' names.
fn path_of(collection: &Collection, by_id: &HashMap<i64, &Collection>) -> Vec<String> {
    let mut path = vec![collection.name.trim().to_string()];
    let mut seen = HashSet::from([collection.id]);
    let mut parent = collection.parent_id;
    while let Some(id) = parent {
        let Some(up) = by_id.get(&id) else { break };
        if !seen.insert(id) {
            break;
        }
        path.insert(0, up.name.trim().to_string());
        parent = up.parent_id;
    }
    path
}

/// What JSON would import, given the addresses already saved (EXISTING,
/// normalized -- see url::normalize).
fn plan(backup: &Backup, existing: &HashSet<String>) -> (Vec<Planned>, Vec<CollectionToCreate>, ImportReport) {
    let by_id: HashMap<i64, &Collection> = backup.collections.iter().map(|c| (c.id, c)).collect();
    let mut seen = existing.clone();
    let mut planned = Vec::new();
    let mut collections = Vec::new();
    let mut tags = HashSet::new();
    let mut report = ImportReport { collections: backup.collections.len(), ..ImportReport::default() };

    for collection in ordered(backup) {
        let path = path_of(collection, &by_id);
        let description = text(&collection.description);
        collections.push((path.clone(), Some(description).filter(|d| !d.is_empty())));
        for link in &collection.links {
            report.links += 1;
            let address = text(&link.url);
            if !(address.starts_with("http://") || address.starts_with("https://")) {
                report.without_url += 1;
                continue;
            }
            if !seen.insert(url::normalize(&address)) {
                report.duplicates += 1;
                continue;
            }
            let link_tags: Vec<String> = link.tags.iter().filter_map(|tag| write::org_tag(&tag.name)).collect();
            tags.extend(link_tags.iter().cloned());
            let title = text(&link.name);
            planned.push(Planned {
                collection: path.clone(),
                draft: Draft {
                    title: if title.is_empty() { url::host(&address) } else { title },
                    url: address,
                    tags: link_tags,
                    read_later: false,
                    note: text(&link.description),
                    description: Some(text(&link.meta_description)).filter(|d| !d.is_empty()),
                },
                created: org_time(link.created_at.as_deref()).unwrap_or_else(|| inactive_timestamp(Local::now())),
                updated: org_time(link.updated_at.as_deref()).filter(|u| Some(u) != org_time(link.created_at.as_deref()).as_ref()),
            });
        }
    }
    report.imported = planned.len();
    report.tags = tags.len();
    (planned, collections, report)
}

/// What importing JSON into the bookmarks file TEXT would do -- nothing
/// is written.
pub fn preview(json: &str, existing: &HashSet<String>) -> Result<ImportReport, String> {
    let backup = parse(json)?;
    Ok(plan(&backup, existing).2)
}

/// TEXT (a bookmarks file) with everything JSON brings that is not there
/// yet, and what that was.
pub fn import(text: &str, json: &str, existing: &HashSet<String>) -> Result<(String, ImportReport), String> {
    let backup = parse(json)?;
    let (planned, collections, report) = plan(&backup, existing);
    let mut out = text.to_string();
    // Every collection first, in the backup's order and with its own
    // description -- also one whose links all live in sub-collections,
    // and one that is empty: the structure is part of what is imported.
    for (path, description) in &collections {
        out = write::ensure_collection(out, path, description.as_deref());
    }
    for item in &planned {
        out = insert_under(&out, &item.collection, |level| {
            write::entry_with(&item.draft, level, &item.created, item.updated.as_deref())
        })
        .0;
    }
    Ok((out, report))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::bookmarks_of;

    const BACKUP: &str = r##"{
        "uuid": "21fbdb4a", "name": "Dennis", "collectionOrder": [2, 1],
        "collections": [
            { "id": 1, "name": "Schule", "description": "", "parentId": null,
              "createdAt": "2025-06-10T08:33:15.313Z",
              "links": [
                { "id": 596, "name": "Making Software: What is a color space?", "type": "url",
                  "description": "", "metaDescription": null,
                  "url": "https://www.makingsoftware.com/chapters/color-spaces-models-and-gamuts",
                  "createdAt": "2025-08-25T18:09:25.759Z", "updatedAt": "2025-08-25T18:10:24.819Z",
                  "tags": [ { "id": 100, "name": "farbe" }, { "id": 101, "name": "web-dev" } ] },
                { "id": 597, "name": "Upload", "type": "pdf", "url": null, "tags": [] }
              ] },
            { "id": 2, "name": "Rust", "description": "Alles zu Rust", "parentId": null, "links": [] },
            { "id": 5, "name": "Async", "parentId": 2, "links": [
                { "id": 600, "name": "", "url": "https://tokio.rs/", "description": "Runtime.",
                  "metaDescription": "An asynchronous runtime.", "createdAt": "2025-09-01T10:00:00Z",
                  "updatedAt": "2025-09-01T10:00:00Z", "tags": [] },
                { "id": 601, "name": "tokio again", "url": "https://tokio.rs", "tags": [] }
            ] }
        ]
    }"##;

    const FILE: &str = "#+TITLE: Privat\n#+NOBO_TYPE: bookmarks\n";

    #[test]
    fn a_backup_becomes_collections_and_bookmarks() {
        let (out, report) = import(FILE, BACKUP, &HashSet::new()).unwrap();
        let all = bookmarks_of("/v/p.org", "Privat", &out);

        assert_eq!(report.links, 4);
        assert_eq!(report.imported, 2);
        assert_eq!(report.without_url, 1);
        assert_eq!(report.duplicates, 1, "tokio twice in the backup");
        assert_eq!(report.collections, 3);
        assert_eq!(all.len(), 2);

        let color = all.iter().find(|b| b.url.contains("makingsoftware")).unwrap();
        assert_eq!(color.title, "Making Software: What is a color space?");
        assert_eq!(color.collection, ["Schule"]);
        assert_eq!(color.tags, ["farbe", "web_dev"]);
        assert!(color.created.as_deref().unwrap().starts_with("[2025-08-25 Mo "), "{:?}", color.created);

        let tokio = all.iter().find(|b| b.url == "https://tokio.rs/").unwrap();
        assert_eq!(tokio.collection, ["Rust", "Async"], "a sub-collection under its parent");
        assert_eq!(tokio.title, "tokio.rs", "no name: the site");
        assert_eq!(tokio.note, "Runtime.");
        assert_eq!(tokio.description.as_deref(), Some("An asynchronous runtime."));
    }

    #[test]
    fn the_users_collection_order_is_kept_and_descriptions_carried_over() {
        let (out, _) = import(FILE, BACKUP, &HashSet::new()).unwrap();
        let rust = out.find("* Rust").unwrap();
        let schule = out.find("* Schule").unwrap();
        assert!(rust < schule, "collectionOrder puts Rust first:\n{out}");
        assert!(out.contains("* Rust\nAlles zu Rust\n"), "{out}");
        assert!(out.contains("** Async\n"), "{out}");
    }

    #[test]
    fn updated_is_kept_when_it_differs_from_created() {
        let (out, _) = import(FILE, BACKUP, &HashSet::new()).unwrap();
        let color = &out[out.find("Making Software").unwrap()..];
        assert!(color.contains(":UPDATED:"), "{out}");
        let tokio = &out[out.find(":URL:      https://tokio.rs/").unwrap()..];
        assert!(!tokio[..tokio.find(":END:").unwrap()].contains(":UPDATED:"), "same as created: left out");
    }

    #[test]
    fn what_is_already_saved_is_skipped() {
        let existing = HashSet::from([url::normalize("https://tokio.rs")]);
        let report = preview(BACKUP, &existing).unwrap();
        assert_eq!(report.imported, 1);
        assert_eq!(report.duplicates, 2);
    }

    #[test]
    fn importing_twice_adds_nothing_the_second_time() {
        let (once, _) = import(FILE, BACKUP, &HashSet::new()).unwrap();
        let existing: HashSet<String> = bookmarks_of("/v/p.org", "P", &once).iter().map(|b| url::normalize(&b.url)).collect();
        let (twice, report) = import(&once, BACKUP, &existing).unwrap();
        assert_eq!(report.imported, 0);
        assert_eq!(twice, once);
    }

    #[test]
    fn something_else_is_refused_with_a_reason() {
        assert!(preview("{\"foo\": 1}", &HashSet::new()).unwrap_err().contains("Linkwarden"));
        assert!(preview("kein json", &HashSet::new()).is_err());
    }
}
