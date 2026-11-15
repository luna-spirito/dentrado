- Speak English with the user, even if the user uses some other language.
- Follow principles of pure functional programming.
- Basically, be a Haskell+DependentTyping+AlgebraicEffects programmer stuck in a Rust codebase.
- Aim for clean, concise, elegant code.
- Always follow DRY. Don't write unnecessary code. Aim to simplify the codebase as a whole. If something takes way too much code to implement, consider other solutions or abstractions.
- Prefer self-descriptive clean code over excessive comments. When writing documentation, use english, not russian.
- Code files shouldn't exceed 500 lines (any lines, not just SLOC). Legacy files are to be converted eventually.

`python3 scripts/module_order.py` checks every workspace library (or pass one or
more `lib.rs` paths). It runs as a pre-commit hook.

- Sometimes rust-analyzer (LSP) returns phantom errors/diagnostics. If there is a diagnostic that shouldn't be there, ignore it, just make sure `cargo check` passes.
- You can use `cargo run -p kolorinko --release -- apps/kolorinko/config.dev.toml render rpcauthority` to render the entire corpus (gets placed into @.kolorinko/render), or `cargo run -p kolorinko --release -- apps/kolorinko/config.dev.toml render rpcauthority/nav:side`, to render aa specific page as `nav:side`. Needed to evaluate correctness of parsing changes across corpus.`

dentrado-cache:
- zero-copy
- "Temporarily" storing page's content into a Vec or on-tack `AlignedPage`, or array, is not allowed. Pages should never leave the arena they belong to, and only be provided via a reference.
- Assume a single table entry can depend on a chain of pages worth 5GB.

# Module order

Every crate must read in a single depth-first postorder:

- Children are visited in `mod` declaration order.
- A module is visited *after* all of its children.
- A module may refer only to modules visited earlier.

Concretely, given `x::a`, `x::b`, `x::c`, then `y`, then `z::d`, `z::e::m`,
`z::e::n`, `z::f`:

- `x::a` may refer to nothing.
- `x::b` may refer to `x::a`.
- `x::c` may refer to `x::a`, `x::b`.
- `y` may refer to `x::{a,b,c}`.
- `z::d` may refer to `x::{a,b,c}`, `y`.
- `z::e::m` may refer to `x::{a,b,c}`, `y`, `z::d`.
- `z::e::n` may refer to `x::{a,b,c}`, `y`, `z::d`, `z::e::m`.
- `z::f` may refer to any earlier module.

Ancestor/descendant references are structural and always allowed (a parent may
name its children; a child may use names its parent re-exports). Only
cross-branch references constrain order. Cycles are illegal — fix them, never
baseline or allowlist them.

# About

Dentrado — functional-reactive DBMS + wiki engine (kolorinko)
Two layers: an engine (event-driven, gear-based, CRDT-like) and an application (Wikidot-compatible wiki with SSR + wasm client).
Engine
- dentrado/ — core (src/lib.rs), no unsafe.
- core/api/ — runtime type vocabulary (RuntimeTypes, GearMeta/Input/Produce/Result); shared.rs — shared Sync outputs.
- core/core_ctx.rs (2k lines) — the heart: gear execution context.
- core/gear.rs — gear running, IsRuntime, query lenses.
- core/db.rs — Db, DbConfig, doorbell, threads/cores.
- core/loc_ctx.rs, routing.rs, stats.rs, doorbell.rs — id localization, event routing.
- core/wire/{format,builder,merger}.rs — network event protocol.
- core/storage/ — the Storage<R> contract: in_memory.rs (reference), paged/ (on-disk via dentrado-cache), view.rs (GearView overlay).
- utils/ — state_graph*.rs (dependency graph/incrementality), text.rs (RGA/text CRDTs), sg_ord_map.rs.
- tests/ — mulsum, paged, shared, wiki, wiki2 (collaboration scenarios).
- dentrado-cache/ — persistent page cache (port of the Lean model; single-threaded, arena, zero-copy).
- page.rs, disk.rs — pages and the Storage trait.
- blockcache/ — slot arena, flight registry, queues (arena/seq/flights/disk_arena).
- core/ — free-list (2 pools), EBR, tables (B-tree + delta), page codecs.
- persist/ — flush rounds, recovery, poison.
- storage/ — async surface: records, drafts, page reads, guard.
- dentrado-types/ — Localizable/Remapper, local id newtypes; wasm-safe, no compio.
- dentrado-macros/ — #[derive(Localizable)], #[gears] (gear-module declaration, src/gears.rs), #[gears_schema] (wasm schema from the same file).
Application (everything around kolorinko)
- apps/kolorinko-wikitext/ — wikitext AST + payloads (Content/Node, ArticleView).
- apps/kolorinko-rt/ — shared gear schema and wire protocol; gears.def.rs is the single gear declaration, src/ids.rs — routes/slugs.
- apps/kolorinko/ — server:
- wikidot_parser/ (lexer/pairer/builder/merge — the parser), wikidot_page/ (wiki modules: includes, listpages, links, vars, forms, dataset…).
- server.rs, web.rs, transport stack, respond.rs, ssr.rs — HTTP/SSR/WebTransport.
- render_cli.rs — render for corpus checks, assets.rs, steer.rs + steer-bpf/ (BPF accelerator), tls.rs, repo.rs, metrics.rs, globals.rs.
- apps/kolorinko-render/ — pure Leptos renderer (render.rs, layout.rs, css.rs, skeleton.rs, opengraph.rs), csr/ssr.
- apps/kolorinko-web/ — wasm client: main.rs, transport.rs (WebTransport + fallback), cache.rs, router.rs, menu.rs.
Misc
docs/ (architecture PDFs, roadmap.md, kolorinko.md), thinking/ (design notes), scripts/module_order.py (module order, pre-commit), justfile (dev/test/package), nix/ + flake.nix, TODO.md.
