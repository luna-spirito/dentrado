{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-parts.url = "github:hercules-ci/flake-parts";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    crane.url = "github:ipetkov/crane";
    evakuilo = {
      url = "github:luna-spirito/wikidot-evakuilo";
      inputs.rust-overlay.follows = "rust-overlay";
      inputs.crane.follows = "crane";
    };
    git-hooks = {
      url = "github:cachix/git-hooks.nix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    inputs@{
      flake-parts,
      rust-overlay,
      crane,
      nixpkgs,
      git-hooks,
      ...
    }:
    # Single source of truth for the deployment's rustc: consumed by this
    # flake's `rustToolchain` and by the re-exported evakuilo package below,
    # so every package in the deployment compiles with one nightly —
    # bpf-linker's LLVM pin is coupled to this date.
    let rustNightly = "2026-07-15"; in
    flake-parts.lib.mkFlake { inherit inputs; } {
      imports = [
        git-hooks.flakeModule
      ];

      # The module requires `services.kolorinko.package`; this wrapper
      # defaults it to this flake's own crane-built package for the host
      # system, so consumers only need `services.kolorinko.enable = true`.
      flake.nixosModules.kolorinko =
        { pkgs, lib, ... }:
        {
          imports = [ ./nix/kolorinko-module.nix ];
          services.kolorinko.package = lib.mkDefault inputs.self.packages.${pkgs.system}.kolorinko;
        };

      # evakuilo's own module, with `services.evakuilo.package` re-defaulted
      # to the re-blessed package below (evakuilo's module defaults it to its
      # self-build) — the server imports everything from this one flake.
      flake.nixosModules.evakuilo =
        { pkgs, lib, ... }:
        {
          imports = [ inputs.evakuilo.nixosModules.evakuilo ];
          services.evakuilo.package = lib.mkDefault inputs.self.packages.${pkgs.system}.evakuilo;
        };

      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];

      perSystem =
        { config, system, ... }:
        let
          pkgs = import nixpkgs {
            inherit system;
            overlays = [
              rust-overlay.overlays.default
              # wasm-opt (binaryen): nixpkgs' current build (132) miscompiles
              # our module — under trunk's flags (`-Oz --all-features`) it
              # emits a corrupt import section, and every engine then rejects
              # the wasm with "unknown import kind 0x7e". 123 is trunk's own
              # tested pin (what `trunk build` downloads for itself in dev).
              # Re-bump only against a `nix build .#kolorinko` whose wasm
              # validates.
              (_: prev: {
                binaryen = prev.binaryen.overrideAttrs (_: {
                  version = "123";
                  # The 132-era recipe's test wiring patches files that don't
                  # exist in 123 yet (scripts/test/finalize.py).
                  doCheck = false;
                  # …and 123's C++ predates the current GCC's warnings, which
                  # binaryen's own -Werror then escalates.
                  env.NIX_CFLAGS_COMPILE = "-Wno-error";
                  src = prev.fetchFromGitHub {
                    owner = "WebAssembly";
                    repo = "binaryen";
                    rev = "version_123";
                    hash = "sha256-SFruWOJVxO3Ll1HwjK3DYSPY2IprnDly7QjxrECTrzE=";
                  };
                });
              })
            ];
          };

          # Pinned to the last nightly from LLVM main before the 23 branch
          # cut: `nightly.latest` carries an LLVM snapshot newer than any
          # release, and its bitcode can't be read back by the release-LLVM
          # builds of bpf-linker ("ERROR llvm: Invalid record"). This date's
          # LLVM is 22.x, which `llvmPackages_22` (22.1.8) reads fine. Bump
          # the pin together with bpf-linker's LLVM below — it also builds
          # the re-exported evakuilo package.
          rustToolchain = pkgs.rust-bin.nightly.${rustNightly}.default.override {
            extensions = [
              "rust-src"
              "rust-analyzer"
              "clippy"
              "rustfmt"
            ];
            targets = [ "wasm32-unknown-unknown" ];
            # `bpfel-unknown-none` (kolorinko's eBPF steering program) is
            # deliberately absent: it has no rust-std component, and rustc
            # knows the target spec built-in — kolorinko's build.rs compiles
            # core from `rust-src` via `-Z build-std=core`.
          };

          craneLib = (crane.mkLib pkgs).overrideToolchain rustToolchain;

          # nixpkgs' bpf-linker defaults to the LLVM of nixpkgs' own rustc
          # (currently 21), while our pinned nightly's is 22.x — and bitcode
          # needs a reader ≥ its producer. The recipe's override hook exists
          # exactly for rust-overlay setups like ours; bump it together with
          # the nightly pin above.
          bpf-linker = pkgs.bpf-linker.override {
            llvmPackagesForLinker = pkgs.llvmPackages_22;
          };
        in
        {
          pre-commit.settings.hooks = {
            rustfmt = {
              enable = true;
              package = rustToolchain;
            };
            clippy = {
              enable = true;
              package = rustToolchain;
            };
            cargo-deny = {
              enable = true;
              name = "Cargo deny check";
              entry = "${pkgs.cargo-deny}/bin/cargo-deny check";
              files = "(Cargo\\.(toml|lock)|deny\\.toml)$";
              pass_filenames = false;
            };
          };

          packages.kolorinko = pkgs.callPackage ./nix/kolorinko-package.nix {
            inherit craneLib bpf-linker rustToolchain;
          };

          # evakuilo, rebuilt from its own recipe (evakuilo.lib.mkEvakuilo)
          # with our pinned nightly — the toolchain override happens at the
          # package-function level, since a derivation `.override` can no
          # longer swap the rustc crane baked into it.
          packages.evakuilo = (inputs.evakuilo.lib.mkEvakuilo {
            inherit pkgs rustToolchain;
          }).evakuilo;

          devShells.default = pkgs.mkShell {
            name = "rust-nightly";

            shellHook = config.pre-commit.shellHook;

            packages = config.pre-commit.settings.enabledPackages ++ [
              rustToolchain
              pkgs.cargo-nextest
              pkgs.cargo-watch
              pkgs.cargo-deny

              # Links the eBPF steering program kolorinko's build.rs compiles.
              bpf-linker

              pkgs.typst

              # libgit2 TLS backend: `git2`'s `https` feature links against
              # system OpenSSL, which it finds via pkg-config.
              pkgs.openssl
              pkgs.pkg-config
            ];
          };

          devShells.kolorinko = pkgs.mkShell {
            name = "rust-nightly";

            shellHook = config.pre-commit.shellHook;

            packages = config.pre-commit.settings.enabledPackages ++ [
              rustToolchain
              pkgs.cargo-nextest
              pkgs.cargo-watch
              pkgs.cargo-deny

              # Links the eBPF steering program kolorinko's build.rs compiles.
              bpf-linker

              pkgs.trunk

              # The web pipeline's tools, pinned here once so dev trunk builds
              # with the same binaries the release build uses — trunk
              # otherwise silently downloads its own copies, and that
              # divergence is what shipped a corrupt wasm to production.
              # `wasm-bindgen-cli` must byte-match Cargo.lock's `wasm-bindgen`
              # (asserted in nix/kolorinko-package.nix); `binaryen` is pinned
              # to 123 by the overlay above.
              pkgs.wasm-bindgen-cli
              pkgs.binaryen

              pkgs.just
              pkgs.xdg-utils

              # libgit2 TLS backend: `git2`'s `https` feature links against
              # system OpenSSL, which it finds via pkg-config.
              pkgs.openssl
              pkgs.pkg-config
              pkgs.heaptrack
            ];
          };
        };
    };
}
