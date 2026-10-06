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

### Neu

- Erste versionierte Fassung. Alles davor steht in der Git-Historie, nicht hier.
- `nobo-core`: Org-Dateien, Index, Suche, Verlauf, Datei-Arten, Merge, Secrets, Einstellungen.
- `nobo-app`: die Befehle der App, ohne Host.
- Feature-Kisten `nobo-babel`, `nobo-calendar`, `nobo-caldav`, `nobo-cookbook`, `nobo-review`, `nobo-sync`, jeweils mit optionalem Tauri-Teil.
- Geteilte Bibliotheken `nobo-http`, `nobo-webdav`, `nobo-ical`, `nobo-calendar-mirror`.
