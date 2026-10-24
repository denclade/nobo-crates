# nobo-crates

Die gemeinsamen Rust-Bibliotheken von nobo: Org-Dateien, Aufgaben,
Kalender, Rezepte, Lesezeichen und verschlüsselter Vault-Sync. Ein eigener
Cargo-Workspace, der unabhängig von einer App gebaut und getestet werden
kann. Desktop- und Sailfish-Hosts binden dieselben Bibliotheken ein.

| Kiste | Inhalt |
|---|---|
| `nobo-core` | Org-Dateien, Headings, Index, Suche, History, Einstellungen, Merge, Secrets, DB und Uhr |
| `nobo-app` | gemeinsame Command-Bodies ohne Host-Verpackung |
| `nobo-http`, `nobo-webdav`, `nobo-ical`, `nobo-calendar-mirror` | gemeinsame Protokoll- und Formatbibliotheken |
| `nobo-babel`, `nobo-bookmarks`, `nobo-calendar`, `nobo-caldav`, `nobo-cookbook`, `nobo-review`, `nobo-sync` | optionale Features und ihre Tauri-Adapter |
| `testdata/` | Beispieltresor und Test-Fixtures; keine Live-Konten erforderlich |

## Mit mise auf dem Host

Linux braucht einen C-Compiler samt Linker und `pkg-config`; die
Babel-Tests führen außerdem Bash und Python aus. Rust einschließlich
Clippy/rustfmt kommt aus `rust-toolchain.toml`, Python aus `mise.toml`:

```sh
mise trust
mise install
mise tasks
mise run build
mise run test
mise run verify
```

Die normalen Tasks bauen alle 13 Crates ohne Tauri, mit beiden
Sync-Providern und den Import-/Fetch-Features. Tests verwenden temporäre
Verzeichnisse und lokale HTTP-Testserver; sie brauchen keine produktiven
Zugangsdaten oder laufende nobo-App. `mise run verify` prüft zusätzlich
Formatierung, Clippy, minimale Features und CalDAV in mehreren Zeitzonen.

Direkt mit Cargo ist derselbe Build möglich:

```sh
cargo build --locked --workspace --no-default-features \
  --features nobo-sync/pocketbase,nobo-sync/webdav,nobo-cookbook/import,nobo-bookmarks/fetch
cargo test --locked --workspace --no-default-features \
  --features nobo-sync/pocketbase,nobo-sync/webdav,nobo-cookbook/import,nobo-bookmarks/fetch
```

## Mit Nix

`flake.lock` pinnt das Toolset und die nativen Bibliotheken. Die
Entwicklungs-Shells unterstützen `x86_64-linux` und `aarch64-linux`:

```sh
nix develop                  # Rust, mise und native Tauri-Abhängigkeiten
mise run verify:tauri

nix develop .#headless       # kleinere Shell ohne GTK/WebKit
mise run verify
```

Innerhalb der Nix-Shell verwendet mise das von Nix gelieferte Rust und
Python. `mise install` ist dort nicht zusätzlich nötig. Cargo lädt seine
im eingecheckten `Cargo.lock` festgelegten Bibliotheken beim ersten Build.

Die optionale x86_64-MicroVM (`nix run .` oder `mise run vm`) benutzt die
öffentliche [nix-dev-sandbox](https://github.com/denclade/nix-dev-sandbox).
Einrichtung und SSH-Zugang stehen in [docs/development.md](docs/development.md).
Die Firewall bleibt Teil der gemeinsamen Basis-Konfiguration.

## Tauri und TypeScript-Bindings

`nobo-core` ist unabhängig von Tauri. Hosts stellen `IndexListener`,
`KeychainBackend` und die Konfigurationsablage bereit. Die Feature-Crates
halten ihre Tauri-Adapter hinter `tauri`; das Feature bleibt für
bestehende Desktop-Hosts standardmäßig eingeschaltet.

```sh
mise run tauri:check
mise run tauri:build
mise run tauri:test
mise run bindings:export                 # target/bindings/<feature>/bindings.ts
mise run bindings:export -- /pfad/zur/app/src/features
```

Diese Tasks brauchen die [nativen Tauri-Abhängigkeiten](https://v2.tauri.app/start/prerequisites/)
oder die oben genannte Nix-Shell. Normale Binding-Tests exportieren in
temporäre Verzeichnisse. Nur ein expliziter `bindings:export`-Aufruf
schreibt in ein dauerhaftes Ziel; es gibt keine festen Pfade in ein
übergeordnetes App-Repository mehr.

`nobo-sync` verlangt mindestens einen Provider (`webdav` oder
`pocketbase`), wenn Default-Features abgeschaltet werden.
`mise run check:minimal` prüft beide Varianten einzeln.

## Beitragen und Releases

[CONTRIBUTING.md](CONTRIBUTING.md) beschreibt die lokalen Checks und die CI.
[docs/releasing.md](docs/releasing.md) beschreibt eigenständige Git-Releases.
Die Vorbereitung der öffentlichen Quellen ist in
[docs/publication.md](docs/publication.md) dokumentiert.

## Lizenz

GPL-3.0-or-later; der vollständige Lizenztext liegt in [LICENSE](LICENSE).
