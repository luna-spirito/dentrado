//! The recursive element-parser knot that ties every construct together.

use super::*;

/// The single-element parser, tied into a knot with [`recursive`] so containers
/// can recurse.
pub(crate) fn build_element<'a>() -> impl Parser<'a, In<'a>, Node, E<'a>> + Clone + 'a {
    recursive(|element| {
        choice((
            text_run(),
            raw_escape(),
            bare_http_link(),
            // Line-start-only block constructs.
            line_syntax(element.clone()),
            // Single-bracket `[url text]` link (must precede the `[[…]]` arm).
            single_bracket_link(),
            // Bracketed `[[…]]` constructs (and `[[[…]]]` links).
            just('[').ignore_then(choice((
                comment().ignore_then(element.clone()),
                just('[').ignore_then(bracket_syntax(element.clone())),
            ))),
            // Inline markup: `//`, `**`, `__`, `--`, `^^`, `,,`, `##`, vars.
            inline_syntax(element.clone()),
            // Fallback: a single arbitrary character (graceful degradation).
            any::<In<'a>, E<'a>>().map(|c| Node::Text(TextObj::Plain(c.to_string()))),
        ))
    })
}
