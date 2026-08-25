# kolorinko dev tasks.

default: dev

dev:
    cd apps/kolorinko-web && trunk build
    cargo run -p kolorinko -- apps/kolorinko/config.dev.toml

test:
    cargo nextest run --workspace || cargo test --workspace

clean:
    cargo clean
    rm -rf apps/kolorinko-web/dist

package:
    nix build .#kolorinko
