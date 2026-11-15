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

### Module order

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
