<!-- SPDX-FileCopyrightText: 2026 Dennis Burgermeister -->
<!-- SPDX-License-Identifier: GPL-3.0-or-later -->

# nobo-crates

Shared Rust libraries for nobo: Org files, tasks, calendars, recipes,
bookmarks and encrypted vault sync. A standalone Cargo workspace with 13
crates; no application checkout or live accounts required.

- `nobo-core`: Org editing, indexing, search, history, settings and storage.
- `nobo-app`: shared application commands.
- `nobo-http`, `nobo-webdav`, `nobo-ical`, `nobo-calendar-mirror`: protocol libraries.
- `nobo-babel`, `nobo-bookmarks`, `nobo-calendar`, `nobo-caldav`, `nobo-cookbook`,
  `nobo-review`, `nobo-sync`: features with optional Tauri adapters.

## Build and test

Install [mise](https://mise.jdx.dev/), a C compiler/linker, Bash and
`pkg-config`. Rust is pinned in `rust-toolchain.toml`; Python in `mise.toml`.

```sh
mise trust
mise install
mise run build
mise run test
mise run verify
```

These tasks disable Tauri and enable both sync providers and network import
features. `verify` also checks formatting, Clippy, minimal features and three
CalDAV timezones. Tests use temporary files and local HTTP servers.
Use `mise tasks` to list all tasks and `mise run doc` for API documentation.

## Nix and Tauri

Nix supplies pinned tools and native dependencies on x86_64/aarch64 Linux;
`mise install` is unnecessary inside these shells.

```sh
nix develop .#headless
mise run verify

nix develop
mise run verify:tauri
mise run bindings:export                    # target/bindings/<feature>/bindings.ts
mise run bindings:export -- /path/to/app/src/features
```

Outside Nix, Tauri tasks need the [native prerequisites](https://v2.tauri.app/start/prerequisites/).
Default tests export bindings to temporary directories. Headless and Tauri
tasks use separate build caches. With defaults disabled, `nobo-sync`
requires `webdav` or `pocketbase`.

An optional x86_64 MicroVM uses the public
[nix-dev-sandbox](https://github.com/denclade/nix-dev-sandbox), including its
firewall: `mise run vm`. See [development setup](docs/development.md) for
VM access and host integration.

## Contributing and license

See [CONTRIBUTING.md](CONTRIBUTING.md), [releases](docs/releasing.md) and the
[publication audit](docs/publication.md). Source files carry SPDX notices;
`REUSE.toml` covers fixtures and generated lockfiles. Check with `reuse lint`.

Copyright 2026 Dennis Burgermeister. [GPL-3.0-or-later](LICENSE).
