#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 Dennis Burgermeister
# SPDX-License-Identifier: GPL-3.0-or-later

"""Check that every workspace crate can be used from this repository alone."""

import json
from pathlib import Path
import subprocess
import tomllib


def main():
    root = Path(__file__).resolve().parents[1]
    manifest = tomllib.loads((root / "Cargo.toml").read_text())
    version = manifest["workspace"]["package"]["version"]
    metadata = json.loads(subprocess.check_output(
        ["cargo", "metadata", "--locked", "--no-deps", "--format-version", "1"],
        cwd=root,
        text=True,
    ))
    errors = []
    if Path(metadata["workspace_root"]).resolve() != root:
        errors.append("Cargo selected a workspace outside this repository")
    for package in metadata["packages"]:
        name = package["name"]
        if package["version"] != version:
            errors.append(f"{name}: version differs from workspace.package.version")
        if package["license"] != "GPL-3.0-or-later":
            errors.append(f"{name}: missing or unexpected license declaration")
        paths = [("manifest", package["manifest_path"])]
        paths.extend((dep["name"], dep["path"]) for dep in package["dependencies"] if dep.get("path"))
        for label, path in paths:
            if not Path(path).resolve().is_relative_to(root):
                errors.append(f"{name}: {label} points outside this repository")
    for filename in ("Cargo.lock", "LICENSE", "README.md", "rust-toolchain.toml", "flake.lock"):
        if not (root / filename).is_file():
            errors.append(f"Missing {filename}")
    if errors:
        raise SystemExit("\n".join(errors))
    print(f"{len(metadata['packages'])} standalone crates, version {version}, license and local dependencies checked")


if __name__ == "__main__":
    main()
