//! Pure Leptos view renderer for Wikidot pages, shared by the CSR web client
//! (`kolorinko-web`) and the SSR debug CLI (`kolorinko render`).
//!
//! Mode-agnostic: build with the `csr` feature for the browser (the `view!`
//! macro emits DOM nodes) or `ssr` for the host (it emits HTML strings). The
//! render functions produce `AnyView`s from a parsed [`kolorinko_wikitext`]
//! tree and depend on no platform-specific API, so the same source compiles
//! under either feature.

mod render;

pub use render::render_block;

#[cfg(feature = "ssr")]
mod skeleton;

#[cfg(feature = "ssr")]
pub use skeleton::render_page_document;
