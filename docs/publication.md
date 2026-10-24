# Vorbereitung der öffentlichen Quellen

Geprüfter historischer Ausgangsstand: `c9041bf` am 24. Oktober 2026.
Die lokal vorhandene Historie umfasst 65 Commits; alle lokal verfügbaren
Branches und Tags und alle von harbour-nobo referenzierten 24 Versionen
liegen darin. Die Prüfung mit Gitleaks 8.30.1 und ergänzender Suche über
alle erreichbaren Dateiobjekte ergab keine echten Schlüssel, Tokens oder
produktiven Zugangsdaten.

Die zusätzlichen Treffer waren öffentliche Autorenangaben, generische
Demo-Inhalte, Beispiel-Logins und persönliche Beispielpfade. Im aktuellen
Stand sind die interne Projekt-URL im README und die persönlichen
absoluten Pfade aus den Rust-Tests entfernt. Der öffentliche Nix-Input
und seine transitiven Quellen sind im Lockfile festgelegt.

Autorenname und E-Mail bleiben als Urheber-/Commitangaben erhalten.
`testdata/` ist ein umfangreicher Demo-Bestand; vor neuen Ergänzungen
immer sicherstellen, dass Inhalte und Bilder veröffentlicht werden dürfen.
Die sechs PNG-Testanhänge sind 64×64-Platzhalter. Es wurden darin keine
privaten Foto- oder Standortmetadaten gefunden.

Die Historie wurde nicht umgeschrieben. Frühere Commits enthalten weiterhin
eine interne Projekt-URL und persönliche Beispielpfade. Wer die vollständige
Historie veröffentlicht, veröffentlicht auch diese älteren Angaben.

## Vor der tatsächlichen Freigabe

- Lokale Build-/Test- und Tauri-Prüfungen ausführen; die CI ist als
  GitHub-Workflow vorbereitet und läuft erst nach Bereitstellung dort.
- Änderungen einschließlich `LICENSE`, Entwicklungs-Konfiguration und
  Dokumentation reviewen und committen.
- Repository unter der vorgesehenen öffentlichen Adresse bereitstellen
  und ohne Zugangsdaten klonen können.
- Erst danach die Submodul-URL im konsumierenden Projekt umstellen.

Es wurde kein Repository angelegt, keine Remote-Adresse umgestellt und
nichts gepusht, getaggt oder auf einer Package-Registry veröffentlicht.

## Lokale Validierung dieser Vorbereitung

- 18 eigene mise-Tasks validiert; auch im Submodul werden keine Tasks der
  übergeordneten App geladen. Getrennte Build-Caches und explizite
  `CARGO_TARGET_DIR`-Overrides sind einschließlich Leerzeichen geprüft.
- 13 Workspace-Crates: gemeinsame Version 0.18.0, Lizenzdeklarationen und
  sämtliche lokalen Cargo-Abhängigkeiten innerhalb des Repositorys geprüft.
- Ohne Tauri: 1.539 Tests erfolgreich. Der gesamte `verify`-Task lief aus
  einer separaten Quellkopie ohne App-Nachbarverzeichnisse durch.
- Mit Tauri: 1.546 Tests erfolgreich. In beiden Varianten bleiben die
  bereits vorhandenen 13 ignorierten Doku-Beispiele unverändert ignoriert.
- Beide minimalen Sync-Provider-Konfigurationen geprüft; zusätzlich jeweils
  196 CalDAV-Tests in UTC, Europe/Berlin und America/New_York erfolgreich.
- Alle sieben Binding-Exporte mit den zuvor kompilierten Testprogrammen
  ausgeführt; vollständige, nicht leere Ausgaben in einem Ziel mit Leerzeichen.
- API-Dokumentation aller 13 Crates mit `-D warnings` erfolgreich erzeugt;
  unauflösbare und private Rustdoc-Verweise korrigiert.
- Clippy besteht mit der dokumentierten Fehlerschwelle für Korrektheits-
  und Verdachtsbefunde. Vorhandene Stil-/Strukturhinweise bleiben sichtbar;
  diese Vorbereitung beansprucht keinen vollständig warnungsfreien Bestand.
- Nix-Auswertung mit `flake check --all-systems --no-build` erfolgreich für
  beide Linux-Shells und den x86_64-VM-Launcher. Keine VM gestartet und
  kein tatsächlicher ARM-Build ausgeführt.
- CI-YAML, Shell-Schritte und auf offizielle Commits gepinnte Actions lokal
  geprüft. Der Workflow wurde noch nicht auf GitHub ausgeführt.

Die lokalen Cargo-/mise-Läufe nutzten Rust 1.99.0 und Python 3.14.8 aus
Nix; Compiler und Python wurden dabei nicht erneut durch mise installiert.
