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
    if let Ok(src) = std::fs::read_to_string("/tmp/t1.txt") {
        let html = render_html(test_space(), &src);
        println!("{}", html.replace("<!>", ""));
    }
}

#[test]
fn bug1_line_start_anchoring() {
    for src in [
        "Some text 5[[include start]] more text",
        "Prefix .[[module ListPages category=\"x\"]]\nAfter",
        "A\nB[[include :scp-wiki:start]]C",
    ] {
        let html = render_html(test_space(), src);
        println!("IN : {src:?}\nOUT: {}\n---", html.replace("<!>", ""));
    }
}

#[test]
fn bug2_empty_escape() {
    for src in [
        "Text\n@@@@\nMore text",
        "[[collapsible show=\"s\" hide=\"h\"]]\nA\n[[/collapsible]]\n@@@@\n[[collapsible show=\"s\" hide=\"h\"]]\nB\n[[/collapsible]]",
        "X\n@@@@@@@@\nY",
    ] {
        let html = render_html(test_space(), src);
        println!("IN : {src:?}\nOUT: {}\n---", html.replace("<!>", ""));
    }
}

#[test]
fn bug3_center_line() {
    for src in [
        "= text",
        "para\n= centered line\nmore",
        "[[=]]\ndiv centered\n[[/=]]",
    ] {
        let html = render_html(test_space(), src);
        println!("IN : {src:?}\nOUT: {}\n---", html.replace("<!>", ""));
    }
}

#[test]
fn bug4_footnote_space() {
    for src in [
        "This list.// [[footnote]] body here [[/footnote]] after",
        "word[[footnote]] nb [[/footnote]]",
        "line1\n[[footnote]] eaten [[/footnote]]",
    ] {
        let html = render_html(test_space(), src);
        println!("IN : {src:?}\nOUT: {}\n---", html.replace("<!>", ""));
    }
}

#[test]
fn bug5_star_links() {
    for src in [
        "Join the [*https://discord.gg/x RPC Discord] now",
        "[[[*canon-policy |canon policy]]]",
        "[[*page]]",
    ] {
        let html = render_html(test_space(), src);
        println!("IN : {src:?}\nOUT: {}\n---", html.replace("<!>", ""));
    }
}

#[test]
fn bug6_table_colspan() {
    let src = "[[table style=\"width: 100%;\"]]\n[[row]]\n[[cell class=\"hcell\" colspan=\"3\"]]\n+ Title\n[[/cell]]\n[[/row]]\n[[row]]\n[[cell rowspan=\"2\"]]\na\n[[/cell]]\n[[cell]]\nb\n[[/cell]]\n[[/row]]\n[[row]]\n[[cell]]\nc\n[[/cell]]\n[[/row]]\n[[/table]]";
    let html = render_html(test_space(), src);
    println!("OUT: {}", html.replace("<!>", ""));
}
