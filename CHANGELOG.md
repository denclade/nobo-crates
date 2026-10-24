# Changelog

Alle nennenswerten Änderungen an den Kisten, nach Version. Alle Kisten
dieses Workspace tragen eine gemeinsame Version und werden zusammen
veröffentlicht. Format nach
[Keep a Changelog](https://keepachangelog.com/de/1.1.0/), Versionen nach
[Semantic Versioning](https://semver.org/lang/de/).

Eigenständige Git-Releases sind in [docs/releasing.md](docs/releasing.md)
beschrieben. Vor 1.0 gilt: Eine Änderung, die
Hosts (die App, harbour-nobo) anpassen müssen, erhöht die zweite Stelle;
die dritte bleibt für Korrekturen.

## [Unreleased]

### Behoben

- CalDAV sortiert Erstellungszeiten auf derselben UTC-Skala wie explizite
  Sortierwerte; die bisherigen Regressionen laufen auch außerhalb von UTC.

### Geändert

- Eigenständige Entwicklung mit mise-Tasks, gepinnter Rust-Toolchain,
  Nix-DevShells, optionaler MicroVM und CI; Lizenztext und Release-Anleitung ergänzt.
- Tauri-Binding-Tests schreiben standardmäßig in temporäre Verzeichnisse.
  Hosts exportieren dauerhaft mit `mise run bindings:export -- <Ziel>`
  statt über feste Pfade in ein benachbartes App-Repository.
- Die API-Dokumentation löst interne und externe Verweise ohne Rustdoc-Warnungen auf.
- Interne Projektadresse und persönliche absolute Beispielpfade aus dem
  aktuellen Quellstand entfernt.

## [0.18.0] – 2026-10-13

### Neu

- Kochbuch: `import_known_site` / `known_site` – ob eine Adresse eine bekannte Rezeptseite ist (Chefkoch, Cookidoo), ohne etwas abzurufen; als Befehl `cookbook_import_known_site`.

## [0.17.0] – 2026-10-13

### Geändert (Hosts passen an)

- Befehle tragen den Namen ihres Plugins: `babel_discover_bash`, `babel_discover_python_interpreters`, `babel_execute_block`, `babel_kill_session` (vorher ohne `babel_`-Präfix bzw. `execute_babel_block`/`kill_babel_session`), `cookbook_set_file` (vorher `set_cookbook`, nimmt jetzt die Datenbank für den Verlauf), `review_state`, `review_save_state`, `review_save_settings` (vorher `get_review_state`, `save_review_state`, `save_review_settings`).
- `bookmarks_set_file` und `cookbook_set_file` schreiben über `vault_write` und landen im Verlauf.
- Lesezeichen und Kochbuch registrieren alle Befehle in jedem Build; ohne Netzwerk antworten Seitenabruf und Rezept-Import mit einem Grund.

### Neu

- `core::vault_write`: ein Schreibweg für alles, was im Verlauf landet (`edit_file`, `edit_files`, `recorded`), eine Sperre je Datei (wiedereintrittsfähig), `write_generated` für Spiegel, `declare_kind`, `app_dir`.
- `org_syntax`: gemeinsames Lesen und Schreiben von Property-Drawern im Format von Org; `Vault::kind_candidates` für die Dateiauswahl der Features.

### Geändert

- Alle Schreibstellen (Überschriften, Uhr, Notiz löschen, Checkboxen, Kochbuch, CalDAV-Spiegel) laufen über `vault_write`.
- `commands_headings` und `fs_utils` nach Aufgaben aufgeteilt; öffentliche Pfade unverändert.
- Einheitlich mit `rustfmt` formatiert (`rustfmt.toml`); `.git-blame-ignore-revs` überspringt den Formatierungs-Commit.

## [0.16.0] – 2026-10-12

### Behoben

- Org is read and written as org-mode does

## [0.15.0] – 2026-10-12

### Behoben

- Bookmarks: a robust format, safe concurrent edits, no duplicates

## [0.14.0] – 2026-10-12

### Neu

- Bookmarks: collections carry their heading's line

## [0.13.1] – 2026-10-12

### Geändert

- Aufgeräumt: Testmodul in `nobo-bookmarks` ans Dateiende (Clippy), ohne Verhaltensänderung.

## [0.13.0] – 2026-10-12

### Neu

- Lesezeichen: neue Kategorien liegen als `.nobo/bookmarks-<name>.org`; `bookmarks_collections` liefert alle Sammlungen, leere eingeschlossen; Speichern und Verschieben legen fehlende Sammlungen an. Beispieldaten im Testtresor (Work, Private).

## [0.12.0] – 2026-10-12

### Neu

- `nobo-bookmarks`: Lesezeichen als Org-Überschriften -- eine Lesezeichen-Datei (`#+NOBO_TYPE: bookmarks`) je Kategorie, Überschriften darüber als Sammlungen, Tags, `:readlater:`, `:CREATED:`, Notiz und Seitenbeschreibung. Lesen wie Org, Schreiben je Lesezeichen (anlegen, ändern, verschieben, löschen), jede Änderung ein Verlaufseintrag; Vorschau einer Seite (Titel, Beschreibung); Suche; Import eines Linkwarden-Backups.
- `core::utils::html` (die HTML-Helfer des Rezept-Imports), `nobo_http::fetch` (Seitenabruf mit Größengrenze, neu mit Zeichensatz-Erkennung), `core::org_insert` (Eintrag unter einen Überschriftenpfad), `note_utils::inactive_timestamp`, `vault::format_heading_line`.

## [0.11.0] – 2026-10-07

### Neu

- Nummerierte Listen mit Klammer (`1)`, `a)`) sind Listen wie `1.` und `a.`: `org_ast::list_of` erkennt sie, Fortsetzen und Sortieren behalten die Klammer bei, `BulletKind::NumberedParen` stellt eine Liste darauf um.

## [0.10.0] – 2026-10-07

### Neu

- `core::org_sort`: sortiert, worin der Cursor steht, wie `org-sort` – die Zeilen einer Tabelle nach einer Spalte (zwischen zwei Trennlinien, nie die Kopfzeile), die Punkte einer Liste samt Unterpunkten (Nummern werden neu vergeben), die Unterüberschriften eines Eintrags samt Teilbäumen. `target` sagt, was sortiert würde und welche Sortierarten passen (alphabetisch, Zahl, Datum, Checkbox, Status, Priorität); `sort` führt es aus.
- `org_edit::sort`, `table_at`, `replace_table`; `org_table::set_cell`.
- `nobo-app::commands_org::sort_target` und `sort_text` für Editoren; die Typen dafür sind für Tauri-Bindings exportierbar.

## [0.9.0] – 2026-10-06

### Neu

- Erste versionierte Fassung. Alles davor steht in der Git-Historie, nicht hier.
- `nobo-core`: Org-Dateien, Index, Suche, Verlauf, Datei-Arten, Merge, Secrets, Einstellungen.
- `nobo-app`: die Befehle der App, ohne Host.
- Feature-Kisten `nobo-babel`, `nobo-calendar`, `nobo-caldav`, `nobo-cookbook`, `nobo-review`, `nobo-sync`, jeweils mit optionalem Tauri-Teil.
- Geteilte Bibliotheken `nobo-http`, `nobo-webdav`, `nobo-ical`, `nobo-calendar-mirror`.

