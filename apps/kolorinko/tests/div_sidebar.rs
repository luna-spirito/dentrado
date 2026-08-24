//! Regression test for `[[div_]]` rendering: a sidebar of nested `[[div_]]`
//! blocks and `----` rules must render as a flat sequence of block elements
//! inside the outer `<div>`, with none wrapped in `<p>`. Before the fix `div_`
//! wrapped every interior blank-line-separated run in `<p>`, producing invalid
//! `<p><div>…</div></p>` and `<p><hr></p>` for block-heavy bodies — and was the
//! reason `div_` "behaved like `span`" in practice. See `div_sidebar_input.txt`
//! for the source (the RPC Authority `nav:side`, captured by Vizlox).

use kolorinko::wikidot_parser;
use kolorinko_render::render_block;
use leptos::prelude::*;

/// A fixed space for link-href assertions (raw bytes; Display adds the 'S' marker).
fn test_space() -> Option<kolorinko_rt::SpaceId> {
    Some(kolorinko_rt::SpaceId::from_bytes([0x2a; 16]))
}

/// Parse `src`, render top-level block flow, and return the SSR HTML.
fn render_html(space: Option<kolorinko_rt::SpaceId>, src: &str) -> String {
    let views = render_block(space, &wikidot_parser::parse(src));
    view! { <div>{views}</div> }.to_html()
}

/// Strip Leptos SSR hydration markers (`<!>`) so element/text structure can be
/// matched directly. Whitespace is kept (text like "RPC Database" keeps its
/// space); the renderer emits no inter-element whitespace for these blocks.
fn normalize(html: &str) -> String {
    html.replace("<!>", "")
}

/// Extract the inner HTML of the outer `<div class="side-block">…</div>`,
/// matching its closing tag by `<div>` nesting depth.
fn side_block_inner(norm: &str) -> &str {
    const OPEN: &str = r#"<div class="side-block">"#;
    let start = norm.find(OPEN).unwrap() + OPEN.len();
    let mut depth = 1i32;
    let mut i = start;
    while i < norm.len() && depth > 0 {
        let rest = &norm[i..];
        if rest.starts_with("<div ") || rest.starts_with("<div>") {
            depth += 1;
            i += 5;
        } else if rest.starts_with("</div>") {
            depth -= 1;
            i += 6;
        } else {
            i += 1;
        }
    }
    &norm[start..i - 6]
}

#[test]
fn div_sidebar_renders_blocks_without_p() {
    let norm = normalize(&render_html(
        test_space(),
        include_str!("div_sidebar_input.txt"),
    ));
    let inner = side_block_inner(&norm);

    // `div_` is a `<div>`, not a `<span>`: the two are distinct containers.
    assert!(norm.contains(r#"<div class="side-block">"#));
    assert!(!norm.contains(r#"<span class="side-block">"#));

    // `div_` suppresses `<p>`: a sidebar of only block children has no `<p>`
    // anywhere inside it. (The bug wrapped each interior block in `<p>`.)
    assert!(!inner.contains("<p>"), "side-block has stray <p>: {inner}");
    assert!(!norm.contains("<p><div"));
    assert!(!norm.contains("<p><hr"));

    // Block children render as a flat sequence in source order.
    assert!(inner.starts_with(r#"<div class="menu-item">"#));
    assert!(inner.contains(r#"<hr><div class="heading">RPC Database</div>"#));
    // Each menu-item holds its inline content directly (no `<p>` around it).
    assert!(inner.contains(&format!(
        r#"<div class="menu-item"><a href="/{s}/">Main</a></div>"#,
        s = test_space().unwrap()
    )));
    // Section dividers (`----`) survive as `<hr>` between the heading groups.
    assert_eq!(inner.matches("<hr>").count(), 7);
}
