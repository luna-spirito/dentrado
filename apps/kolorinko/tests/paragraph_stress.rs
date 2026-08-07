//! Regression test for Wikidot's line/paragraph handling inside `[[div]]`,
//! `[[div_]]` and `[[span]]`, captured from a real WikiDot render of the same
//! input (see `paragraph_stress_input.txt` / `expected/`).

use kolorinko::wikidot_parser;
use kolorinko_render::render_block;
use leptos::prelude::*;

/// Marker sentinels used to slice the SSR output back out of its wrapper.
const OPEN: &str = "@@__KOLORINKO_OPEN__@@";
const CLOSE: &str = "@@__KOLORINKO_CLOSE__@@";

/// Parse `src` and render `#page-content` (top-level block flow) to HTML.
fn render_block_html(src: &str) -> String {
    let views = render_block("test", &wikidot_parser::parse(src));
    let html = view! { <div>{OPEN}{views}{CLOSE}</div> }.to_html();
    let i = html.find(OPEN).unwrap() + OPEN.len();
    let j = html.find(CLOSE).unwrap();
    html[i..j].to_string()
}

/// Neutralise Leptos SSR cosmetics — hydration markers and empty `class`/`style`
/// attributes — plus the blank-line padding WikiDot adds around block output, so
/// only the element/text structure is compared.
fn normalize(html: &str) -> String {
    html.replace("<!>", "")
        .replace("class=\"\"", "")
        .replace("style=\"\"", "")
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect()
}

#[test]
fn paragraph_stress_matches_wikdot() {
    let actual = render_block_html(include_str!("paragraph_stress_input.txt"));
    let expected = include_str!("expected/paragraph_stress_expected.html");
    assert_eq!(normalize(&actual), normalize(expected));
}
