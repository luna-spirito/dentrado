# kolorinko dev tasks.
#
# Entry point: `just dev` — builds the frontend, runs the server with sane
# defaults, and opens the browser. Re-run to pick up frontend changes.
#
# Run inside the kolorinko shell so `just`, `trunk`, and `xdg-open` are on PATH:
#     nix develop .#kolorinko -c just dev
# or enter the shell once and then:
#     just dev

# ---- config (override via env) -------------------------------------------

# Single origin for everything: TCP (HTTPS/HTTP1.1 bootstrap) and UDP
# (QUIC/H3+WebTransport) coexist on the same `host:port`. The server default
# is `[::1]:4433`; here we use IPv4 loopback explicitly.
port := env_var_or_default("KOLORINKO_PORT", "4433")
host  := "127.0.0.1"

# mkcert cert the browser trusts. WebTransport cannot use a self-signed cert
# (QUIC has no "proceed anyway" prompt), so we fail fast if the pair is
# missing rather than silently serving a browser-refusing server. The server
# also auto-discovers this same pair, but exporting it makes the dependency
# explicit. Generate once (one-time, outside the nix store):
#   mkdir -p .certs && cd .certs && mkcert localhost 127.0.0.1 ::1 \
#     && mv localhost+2.pem localhost.pem && mv localhost+2-key.pem localhost-key.pem
# and trust the root in the system store: mkcert -install  (then add
# security.pki.certificateFiles = [ "~/.local/share/mkcert/rootCA.pem" ]; on
# NixOS and rebuild so Chromium reads it).
cert_file := env_var_or_default("KOLORINKO_CERT_FILE", justfile_directory() + "/.certs/localhost.pem")
key_file  := env_var_or_default("KOLORINKO_KEY_FILE",  justfile_directory() + "/.certs/localhost-key.pem")

# Where the Wikidot export lives. Prefers an explicit $REPO_DIR, then reuses
# an existing clone at /tmp/kolorinko-export, else falls back to a project-local
# .kolorinko/repo (the server auto-clones it on first use).
# `${REPO_DIR:-}` (not `"$REPO_DIR"`) is the set-`u`-safe POSIX idiom: under
# `set -o nounset` a bare `$REPO_DIR` aborts when unset, but `${VAR:-}` always
# expands to empty. Resolved once at load so all recipes share one value.
repo_dir := `if [ -n "${REPO_DIR:-}" ]; then printf '%s' "${REPO_DIR}"; elif [ -d /tmp/kolorinko-export ]; then printf '%s' /tmp/kolorinko-export; else printf '%s' .kolorinko/repo; fi`

# Disable background `git pull` during dev (avoids hard-resetting the export
# out from under you). Set REPO_INTERVAL to enable periodic pulls.
repo_interval := env_var_or_default("REPO_INTERVAL", "999999")

# Frontend tooling. `trunk` and `xdg-open` are provided by the kolorinko shell.
trunk := env_var_or_default("TRUNK", "trunk")
opener := env_var_or_default("BROWSER", "xdg-open")

# ---- recipes -------------------------------------------------------------

# `just` with no argument → dev.
default: dev

# Build the Leptos frontend into apps/kolorinko-web/dist.
web:
    cd apps/kolorinko-web && {{trunk}} build

# Type-check the frontend for wasm (fast, no codegen).
web-check:
    cd apps/kolorinko-web && cargo check --target wasm32-unknown-unknown

# Build the frontend, run the server, open the browser. Ctrl-C stops the server.
dev: web
    #!/usr/bin/env bash
    set -uo pipefail

    # WebTransport needs a browser-trusted cert; fail fast rather than serve a
    # self-signed cert the browser will reject with CERTIFICATE_VERIFY_FAILED.
    if [[ ! -f "{{cert_file}}" || ! -f "{{key_file}}" ]]; then
      echo "✗ no TLS cert at {{cert_file}} (and {{key_file}})." >&2
      echo "  Generate once:  mkdir -p .certs && cd .certs && \\" >&2
      echo "    mkcert localhost 127.0.0.1 ::1 && \\" >&2
      echo "    mv localhost+2.pem localhost.pem && mv localhost+2-key.pem localhost-key.pem" >&2
      exit 1
    fi

    export REPO_DIR="{{repo_dir}}"
    export REPO_INTERVAL="{{repo_interval}}"
    export KOLORINKO_BIND="{{host}}:{{port}}"
    export KOLORINKO_WEB_DIST="apps/kolorinko-web/dist"
    export KOLORINKO_CERT_FILE="{{cert_file}}"
    export KOLORINKO_KEY_FILE="{{key_file}}"
    url="https://{{host}}:{{port}}"

    echo "➜ kolorinko dev  →  $url"
    echo "   repo: $REPO_DIR   (first page load clones it if missing)"

    cargo run -p kolorinko &
    srv=$!

    cleanup() {
      kill "$srv" 2>/dev/null || true
      wait "$srv" 2>/dev/null || true
    }
    trap cleanup EXIT INT TERM

    # Wait (up to ~30s) for the port to accept connections, bailing early if
    # the server dies. The port opens before any page is loaded, so a slow
    # first-clone won't block the browser from loading the shell.
    up=0
    for _ in $(seq 1 150); do
      if ! kill -0 "$srv" 2>/dev/null; then
        echo "✗ server exited during startup" >&2
        exit 1
      fi
      if (exec 3<>/dev/tcp/{{host}}/{{port}}) 2>/dev/null; then
        exec 3>&- 3<&-
        up=1
        break
      fi
      sleep 0.2
    done

    if [ "$up" = 1 ]; then
      echo "✓ server up — opening browser"
      {{opener}} "$url" >/dev/null 2>&1 || echo "   (couldn't open a browser; visit $url manually)"
    else
      echo "⚠ server still starting — visit $url once it's ready"
    fi

    # Foreground the server so Ctrl-C is delivered to this script.
    wait "$srv"

# Run the server only (no build, no browser). Assumes `just web` was run.
run:
    #!/usr/bin/env bash
    set -uo pipefail
    export REPO_DIR="{{repo_dir}}"
    export REPO_INTERVAL="{{repo_interval}}"
    export KOLORINKO_BIND="{{host}}:{{port}}"
    export KOLORINKO_WEB_DIST="apps/kolorinko-web/dist"
    export KOLORINKO_CERT_FILE="{{cert_file}}"
    export KOLORINKO_KEY_FILE="{{key_file}}"
    cargo run -p kolorinko --bin kolorinko # -- --inject-wt-hash

# Open the browser against an already-running server.
open:
    {{opener}} "https://{{host}}:{{port}}" || echo "open https://{{host}}:{{port}} manually"

# Run the whole workspace's tests.
test:
    cargo nextest run --workspace || cargo test --workspace

# Remove build artifacts and the frontend dist.
clean:
    cargo clean
    rm -rf apps/kolorinko-web/dist
