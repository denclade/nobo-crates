# SPDX-FileCopyrightText: 2026 Dennis Burgermeister
# SPDX-License-Identifier: GPL-3.0-or-later

{
  description = "Standalone development environment for nobo's Rust libraries";

  inputs = {
    dev-sandbox.url = "github:denclade/nix-dev-sandbox";
    nixpkgs.follows = "dev-sandbox/nixpkgs";
  };

  outputs = { self, nixpkgs, dev-sandbox }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
      # Keep the native dependencies in one place for both shells.
      tauriLibraries = pkgs: with pkgs; [
        gtk3 webkitgtk_4_1 libsoup_3 librsvg libayatana-appindicator
        glib glib-networking openssl dbus zlib
      ];
      mkShell = system: withTauri:
        let pkgs = nixpkgs.legacyPackages.${system};
        in pkgs.mkShell {
          packages = with pkgs; [
            cargo rustc rustfmt clippy rust-analyzer
            mise python3 bash git pkg-config
          ];
          buildInputs = if withTauri then tauriLibraries pkgs else [ pkgs.openssl ];
          RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";
          shellHook = ''
            # Use the toolchain and Python pinned by flake.lock in Nix shells.
            export MISE_DISABLE_TOOLS="''${MISE_DISABLE_TOOLS:+$MISE_DISABLE_TOOLS,}rust,python"
          '';
        };
    in {
      devShells = forAllSystems (system: {
        default = mkShell system true;
        headless = mkShell system false;
      });

      # The shared base owns persistence, nix-ld and the firewall.
      # This VM gets its own loopback address so harbour-nobo can run alongside it.
      packages.x86_64-linux.default = dev-sandbox.lib.mkDevSandbox {
        name = "nobo-crates";
        hostname = "nobo-crates-vm";
        hostAddress = "127.0.10.2";
        sshPort = 2200;
        profiles = [ "tauri" ];
        extraEnv = {
          MISE_DISABLE_TOOLS = "rust,python";
          RUST_SRC_PATH = "${nixpkgs.legacyPackages.x86_64-linux.rustPlatform.rustLibSrc}";
          # Keep the first GTK/WebKit build within the base VM's 4 GiB RAM.
          CARGO_BUILD_JOBS = "2";
          CARGO_PROFILE_DEV_DEBUG = "0";
          CARGO_PROFILE_TEST_DEBUG = "0";
        };
      };
    };
}
