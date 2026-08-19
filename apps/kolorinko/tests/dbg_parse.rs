//! Debug scratch: parse + render a source file and dump HTML.
use kolorinko::wikidot_parser;
use kolorinko_render::render_block;
use leptos::prelude::*;

fn render_html(site: &str, src: &str) -> String {
    let views = render_block(site, &wikidot_parser::parse(src));
    view! { <div>{views}</div> }.to_html()
}

#[test]
fn dbg_render() {
    let src = std::fs::read_to_string("/tmp/t1.txt").unwrap();
    let html = render_html("site", &src);
    println!("{}", html.replace("<!>", ""));
}
