<!-- SPDX-FileCopyrightText: 2026 Dennis Burgermeister -->
<!-- SPDX-License-Identifier: GPL-3.0-or-later -->

# Public source preparation

Audited on 2026-10-24. Gitleaks and additional checks across historical
file contents, commit messages and tags found no production credentials,
private keys or tokens. Public author names and email addresses remain.
Example logins, local test servers and generic filesystem paths are test data.

## History cleanup

`git-filter-repo` 2.47.0 rewrote all local branches and tags, preserving all
68 commits, their parent relationships and the three existing release tags.
Internal repository addresses and personal home-directory paths were
replaced in file contents and commit/tag messages. The internal remote
was removed. No replacement public remote has been assumed.

The five recipe-import HTML fixtures now use synthetic markup and test
content throughout their history. Copied website prose, comments and user
profiles have been removed. Their parser regressions remain covered.
Org fixtures and the six small placeholder PNGs retain their original bytes.

Commit IDs and tag targets have changed. Existing clones must use the
rewritten history; merging or fetching an old branch into it can restore
removed content. Consumers must update their submodule pins. Historical
pins in consumer repositories still refer to the old history and require
separate migration before those repositories can be published.

A verified Git bundle, worktree archive, original configuration and ref
list were saved outside the repository before the rewrite. The rewrite
mapping is in Git's local `filter-repo/commit-map`; it is not published.
Backups contain the original private infrastructure references.

## Licensing

Source, configuration and documentation carry SPDX notices. `REUSE.toml`
covers Org/PNG fixtures and generated lockfiles without changing their
contents. The complete GPL text is in `LICENSE` and
`LICENSES/GPL-3.0-or-later.txt`. CI runs `reuse lint`.

## Validation

The standalone setup was previously checked with Rust 1.99.0 and Python
3.14.8: 1,539 headless tests, 1,546 tests with Tauri, both minimal sync
providers, 196 CalDAV tests in each of three timezones, seven binding
exports and API documentation with warnings rejected. Thirteen existing
documentation examples remain ignored. Clippy rejects correctness and
suspicious findings while reporting existing style warnings.

Nix evaluation covered both Linux development shells and the x86_64 VM
launcher; no VM boot or ARM runtime build was performed. GitHub CI has
not been run remotely. For this cleanup, REUSE coverage for all 742
relevant files, formatting, history integrity,
18 mise task definitions, CI YAML/shell syntax and secret scans of both
the history and current sources were checked. All 78 Cookbook tests passed
with the synthetic fixtures. The complete Tauri build was not repeated.

## Publication

Commit the remaining working-tree changes, provision the public repository
and verify an unauthenticated clone. Then update the consumer's submodule
URL and pin. Nothing has been pushed or uploaded to a package registry.
