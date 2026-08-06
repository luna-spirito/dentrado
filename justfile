# kolorinko dev tasks.

default: dev

dev:
    cd apps/kolorinko-web && trunk build
    cargo run -p kolorinko -- apps/kolorinko/config.dev.toml & sleep 1; xdg-open https://127.0.0.1:4433 || true; wait

test:
    cargo nextest run --workspace || cargo test --workspace

clean:
    cargo clean
    rm -rf apps/kolorinko-web/dist
