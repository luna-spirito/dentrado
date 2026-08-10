//! Inline markup: emphasis spans, super/subscript, colour spans, variables.

use super::*;

/// All inline (non-line-start) markup.
pub(crate) fn inline_syntax<
    'a,
    P: Parser<'a, In<'a>, (Content, Option<ContentExitReason>), E<'a>> + Clone + 'a,
>(
    element: P,
) -> impl Parser<'a, In<'a>, Node, E<'a>> + Clone + 'a {
    choice((
        style(element.clone(), "//", TextStyle::Italic),
        style(element.clone(), "**", TextStyle::Bold),
        style(element.clone(), "__", TextStyle::Underline),
        style(element.clone(), "--", TextStyle::Strikethrough),
        superscript(element.clone()),
        subscript(element.clone()),
        color_span(element),
        module_var(),
        include_var(),
        // `-- ` → em-dash.
        just("-- ").to(Node::Text(TextObj::Plain("— ".to_string()))),
    ))
}

/// A `//…//`-style delimited span. The opener must not be immediately followed
/// by a space; the body runs to the next delimiter or EOL.
pub(crate) fn style<
    'a,
    P: Parser<'a, In<'a>, (Content, Option<ContentExitReason>), E<'a>> + Clone + 'a,
>(
    element: P,
    delim: &'static str,
    st: TextStyle,
) -> impl Parser<'a, In<'a>, Node, E<'a>> + Clone + 'a {
    let end_at = choice((just(delim).ignored(), just('\n').ignored(), end()));
    just(delim)
        .ignore_then(just(' ').not())
        .ignore_then(content_before(element, end_at))
        .then_ignore(just(delim).or_not())
        .map(move |content| Node::Container {
            kind: ContainerKind::Style(st),
            content,
        })
}

/// `^^sup^^`.
pub(crate) fn superscript<
    'a,
    P: Parser<'a, In<'a>, (Content, Option<ContentExitReason>), E<'a>> + Clone + 'a,
>(
    element: P,
) -> impl Parser<'a, In<'a>, Node, E<'a>> + Clone + 'a {
    let end_at = choice((just("^^").ignored(), just('\n').ignored(), end()));
    just("^^")
        .ignore_then(content_before(element, end_at))
        .then_ignore(just("^^").or_not())
        .map(|sup| Node::SupSubscript {
            sup,
            sub: Vec::new(),
        })
}

/// `,,sub,,`.
pub(crate) fn subscript<
    'a,
    P: Parser<'a, In<'a>, (Content, Option<ContentExitReason>), E<'a>> + Clone + 'a,
>(
    element: P,
) -> impl Parser<'a, In<'a>, Node, E<'a>> + Clone + 'a {
    let end_at = choice((just(",,").ignored(), just('\n').ignored(), end()));
    just(",,")
        .ignore_then(content_before(element, end_at))
        .then_ignore(just(",,").or_not())
        .map(|sub| Node::SupSubscript {
            sup: Vec::new(),
            sub,
        })
}

/// `##color|text##`.
pub(crate) fn color_span<
    'a,
    P: Parser<'a, In<'a>, (Content, Option<ContentExitReason>), E<'a>> + Clone + 'a,
>(
    element: P,
) -> impl Parser<'a, In<'a>, Node, E<'a>> + Clone + 'a {
    let end_at = choice((just("##").ignored(), just('\n').ignored(), end()));
    just("##")
        .ignore_then(read_until(&["|"]))
        .then_ignore(just('|'))
        .then(content_before(element, end_at))
        .then_ignore(just("##").or_not())
        .map(|(color, content)| Node::Container {
            kind: ContainerKind::Color(normalize_color(color.to_string())),
            content,
        })
}

/// `%%name|default%%` module/listpages variable.
pub(crate) fn module_var<'a>() -> impl Parser<'a, In<'a>, Node, E<'a>> + Clone + 'a {
    just("%%")
        .ignore_then(read_until(&["%%"]))
        .then_ignore(just("%%"))
        .map(|raw: &str| match raw.split_once('|') {
            Some((n, d)) => Node::Text(TextObj::ModuleVar {
                name: n.to_string(),
                default: Some(d.to_string()),
            }),
            None => Node::Text(TextObj::ModuleVar {
                name: raw.to_string(),
                default: None,
            }),
        })
}

/// `{$name//default}` include variable. The default is currently captured as
/// raw text (full markup-in-default is a TODO).
pub(crate) fn include_var<'a>() -> impl Parser<'a, In<'a>, Node, E<'a>> + Clone + 'a {
    just("{$")
        .ignore_then(read_until(&["}"]))
        .then_ignore(just('}'))
        .map(|raw: &str| match raw.split_once("//") {
            Some((n, d)) => Node::Text(TextObj::IncludeVar {
                name: n.to_string(),
                default: Some(parse(d)),
            }),
            None => Node::Text(TextObj::IncludeVar {
                name: raw.to_string(),
                default: None,
            }),
        })
}
