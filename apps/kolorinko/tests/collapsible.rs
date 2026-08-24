//! Regression tests for `[[collapsible]]` pairing: the lexer splits the
//! opener into an opener token plus a toggle-header leaf, so an inline
//! pairing may wrap the header (`[[size]] [[collapsible]] [[/size]]` —
//! the crossed idiom Wikidot honors), while the closer collects whatever
//! node holds the leaf as the node's header and the body gathered after
//! it stays unformatted.

use kolorinko::wikidot_parser;
use kolorinko_render::render_block;
use kolorinko_wikitext::{ContainerKind, Node, TextObj};
use leptos::prelude::*;

/// A fixed space for link-href assertions: byte 0x2a everywhere (the marker
/// bit is forced by `from_bytes`), rendered via `Display` into expected hrefs.
fn test_space() -> Option<kolorinko_rt::SpaceId> {
    Some(kolorinko_rt::SpaceId::from_bytes([0x2a; 16]))
}

fn html(src: &str) -> String {
    let views = render_block(test_space(), &wikidot_parser::parse(src));
    view! { <div>{views}</div> }.to_html().replace("<!>", "")
}

/// The crossed idiom `[[size 120%]] [[collapsible …]] [[/size]] Body [[/collapsible]]`:
/// the size interval crosses the collapsible interval, and the header leaf
/// — planted between them — is wrapped by the ordinary inline machinery.
/// The closer then wraps the whole block around the node holding the leaf,
/// so both toggle links carry the size while the body stays clean.
#[test]
fn size_idiom_wraps_the_header_not_the_body() {
    let c = wikidot_parser::parse(
        "[[size 120%]] [[collapsible show=\"+ open\" hide=\"- close\"]] [[/size]] Body Here [[/collapsible]]",
    );

    let [Node::Collapsible { header, body }] = &c[..] else {
        panic!("expected one collapsible, got {c:#?}")
    };
    assert!(matches!(
        &header[..],
        [Node::Container { kind: ContainerKind::Size(v), content }]
            if v == "120%" && matches!(
                &content[..],
                [Node::Text(TextObj::Plain(_)), Node::CollapsibleHeader { open, close, folded: true, .. }, Node::Text(TextObj::Plain(_))]
                    if open == "+ open" && close == "- close"
            )
    ));
    assert!(matches!(&body[..], [Node::Text(TextObj::Plain(t))] if t.contains("Body Here")));

    let h = html(
        "[[size 120%]] [[collapsible show=\"+ open\" hide=\"- close\"]] [[/size]] Body Here [[/collapsible]]",
    );
    let sized = h.matches("font-size:120%").count();
    let in_link = h.matches("font-size:120%;\"> <a").count();
    assert_eq!((sized, in_link), (2, 2), "html = {h}");
    assert!(h.contains("<p>Body Here</p>"), "html = {h}");
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
