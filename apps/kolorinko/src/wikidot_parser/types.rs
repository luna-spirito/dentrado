//! Re-export of the shared Wikidot AST types.
//!
//! The type definitions live in the standalone [`kolorinko_wikitext`] crate so
//! the wasm web client can depend on them without pulling in host-only crates
//! (compio, git2, dentrado). The parser (this module's parent) constructs the
//! same types, and the rest of the server keeps referring to them as
//! `wikidot_parser::types::*`, so this file just re-exports the crate.

pub use kolorinko_wikitext::*;
