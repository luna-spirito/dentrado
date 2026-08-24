//! Regression test for `nav:top` rendering: Wikidot wraps the content of each
//! top-level list item that opens a submenu in `<a href="javascript:;">` (the
//! inert "button" legacy themes style via `#top-bar li a`), while nested leaf
//! entries and top-level entries without a submenu keep their real links.

use kolorinko::wikidot_parser;
use kolorinko_render::{render_block, wrap_topbar_lists};
use leptos::prelude::*;

/// A fixed space for link-href assertions (raw bytes; Display adds the 'S' marker).
fn test_space() -> Option<kolorinko_rt::SpaceId> {
    Some(kolorinko_rt::SpaceId::from_bytes([0x2a; 16]))
}

fn render_topbar_html(space: Option<kolorinko_rt::SpaceId>, src: &str) -> String {
    let mut content = wikidot_parser::parse(src);
    wrap_topbar_lists(&mut content);
    let views = render_block(space, &content);
    view! { <div>{views}</div> }.to_html().replace("<!>", "")
}

#[test]
fn topbar_wraps_submenu_triggers() {
    let html = render_topbar_html(
        test_space(),
        concat!(
            "[[div class=\"top-bar\"]]\n",
            "* RPC Archive\n",
            " * [[[rpc-archive|001]]]\n",
            " * [[[non-canon-hub|Joke]]]\n",
            "* [[[home|Home]]]\n",
            "[[/div]]\n",
        ),
    );

    // A top-level item that opens a submenu has its text wrapped in the inert
    // anchor, placed before the nested `<ul>`.
    assert!(
        html.contains(r#"<li><a href="javascript:;">RPC Archive</a><ul>"#),
        "submenu trigger not wrapped: {html}"
    );
    // Nested leaf entries keep their real links.
    assert!(
        html.contains(&format!(
            r#"<a href="/{s}/rpc-archive">001</a>"#,
            s = test_space().unwrap()
        )),
        "{html}"
    );
    assert!(
        html.contains(&format!(
            r#"<a href="/{s}/non-canon-hub">Joke</a>"#,
            s = test_space().unwrap()
        )),
        "{html}"
    );
    // A top-level item with no submenu keeps its real link — it is not turned
    // into a dead `javascript:;` anchor (and never nests anchors).
    assert!(
        html.contains(&format!(
            r#"<a href="/{s}/home">Home</a>"#,
            s = test_space().unwrap()
        )),
        "{html}"
    );
    assert!(
        !html.contains("<a href=\"javascript:\"><a"),
        "nested anchors: {html}"
    );
}

#[test]
fn topbar_leaves_nested_levels_unwrapped() {
    // A three-level menu: only the outermost level's submenu triggers are
    // wrapped. The middle level (itself a submenu of the top, with its own
    // children) must NOT get a `javascript:;` anchor.
    let html = render_topbar_html(
        test_space(),
        concat!(
            "[[div class=\"top-bar\"]]\n",
            "* Top\n",
            " * Mid\n",
            "  * [[[leaf|Leaf]]]\n",
            "[[/div]]\n",
        ),
    );
    assert!(html.contains(r#"<a href="javascript:;">Top</a>"#), "{html}");
    // "Mid" has a sublist but sits one level down, so it stays bare text.
    assert!(
        !html.contains(r#"<a href="javascript:;">Mid</a>"#),
        "middle level wrongly wrapped: {html}"
    );
    assert!(
        html.contains(&format!(
            r#"<a href="/{s}/leaf">Leaf</a>"#,
            s = test_space().unwrap()
        )),
        "{html}"
    );
}

#[test]
fn topbar_transform_is_idempotent_without_sublists() {
    // A flat list with no submenus is untouched: no `javascript:;` anchors.
    let html = render_topbar_html(test_space(), "* [[[a|A]]]\n* [[[b|B]]]\n");
    assert!(!html.contains("javascript:"), "{html}");
}
