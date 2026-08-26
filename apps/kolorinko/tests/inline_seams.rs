//! Regression test for inline seams: whitespace between adjacent inline nodes
//! must survive paragraph assembly — the space before a trailing `**…**`, and
//! the single newlines between links, which Wikidot renders as `<br>` inside
//! the paragraph (while still trimming the paragraph's own rims).

use kolorinko::wikidot_parser;
use kolorinko_render::render_block;
use kolorinko_wikitext::{ContainerKind, Node, TextObj, TextStyle};
use leptos::prelude::*;

mod common;
use common::test_space;

fn render_nodes(content: &[Node]) -> String {
    let views = render_block(test_space(), &content.to_vec());
    view! { <div>{views}</div> }.to_html().replace("<!>", "")
}

fn html(src: &str) -> String {
    render_nodes(&wikidot_parser::parse(src))
}

/// Whitespace-only rim of many adjacent text nodes (the shape include/module
/// splicing produces, e.g. `nav:top`): the whole paragraph is rim, so no
/// `<p>` may be emitted — in particular no spurious `<p><br></p>` from an
/// untrimmed middle piece.
#[test]
fn whitespace_only_paragraph_of_many_pieces_renders_nothing() {
    let content = [
        Node::Text(TextObj::Plain("\n".into())),
        Node::Text(TextObj::Plain(" \n".into())),
        Node::Text(TextObj::Plain("\n".into())),
        Node::Stylesheet("x{}".into()),
        Node::Text(TextObj::Plain("real".into())),
    ];
    assert_eq!(
        render_nodes(&content),
        "<div><style>x{}</style><p>real</p></div>"
    );
}

/// Rim trimming must not eat past real content: whitespace before/after an
/// inline node at the paragraph rim is dropped, but seams between content
/// pieces survive.
#[test]
fn rim_trim_stops_at_content() {
    let content = [
        Node::Text(TextObj::Plain(" \n".into())),
        Node::Container {
            kind: ContainerKind::Style(TextStyle::Bold),
            content: vec![Node::Text(TextObj::Plain("x".into()))],
        },
        Node::Text(TextObj::Plain(" \n".into())),
    ];
    assert_eq!(
        render_nodes(&content),
        "<div><p><strong>x</strong></p></div>"
    );
}

#[test]
fn seam_space_before_inline_container_survives() {
    assert_eq!(
        html("intro\n\nThe RPC Authority Wiki **[[[component:theme|Black Supremacy]]]**"),
        format!(
            r#"<div><p>intro</p><p>The RPC Authority Wiki <strong>\
<a href="/{s}/component:theme">Black Supremacy</a></strong></p></div>"#,
            s = test_space().space.unwrap()
        )
        .replace("\\\n", "")
    );
}

#[test]
fn single_newlines_between_inline_nodes_render_as_br() {
    let toc = "
[[div class=\"content-panel content-toc\"]]
**Table of Contents**
[/rpc-archive#operational Operational Information]
[/rpc-archive#list List of RPCs]


[[/div]]
";
    assert_eq!(
        html(toc),
        format!(
            "<div><div class=\"content-panel content-toc\">\
<p><strong>Table of Contents</strong><br>\
<a href=\"/{s}/rpc-archive#operational\">Operational Information</a><br>\
<a href=\"/{s}/rpc-archive#list\">List of RPCs</a></p></div></div>",
            s = test_space().space.unwrap()
        )
    );
}
