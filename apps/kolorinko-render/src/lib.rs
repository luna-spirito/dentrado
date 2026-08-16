//! Pure Leptos view renderer for Wikidot pages, shared by the web client
//! (`kolorinko-web`), the live server's SSR responses, and the SSR debug CLI
//! (`kolorinko render`):
//! - [`layout`] is the whole page skeleton, driven by getter closures — the
//!   client feeds it signals (hydrating from embedded SSR state, then live via
//!   WebTransport), the server feeds resolved gear outputs and serializes once.
//! - the render functions produce `AnyView`s from a parsed
//!   [`kolorinko_wikitext`] tree and depend on no platform-specific API, so the
//!   same source compiles under the `csr` and `ssr` features.

mod css;
pub mod layout;
mod render;

pub use css::{http_refs, http_tail, rewrite_with};
pub use layout::{THEME_LINK_ID, document_title, html_escape, layout, theme_link};
pub use render::{render_block, wrap_topbar_lists};

#[cfg(feature = "ssr")]
mod skeleton;

#[cfg(feature = "ssr")]
pub use skeleton::{render_page_document, render_ssr_document};
