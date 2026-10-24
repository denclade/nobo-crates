<!-- SPDX-FileCopyrightText: 2026 Dennis Burgermeister -->
<!-- SPDX-License-Identifier: GPL-3.0-or-later -->

# Beitragen

Im Root dieses Workspaces arbeiten; keine App oder Submodule sind nötig.
`mise install` installiert die deklarierte Toolchain. Alternativ liefert
`nix develop` Rust und die nativen Linux-Abhängigkeiten.

Vor einer Änderung `mise run test`, vor einem Review `mise run verify`
ausführen. Änderungen an Tauri-Adaptern oder Command-Signaturen zusätzlich
mit `mise run verify:tauri` prüfen. Die CI führt beide Varianten aus. Clippy blockiert Korrektheits- und
Verdachtsbefunde; bestehende Stil-/Strukturhinweise werden weiterhin
ausgegeben. Änderungen an diesen öffentlichen APIs und Datenformen
gehören in gezielte Folgeänderungen.

- Formatierung: `mise run fmt`; Kontrolle: `mise run fmt:check`.
- Einzelnes Crate: `cargo test --locked -p nobo-core`.
- CalDAV-Zeitzonen: `mise run test:timezones`.
- Secrets/Historie: `mise run audit:secrets` (installiert Gitleaks nur für diesen Task).
- API-Dokumentation: `mise run doc`.
- Bindings für einen Host: `mise run bindings:export -- /absolutes/ziel`.

Testdaten müssen veröffentlichbare Beispiele bleiben. Keine persönlichen
Vaults, Konten, Tokens, Schlüssel oder private Infrastruktur hinzufügen.
Neue Regressionen sollen mit kleinen lokalen Fixtures auskommen.
`.env`, lokale mise-Overrides, VM-Zustand und `target/` sind ignoriert.

Neue Quellen und Konfigurationen erhalten `SPDX-FileCopyrightText` und
`SPDX-License-Identifier` für GPL-3.0-or-later mit der jeweiligen Urheberschaft.
Fixtures und generierte Lockfiles werden über `REUSE.toml` erfasst;
`reuse lint` prüft die Abdeckung, auch in der CI. HTML-Import-Fixtures sind
synthetische Beispiele, keine kopierten Rezeptseiten.

Alle Crates teilen die Version in `Cargo.toml`. Versionswechsel und
Changelog gehören zusammen; siehe [Releases](docs/releasing.md).
