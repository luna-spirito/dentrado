//! Character classes and low-level parser primitives shared across the grammar.

use super::*;

/// Characters that *might* begin a markup construct and therefore stop a plain
/// text run (PureScript `ebleSintaks = "{h/*_,^>+=|@[-\n#%"`).
pub(crate) fn is_syntax_char(c: char) -> bool {
    matches!(
        c,
        '{' | 'h'
            | '/'
            | '*'
            | '_'
            | ','
            | '^'
            | '>'
            | '+'
            | '='
            | '|'
            | '@'
            | '['
            | ']'
            | '-'
            | '\n'
            | '#'
            | '%'
    )
}

/// Characters allowed in a bare URL (PureScript `url`, plus `%`).
pub(crate) fn is_url_char(c: char) -> bool {
    c.is_ascii_alphanumeric()
        || matches!(
            c,
            '-' | '.'
                | '_'
                | '~'
                | ':'
                | '/'
                | '?'
                | '#'
                | '@'
                | '!'
                | '$'
                | '&'
                | '\''
                | '('
                | ')'
                | '*'
                | '+'
                | ','
                | ';'
                | '='
        )
}

/// Characters allowed in a property / variable name (PureScript `propPerm`).
pub(crate) fn is_prop_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '#'
}

pub(crate) fn is_hex_char(c: char) -> bool {
    c.is_ascii_hexdigit()
}

/// A zero-width assertion that succeeds at the beginning of a line (start of
/// input, or immediately after a `\n`).
///
/// PureScript tracks this via `Position { column }`; chumsky gives us a byte
/// offset into the full slice, which is enough to peek at the previous byte
/// (ASCII `\n`, so UTF-8 boundaries are respected).
pub(crate) fn at_line_start<'a>() -> impl Parser<'a, In<'a>, (), E<'a>> + Clone + 'a {
    custom(|inp: &mut InputRef<'a, '_, In<'a>, E<'a>>| {
        let full = inp.full_slice();
        let off = *inp.cursor().inner();
        let at_ls = off == 0 || full.as_bytes().get(off - 1) == Some(&b'\n');
        if at_ls {
            Ok(())
        } else {
            Err(perr(inp, "expected start of line"))
        }
    })
}

/// Build a `Rich` error at the current (zero-width) position.
pub(crate) fn perr<'a, 'b>(
    inp: &mut InputRef<'a, 'b, In<'a>, E<'a>>,
    msg: &'static str,
) -> Rich<'a, char> {
    let cur = inp.cursor();
    Rich::custom(inp.span_since(&cur), msg)
}

/// Read raw text up to (but not consuming) the earliest of the given
/// delimiters, a newline, or end of input. The returned slice borrows from the
/// input for `'a`.
pub(crate) fn read_until<'a>(
    delims: &'a [&'a str],
) -> impl Parser<'a, In<'a>, &'a str, E<'a>> + Clone + 'a {
    custom(move |inp: &mut InputRef<'a, '_, In<'a>, E<'a>>| {
        let full = inp.full_slice();
        let start = *inp.cursor().inner();
        let rest = &full[start..];
        let mut end = rest.len();
        for d in delims {
            if let Some(p) = rest.find(d) {
                end = end.min(p);
            }
        }
        if let Some(p) = rest.find('\n') {
            end = end.min(p);
        }
        let consumed = &rest[..end];
        for _ in consumed.chars() {
            let _ = inp.next();
        }
        Ok(consumed)
    })
}

/// Like [`read_until`] but does not stop at newlines — for block-level raw
/// bodies (`[[code]]`, `[[module css]]`) that span multiple lines.
pub(crate) fn read_until_lines<'a>(
    delims: &'a [&'a str],
) -> impl Parser<'a, In<'a>, &'a str, E<'a>> + Clone + 'a {
    custom(move |inp: &mut InputRef<'a, '_, In<'a>, E<'a>>| {
        let full = inp.full_slice();
        let start = *inp.cursor().inner();
        let rest = &full[start..];
        let mut end = rest.len();
        for d in delims {
            if let Some(p) = rest.find(d) {
                end = end.min(p);
            }
        }
        let consumed = &rest[..end];
        for _ in consumed.chars() {
            let _ = inp.next();
        }
        Ok(consumed)
    })
}

/// Case-insensitive ASCII keyword (PureScript `slosxilVort`). Consumes the
/// keyword on match.
pub(crate) fn kw_ci<'a>(kw: String) -> impl Parser<'a, In<'a>, (), E<'a>> + Clone + 'a {
    // All callers pass ASCII keywords, so `kw.len()` bytes == `kw.len()` chars
    // and we can compare on raw bytes. Comparing on `&str` slices here would
    // panic: `rest[..kw.len()]` requires `kw.len()` to land on a char boundary,
    // which fails as soon as a multibyte char (e.g. `…`, Cyrillic) sits at the
    // cursor before a would-be keyword.
    debug_assert!(kw.is_ascii(), "kw_ci keywords must be ASCII");
    custom(move |inp: &mut InputRef<'a, '_, In<'a>, E<'a>>| {
        let full = inp.full_slice();
        let off = *inp.cursor().inner();
        let rest_bytes = full.as_bytes().get(off..).unwrap_or(&[]);
        if rest_bytes.len() >= kw.len()
            && rest_bytes[..kw.len()].eq_ignore_ascii_case(kw.as_bytes())
        {
            for _ in 0..kw.len() {
                inp.next();
            }
            Ok(())
        } else {
            Err(perr(inp, "expected keyword"))
        }
    })
}

/// Zero or more spaces.
pub(crate) fn spaces<'a>() -> impl Parser<'a, In<'a>, (), E<'a>> + Clone + 'a {
    just(' ').repeated().ignored()
}

/// One or more spaces.
pub(crate) fn spaces1<'a>() -> impl Parser<'a, In<'a>, (), E<'a>> + Clone + 'a {
    just(' ').repeated().at_least(1).ignored()
}

/// A single trailing newline, or EOF (consumed).
pub(crate) fn line_end<'a>() -> impl Parser<'a, In<'a>, (), E<'a>> + Clone + 'a {
    choice((just('\n').ignored(), end()))
}

/// Recognize (without consuming) a closing tag `[[/KEYWORD]]` for `tag`,
/// yielding the tag back. Whitespace around the inner tokens is permitted.
pub(crate) fn closing_tag<'a>(
    tag: ClosedTag,
) -> impl Parser<'a, In<'a>, ClosedTag, E<'a>> + Clone + 'a {
    let kw = tag.opener_str();
    just("[[")
        .ignore_then(spaces())
        .ignore_then(just('/'))
        .ignore_then(spaces())
        .ignore_then(kw_ci(kw))
        .ignore_then(spaces())
        .ignore_then(just("]]"))
        .to(tag)
}

/// Parse zero or more elements until `term` matches, then consume `term` and
/// return both the content and the exit reason.
///
/// The terminator is checked at every position via [`Parser::not`] (a
/// zero-width, non-consuming assertion), so element parsers never have to worry
/// about accidentally eating into their own closing tag.
pub(crate) fn content_until<'a, P, T>(
    element: P,
    term: T,
) -> impl Parser<'a, In<'a>, (Content, ContentExitReason), E<'a>> + Clone + 'a
where
    P: Parser<'a, In<'a>, Node, E<'a>> + Clone + 'a,
    T: Parser<'a, In<'a>, ContentExitReason, E<'a>> + Clone + 'a,
{
    term.clone()
        .not()
        .ignore_then(element)
        .repeated()
        .collect::<Content>()
        .then(term)
}

/// Parse zero or more elements until `stop` matches, returning just the
/// content. The stop marker is *not* consumed — the caller handles it. Used for
/// inline contexts (style spans, cells, link text).
pub(crate) fn content_before<'a, P, S>(
    element: P,
    stop: S,
) -> impl Parser<'a, In<'a>, Content, E<'a>> + Clone + 'a
where
    P: Parser<'a, In<'a>, Node, E<'a>> + Clone + 'a,
    S: Parser<'a, In<'a>, (), E<'a>> + Clone + 'a,
{
    stop.not()
        .ignore_then(element)
        .repeated()
        .collect::<Content>()
}
