//! The recursive element-parser knot that ties every construct together.

use super::*;

/// The single-element parser (yielding a [`Content`] slice, since a malformed
/// block *flattens* into more than one node), tied into a knot with
/// [`recursive`] so containers can recurse.
pub(crate) fn build_element<'a>()
-> impl Parser<'a, In<'a>, (Content, Option<ContentExitReason>), E<'a>> + Clone + 'a {
    recursive(|element| {
        choice((
            one(text_run()),
            one(raw_escape()),
            one(bare_http_link()),
            // Line-start-only block constructs.
            one(line_syntax(element.clone())),
            // Single-bracket `[url text]` link (must precede the `[[…]]` arm).
            one(single_bracket_link()),
            // `[!-- … --]` comment, routed through the content loop.
            comment(element.clone()),
            // Bracketed `[[…]]` constructs (and `[[[…]]]` links).
            bracket_syntax(element.clone()),
            // Inline markup: `//`, `**`, `__`, `--`, `^^`, `,,`, `##`, vars.
            one(inline_syntax(element.clone())),
            // Fallback: a single arbitrary character (graceful degradation).
            any::<In<'a>, E<'a>>().map(|c| (vec![Node::Text(TextObj::Plain(c.to_string()))], None)),
        ))
    })
}
