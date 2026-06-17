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

# Host/port the dev server listens on and the browser opens.
port := env_var_or_default("KOLORINKO_PORT", "8080")
host  := "127.0.0.1"

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

    export REPO_DIR="{{repo_dir}}"
    export REPO_INTERVAL="{{repo_interval}}"
    export KOLORINKO_BIND="{{host}}:{{port}}"
    export KOLORINKO_WEB_DIST="apps/kolorinko-web/dist"
    url="http://{{host}}:{{port}}"

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
    cargo run -p kolorinko

# Open the browser against an already-running server.
open:
    {{opener}} "http://{{host}}:{{port}}" || echo "open http://{{host}}:{{port}} manually"

# Run the whole workspace's tests.
test:
    cargo nextest run --workspace || cargo test --workspace

# Remove build artifacts and the frontend dist.
clean:
    cargo clean
    rm -rf apps/kolorinko-web/dist
