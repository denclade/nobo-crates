<!-- SPDX-FileCopyrightText: 2026 Dennis Burgermeister -->
<!-- SPDX-License-Identifier: GPL-3.0-or-later -->

# Eigenständige Git-Releases

Alle 13 Crates verwenden `workspace.package.version` aus `Cargo.toml`.
Diese Anleitung erzeugt ein Git-Release des Workspaces. Ein automatischer
Upload der einzelnen Crates in eine Package-Registry ist nicht eingerichtet.

## Vorbereiten

1. `mise run verify:tauri`, `mise run doc` und `mise run audit:secrets`
   müssen durchlaufen.
2. `CHANGELOG.md` vervollständigen. Vor 1.0 erhöhen Änderungen, an die
   Hosts angepasst werden müssen, die Minor-Version; Korrekturen die
   Patch-Version.
3. `workspace.package.version` anpassen und `cargo check --workspace
   --no-default-features --features nobo-sync/webdav` ausführen, damit
   `Cargo.lock` die Versionen der Workspace-Crates nachzieht. Danach die
   Checks mit `--locked` erneut ausführen.
4. Version, Lockfile und Changelog gemeinsam committen. Der Arbeitsbaum
   muss sauber sein; produktive Zugangsdaten gehören nicht in das Release.
5. Einen neuen annotierten Tag `vX.Y.Z` erstellen. Bereits veröffentlichte
   Tags bleiben unverändert. Das Pushen und die Freigabe erfolgen bewusst
   erst nach Review; lokale Tasks pushen nichts.

Es gibt keine Abhängigkeit mehr von `pnpm release:crates` oder einem
Release-Skript in einem benachbarten App-Repository.

## Hosts aktualisieren

Nach der öffentlichen Bereitstellung können Apps ihren Submodul-Zeiger
auf den freigegebenen Commit bzw. Tag setzen. Command-Änderungen und
explizite Binding-Exporte gehören in die jeweilige Host-Anpassung.
Die öffentliche Clone-URL erst übernehmen, sobald das Repository unter
dieser Adresse tatsächlich erreichbar ist.
