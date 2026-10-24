<!-- SPDX-FileCopyrightText: 2026 Dennis Burgermeister -->
<!-- SPDX-License-Identifier: GPL-3.0-or-later -->

# Eigenständige Entwicklung

Der Workspace benötigt keine Nachbarverzeichnisse, kein Sailfish SDK und
kein Frontend. Einstieg und Task-Liste stehen im [README](../README.md).
`.miserc.toml` begrenzt die Projekt-Konfigurationssuche auf dieses
Repository. Auch als Submodul übernimmt mise keine Tasks oder
SDK-Variablen aus der übergeordneten App-Konfiguration.

## Nix-Shell und Compiler

`rust-toolchain.toml` pinnt die mit mise/rustup verwendete Toolchain.
`flake.lock` pinnt Nixpkgs und damit auch dessen Rust-Pakete. Bei einem
Toolchain-Update beide Varianten prüfen; die CI nutzt die deklarierte
mise-Toolchain. Die Nix-Shell setzt `MISE_DISABLE_TOOLS` für Rust/Python,
damit mise diese bereits bereitgestellten Werkzeuge verwendet.

Das Standard-DevShell enthält GTK, WebKitGTK, libsoup, D-Bus, OpenSSL und
die übrigen nativen Tauri-Bibliotheken. `.#headless` lässt die
GUI-Bibliotheken weg. Es gibt hier keine startbare Desktop-App;
`tauri:build` baut die Bibliotheken und ihre Adapter.

## Isolierte MicroVM

Die öffentliche Basis-Flake liefert das Profil `tauri` samt Rust,
GTK/WebKit, mise, Python und allgemeinen CLI-Werkzeugen. Ihre Firewall,
Persistenz und `nix-ld` werden unverändert übernommen.

Auf einem Linux-x86_64-Host mit Nix und KVM:

1. `.env.template` nach `.env` kopieren und den **öffentlichen** SSH-Key
   sowie die gewünschte Git-Identität eintragen.
2. Im eigenständigen Checkout `nix run .` ausführen.
3. Auf dem Host folgenden SSH-Eintrag anlegen:

```sshconfig
Host nobo-crates-vm
    HostName 127.0.10.2
    Port 2200
    User dev
    IdentityFile ~/.ssh/id_ed25519
```

Dann `ssh nobo-crates-vm` und im Workspace `mise run verify:tauri`.
Die Adresse ist von der harbour-nobo-VM getrennt; beide können parallel
laufen. Der Launcher nutzt das aktuelle Projektverzeichnis als Workspace.
`.microvm-state/` und `.env` bleiben lokal; SSH-Privatschlüssel werden
nicht im Repository abgelegt.

Die 4-GiB-VM verwendet zwei Cargo-Build-Jobs und lässt Debug-Informationen
in Dev-/Test-Artefakten weg. Für einen Debugger lassen sich
`CARGO_PROFILE_DEV_DEBUG` bzw. `CARGO_PROFILE_TEST_DEBUG` im jeweiligen
Aufruf auf `1` oder `2` setzen. Die CI verwendet dieselben sparsamen
Profileinstellungen.

Für die VM ein eigenes Checkout verwenden: Als Submodul zeigt `.git` auf
das übergeordnete Repository, dessen Metadaten nicht Teil des separaten
Workspace-Mounts sind. Ein normales Clone ist vollständig eigenständig.

## Build-Caches

mise trennt `target/headless` und `target/tauri`, damit ein Wechsel
zwischen Clippy/Tests ohne Tauri und Binding-Exporten mit Tauri nicht den
jeweils anderen Featuregraphen neu bauen muss. Ein explizit gesetztes
`CARGO_TARGET_DIR` hat Vorrang, etwa für einen vorhandenen gemeinsamen
Cache. In der MicroVM liegen beide Verzeichnisse auf dem persistenten
`target/`-Volume.

## Testdaten und Netzwerk

`testdata/` enthält den Demo-Vault; Tests lesen ihn relativ zum jeweiligen
Crate. Verändernde Tests kopieren Daten in temporäre Verzeichnisse.
HTTP-/WebDAV-/CalDAV-Tests verwenden lokale Testserver. Die Sandbox muss
Loopback-Sockets und das Starten von Bash/Python erlauben.

Externe Downloads für Cargo oder mise unterliegen den Netzwerkregeln der
gewählten Umgebung. Neue Tools können zusätzlich über mise oder uv
installiert werden. Ein nur lesbar eingebundener Nix-Store genügt nicht
für beliebige neue Pakete über `nix shell` innerhalb der VM.
