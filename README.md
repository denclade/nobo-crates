# nobo's Rust-Kisten

Alles, woraus nobos Apps gebaut sind, und nichts, was eine App ist.

Ein eigener Cargo-Workspace, und dafür gedacht, ein eigenes Repository zu
werden: der Sailfish-OS-Port ([harbour-nobo](https://example.org/harbour-nobo))
linkt dieselben Kisten unter einer Qt/Silica-Oberfläche und hat für die
Tauri-App darum herum keine Verwendung. Beide Projekte binden das hier als
Submodul ein.

## Was drin liegt

| Kiste | Was |
|---|---|
| `nobo-core` | Org-Dateien, Headings, Index, Suche, History, Einstellungen, Datei-Arten, Merge, Secrets, DB, Uhr |
| `nobo-app` | die Command-Bodies, ohne die Verpackung irgendeines Hosts |
| `nobo-http`, `nobo-webdav`, `nobo-ical`, `nobo-calendar-mirror` | die geteilten Bibliotheken |
| `nobo-babel`, `nobo-calendar`, `nobo-caldav`, `nobo-cookbook`, `nobo-review`, `nobo-sync` | die Feature-Kisten |
| `testdata/` | der Beispieltresor, den die Tests lesen (`../testdata` aus einer Kiste heraus) |

## Tauri ist optional, überall

`nobo-core` weiß nicht, was Tauri ist -- `cargo tree -p nobo-core` zeigt
null Tauri-Pakete. Was ein Host beisteuern muss, geht durch Traits, die er
ausfüllt: `IndexListener`, `KeychainBackend`, `set_config_dir`.

Die Feature-Kisten sind Tauri-**Plugins**, und das bleiben sie: ihre
`commands.rs` sind schlichte Funktionen, ihre `tauri_commands.rs` legen
hinter dem Feature `tauri` Tauris Verpackung darum. Das Feature ist
standardmäßig an, damit der App-Build unverändert ist:

```sh
cargo check --workspace                     # mit Tauri, wie die App es baut
cargo check --workspace --no-default-features --features nobo-sync/webdav
```

Die zweite Zeile ist der Host-freie Pfad -- das, was harbour-nobo
kompiliert. `nobo-sync` braucht dabei einen Provider genannt, weil es sich
ohne absichtlich weigert zu bauen.

**Nichts hier zeigt aus diesem Verzeichnis heraus.** Jede Tauri-Abhängigkeit
kommt von crates.io; lokale Tauri-Plugins und die Tauri-Antworten auf die
Nähte des Kerns (`nobo-tauri`) liegen bei der App, nicht hier.

Eine Ausnahme mit Ansage: die `bindings_export`-Tests schreiben
`../../src/features/*/bindings.ts`, also ins Frontend der Tauri-App. Sie
hängen am Feature `tauri` und existieren im Host-freien Build gar nicht --
ein `cargo test` mit Default-Features außerhalb von nobos Baum läuft
darum in einen Schreibfehler.

## Tests

```sh
cargo test --workspace --no-fail-fast
```

Zwei Tests in `nobo-caldav` sind zeitzonenabhängig und scheitern außerhalb
von UTC (`sync::tests::the_order_matches_what_nextcloud_and_tasks_org_show`,
`vtodo::tests::the_sort_key_falls_back_to_created_because_only_dragging_writes_the_property`).
Bekannt und älter als dieser Workspace.

## Lizenz

GPL-3.0-or-later.
