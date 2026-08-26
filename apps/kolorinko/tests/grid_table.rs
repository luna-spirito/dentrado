//! Regression test for `[[table]]` grid-table hydration: the SSR output must
//! wrap rows in `<tbody>`, because an HTML parser inserts a `<tbody>` around
//! bare `<tr>`s no matter what the source says. Without it, the served HTML
//! and the client's view tree disagree by one element level, and hydration —
//! which walks nodes positionally, matching any element regardless of tag —
//! drifts silently until it hits a marker comment where an element is
//! expected and panics (first seen on the rate module's badge table, which
//! surfaced as "expected <span>, found #comment" inside a `<td class="cellStyle">`).

use kolorinko::wikidot_parser;
use kolorinko_render::render_block;
use leptos::prelude::*;

mod common;
use common::test_space;

#[test]
fn grid_table_rows_are_wrapped_in_tbody() {
    let src = concat!(
        "[[table class=\"tableStyle\"]]\n",
        "[[row class=\"rateStyle\"]]\n",
        "[[cell class=\"cellStyle\"]][[span class=\"tooltip\"]]A[[/span]][[/cell]]\n",
        "[[/row]]\n",
        "[[/table]]",
    );
    let html = render_block(test_space(), &wikidot_parser::parse(src))
        .into_iter()
        .map(|v| v.to_html())
        .collect::<String>()
        .replace("<!>", "");
    assert!(
        html.contains(r#"<table class="tableStyle"><tbody><tr class="rateStyle">"#),
        "rows must be wrapped in <tbody>: {html}"
    );
}
