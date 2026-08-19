//! Regression tests for `[[collapsible]]` pairing: the opener plants a
//! `CollapsibleHeader` leaf, and the closer pairs it by scanning backwards
//! through the inline containers around it — so the inline formatting
//! context of the opener (`[[size]]`, `[[span]]`, …) wraps both toggle
//! links, while the body collected after the leaf stays unformatted.

use kolorinko::wikidot_parser;
use kolorinko_render::render_block;
use kolorinko_wikitext::Node;
use leptos::prelude::*;

fn html(src: &str) -> String {
    let views = render_block("rpcauthority", &wikidot_parser::parse(src));
    view! { <div>{views}</div> }.to_html().replace("<!>", "")
}

/// The idiom `[[size 120%]] [[collapsible …]] [[/size]] Body [[/collapsible]]`:
/// the `[[size]]` closes around the header leaf, so the scan pulls the leaf
/// out through it and both toggle links inherit the 120% wrap — the body
/// never does.
#[test]
fn size_idiom_wraps_both_links_not_body() {
    let html = html(
        "[[size 120%]] [[collapsible show=\"+ open\" hide=\"- close\"]] [[/size]] Body Here [[/collapsible]]",
    );

    // Exactly two font-size spans: one per toggle link, none around the body.
    assert_eq!(html.matches("font-size:120%").count(), 2, "{html}");
    assert!(html.contains(
        "<div class=\"collapsible-block-folded\"><span style=\"font-size:120%;\"> <a href=\"javascript:;\" class=\"collapsible-block-link\">+\u{a0}open</a> </span></div>"
    ), "{html}");
    assert!(html.contains(
        "<div class=\"collapsible-block-unfolded-link\"><span style=\"font-size:120%;\"> <a href=\"javascript:;\" class=\"collapsible-block-link\">-\u{a0}close</a> </span></div>"
    ), "{html}");

    // The body sits in the content div, free of any wrap.
    let content = html
        .split("<div class=\"collapsible-block-content\">")
        .nth(1)
        .unwrap()
        .split("</div>")
        .next()
        .unwrap();
    assert!(content.contains("Body Here"), "{html}");
    assert!(!content.contains("font-size"), "{html}");
}

/// Wikidot keeps a quoted-space label verbatim: `hide=" "` renders as a
/// single non-breaking space, not as the default label.
#[test]
fn hide_quoted_space_renders_nbsp() {
    let html = html("[[collapsible show=\"X\" hide=\" \"]]b[[/collapsible]]");
    assert!(html.contains(
        "<div class=\"collapsible-block-unfolded-link\"><a href=\"javascript:;\" class=\"collapsible-block-link\">\u{a0}</a></div>"
    ), "{html}");
}

/// An opener that never meets its closer degrades to the raw source text of
/// the opener — no half-open collapsible survives into the tree.
#[test]
fn unclosed_collapsible_degrades_to_raw() {
    let content = wikidot_parser::parse("[[collapsible show=\"X\"]] tail");
    assert!(
        content
            .iter()
            .all(|n| !matches!(n, Node::Collapsible { .. }))
    );
    assert!(
        content
            .iter()
            .any(|n| matches!(n, Node::Raw(s) if s.contains("[[collapsible")))
    );
}
