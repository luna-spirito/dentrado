//! Debug scratch: parse + render a source file and dump HTML.
use kolorinko::wikidot_parser;
use kolorinko_render::render_block;
use leptos::prelude::*;

/// A fixed space for link rendering (raw bytes; Display adds the 'S' marker).
fn test_space() -> Option<kolorinko_rt::SpaceId> {
    Some(kolorinko_rt::SpaceId::from_bytes([0x2a; 16]))
}

fn render_html(space: Option<kolorinko_rt::SpaceId>, src: &str) -> String {
    let views = render_block(space, &wikidot_parser::parse(src));
    view! { <div>{views}</div> }.to_html()
}

#[test]
fn dbg_render() {
    let src = std::fs::read_to_string("/tmp/t1.txt").unwrap();
    let html = render_html(test_space(), &src);
    println!("{}", html.replace("<!>", ""));
}
