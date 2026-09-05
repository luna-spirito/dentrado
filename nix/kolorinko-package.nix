# Self-contained kolorinko package, built purely with crane: the server from
# the workspace root plus the Leptos frontend built with Trunk.
#
# The web crate is a standalone workspace with path dependencies
# (../kolorinko-{rt,wikitext,render}), so both derivations use the whole repo
# tree as src, preserving that layout; Trunk is pointed at the web crate via
# `trunkIndexPath`.
{
  lib,
  craneLib,
  pkgs,
  bpf-linker,
  rustToolchain,
}:

let
  root = ../.;

  # The web crate's Trunk build does `cargo metadata` over the whole root
  # workspace (it reaches it via the ../ path deps), so all member manifests
  # must survive filtering — but the server crate's SOURCES must not: an edit
  # there has nothing to do with the frontend and must not rebuild it.
  serverPrefix = toString root + "/apps/kolorinko/";
  srcWeb = lib.cleanSourceWith {
    src = craneLib.path root;
    filter =
      path: type:
      if lib.hasPrefix serverPrefix path then
        lib.hasSuffix "Cargo.toml" path
      else
        craneLib.filterCargoSources path type || lib.hasPrefix (toString root + "/apps/kolorinko-web") path;
  };

  # `cargo metadata` for the web crate walks up into the root workspace via
  # the ../ path deps, so the vendored registry must cover BOTH lockfiles'
  # closures (e.g. dentrado's blake3) — not just the web one. The toolchain's
  # rust-src lock covers a third closure: `build.rs` compiles the eBPF
  # steering program with `-Z build-std=core`, and that resolution pulls the
  # sysroot workspace's own registry deps (rustc-literal-escaper, hashbrown,
  # …), which no project lockfile knows about — unreachable in the sandbox
  # without them vendored.
  cargoVendorDir = craneLib.vendorMultipleCargoDeps {
    cargoLockList = [
      (root + "/Cargo.lock")
      (root + "/apps/kolorinko-web/Cargo.lock")
      (rustToolchain + "/lib/rustlib/src/rust/library/Cargo.lock")
    ];
  };

  version =
    (builtins.fromTOML (builtins.readFile (root + "/apps/kolorinko/Cargo.toml"))).package.version;

  # The `wasm-bindgen` version the web lockfile actually pins. The CLI must
  # byte-match it: on mismatch it doesn't fail the build, it silently corrupts
  # the wasm's import section (misaligned rewrite, doubled `__wbg_` prefixes),
  # and the SPA only dies in the browser with "wasm validation error".
  lockWasmBindgen =
    let lock = builtins.fromTOML (builtins.readFile (root + "/apps/kolorinko-web/Cargo.lock"));
    in (lib.findFirst (p: p.name == "wasm-bindgen") null lock.package).version;

  srcServer = lib.cleanSourceWith {
    src = craneLib.path root;
    filter = craneLib.filterCargoSources;
  };

  server = (craneLib.buildPackage {
    pname = "kolorinko";
    inherit version;
    src = srcServer;
    inherit cargoVendorDir;
    # Only the server's dependency tree out of the workspace.
    cargoExtraArgs = "-p kolorinko";
    # git2's `https` feature links against system OpenSSL via pkg-config;
    # bpf-linker links the eBPF steering program kolorinko's build.rs compiles.
    nativeBuildInputs = [
      pkgs.pkg-config
      bpf-linker
    ];
    buildInputs = [ pkgs.openssl ];
    doCheck = false;
  }).overrideAttrs (old: {
    # The placeholder fallback in build.rs is for toolchain-less cargo
    # builds; this flake always passes bpf-linker, so a missing embed must
    # fail the package here, not steering in production. In `overrideAttrs`
    # (not the call attrs) so the check doesn't leak into crane's internal
    # deps-only build, which has no `bin/`.
    nativeBuildInputs = old.nativeBuildInputs ++ [ pkgs.python3 ];
    doInstallCheck = true;
    installCheckPhase = ''
      runHook preInstallCheck
      python3 - "$out/bin/kolorinko" <<'PY'
      import struct, sys
      data = open(sys.argv[1], "rb").read()
      off = 0
      while True:
          i = data.find(b"\x7fELF", off)
          if i < 0:
              sys.exit("kolorinko carries no embedded BPF object (placeholder!)")
          if i + 20 <= len(data) and struct.unpack_from("<H", data, i + 18)[0] == 247:
              sys.exit(0)
          off = i + 1
      PY
      runHook postInstallCheck
    '';
  });

  # crane's mkDummySrc keeps only the ROOT Cargo.lock — the web crate's own
  # lockfile gets dropped, cargo re-resolves to different versions and the
  # artifacts never match Trunk's build. Put it back.
  #
  # The output MUST be named "source" (like mkDummySrc's own output): it is
  # what makes this derivation unpack to /build/source, the same directory the
  # Trunk build uses — cargo's fingerprints include CARGO_HOME (= $PWD/
  # .cargo-home) and the path-dep roots, so any other name defeats artifact
  # reuse entirely (rust-lang/cargo#10179).
  webDummySrc = pkgs.runCommand "source" { } ''
    cp -r ${craneLib.mkDummySrc { src = srcWeb; }} $out
    chmod -R u+w $out
    cp ${root + "/apps/kolorinko-web/Cargo.lock"} $out/apps/kolorinko-web/Cargo.lock
  '';

  # NB: pass the real src ONLY as `dummySrc`'s input. Any extra attr
  # (e.g. `srcWeb`) leaks into the derivation env via mkDerivation and
  # re-hashes webDeps on every commit, rebuilding all wasm deps — crane
  # itself sets `src = dummySrc` and ignores (warns about) a real `src`.
  webDeps = craneLib.buildDepsOnly {
    pname = "kolorinko-web";
    inherit version;
    inherit cargoVendorDir;
    dummySrc = webDummySrc;
    CARGO_BUILD_TARGET = "wasm32-unknown-unknown";
    doCheck = false;
    # Run cargo from inside the web crate — the SAME cwd the Trunk build
    # uses. Cargo CONCATENATES [target.<t>.rustflags] across the config
    # hierarchy, so building from the repo root applies the web_sys_unstable_apis
    # cfg once, while building from the web crate applies it twice (web config
    # + root config) — different -Cmetadata, and the inherited artifacts are
    # considered foreign and rebuilt from scratch.
    # CARGO_TARGET_DIR must be absolute since cwd differs from the install
    # hook's (installCargoArtifactsHook defaults to ./target from the source
    # root — the same directory).
    buildPhaseCargoCommand = ''
      export CARGO_TARGET_DIR="$NIX_BUILD_TOP/$sourceRoot/target"
      cd apps/kolorinko-web
      cargoWithProfile check --locked
      cargoWithProfile build --locked
    '';
  };

  web = craneLib.buildTrunkPackage {
    pname = "kolorinko-web";
    inherit version;
    src = srcWeb;
    inherit cargoVendorDir;
    # Trunk discovers the crate and Trunk.toml from the CWD, not from the
    # index path (a path arg into a subdirectory of a virtual workspace fails
    # with "could not find the root package"), so run it from inside the web
    # crate like the justfile does — but keep cargo's target dir where the
    # inherited artifacts land (the hook copies them relative to the build
    # root, while cargo would otherwise follow the web workspace root).
    buildPhaseCargoCommand = ''
      cd apps/kolorinko-web
      export CARGO_TARGET_DIR="$NIX_BUILD_TOP/$sourceRoot/target"
      trunk build --release=true ./index.html
    '';
    # buildPhase's `cd` persists into installPhase, so dist is at ./dist.
    installPhaseCommand = "cp -r dist $out";
    # `wasm-bindgen-cli` must byte-match `wasm-bindgen` in the web lockfile
    # (see `lockWasmBindgen`); refuse evaluation on drift instead of letting
    # the CLI corrupt the wasm.
    wasm-bindgen-cli = lib.throwIfNot (pkgs.wasm-bindgen-cli.version == lockWasmBindgen)
      "wasm-bindgen-cli ${pkgs.wasm-bindgen-cli.version} != wasm-bindgen ${lockWasmBindgen} in apps/kolorinko-web/Cargo.lock — keep them byte-matched"
      pkgs.wasm-bindgen-cli;
    # Deps (leptos, wasm-bindgen, the path crates' deps…) come from the
    # stubbed deps-only build above; Trunk then builds just the web crate.
    cargoArtifacts = webDeps;
    doCheck = false;
    # Compiling the wasm with an engine is the only gate that catches a
    # mis-wired web toolchain — the binaryen-132 corruption of the import
    # section shipped silently otherwise.
    nativeBuildInputs = [ pkgs.nodejs ];
    doInstallCheck = true;
    installCheckPhase = ''
      runHook preInstallCheck
      for wasm in "$out"/*.wasm; do
        node -e 'try { new WebAssembly.Module(require("fs").readFileSync(process.argv[1])) } catch (e) { console.error(e.message); process.exit(1) }' "$wasm"
      done
      runHook postInstallCheck
    '';
  };
in
pkgs.symlinkJoin {
  name = "kolorinko-${version}";
  paths = [
    server
    (pkgs.runCommand "kolorinko-web-dist" { } ''
      mkdir -p "$out/share"
      cp -rL "${web}" "$out/share/web-dist"
    '')
  ];
}
