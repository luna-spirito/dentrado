{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-parts.url = "github:hercules-ci/flake-parts";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    crane.url = "github:ipetkov/crane";
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
            overlays = [ rust-overlay.overlays.default ];
          };

          # Pinned to the last nightly from LLVM main before the 23 branch cut:
          # `nightly.latest` carries an LLVM snapshot newer than any release,
          # and its bitcode can't be read back by the release-LLVM builds of
          # bpf-linker ("ERROR llvm: Invalid record"). This date's LLVM is
          # 22.x, which `llvmPackages_22` (22.1.8) reads fine. Bump the pin
          # together with bpf-linker's LLVM below.
          rustToolchain = pkgs.rust-bin.nightly."2026-07-15".default.override {
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

          # Built against the LLVM matching the pinned nightly (see above).
          bpf-linker = pkgs.rustPlatform.buildRustPackage {
            pname = "bpf-linker";
            version = "0.11.0";
            src = pkgs.fetchFromGitHub {
              owner = "aya-rs";
              repo = "bpf-linker";
              tag = "v0.11.0";
              hash = "sha256-uMpLQR2FAI96MYfWo8lR9pUeWhswY6wMUOxQwq3hCdw=";
            };
            cargoHash = "sha256-asCS4oLMXJ4y4vCDRsq2kuTOOPHebT0Dd+AE20GkZvI=";
            buildNoDefaultFeatures = true;
            buildFeatures = [ "llvm-22" ];
            # llvm-config (dev output) for llvm-sys' build script; the shared
            # library (lib output) for the link itself.
            nativeBuildInputs = [ (pkgs.lib.getDev pkgs.llvmPackages_22.llvm) ];
            buildInputs = [
              pkgs.zlib
              pkgs.libxml2
              (pkgs.lib.getLib pkgs.llvmPackages_22.llvm)
            ];
            doCheck = false;
            meta.mainProgram = "bpf-linker";
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
