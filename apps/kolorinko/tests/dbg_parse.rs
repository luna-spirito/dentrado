//! Debug scratch: parse + render a source file and dump HTML.
use kolorinko::wikidot_parser;
use kolorinko_render::{Scope, render_block};
use leptos::prelude::*;

mod common;
use common::test_space;

fn render_html(scope: Scope, src: &str) -> String {
    let views = render_block(scope, &wikidot_parser::parse(src));
    view! { <div>{views}</div> }.to_html()
}

#[test]
fn dbg_render() {
    let src = std::fs::read_to_string("/tmp/t1.txt").unwrap();
    let html = render_html(test_space(), &src);
    println!("{}", html.replace("<!>", ""));
}
