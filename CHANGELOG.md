# Changelog

Alle nennenswerten Änderungen an den Kisten, nach Version. Alle Kisten
dieses Workspace tragen eine gemeinsame Version und werden zusammen
veröffentlicht. Format nach
[Keep a Changelog](https://keepachangelog.com/de/1.1.0/), Versionen nach
[Semantic Versioning](https://semver.org/lang/de/).

Veröffentlicht wird aus der App heraus (`pnpm release:crates`, siehe
docs/releasing.md im App-Repository). Vor 1.0 gilt: Eine Änderung, die
Hosts (die App, harbour-nobo) anpassen müssen, erhöht die zweite Stelle;
die dritte bleibt für Korrekturen.

## [Unreleased]

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

