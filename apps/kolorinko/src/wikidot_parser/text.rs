//! Plain text runs, raw escapes, and bare / single-bracket links.

use super::*;

/// A maximal run of characters that cannot begin any markup.
pub(crate) fn text_run<'a>() -> impl Parser<'a, In<'a>, Node, E<'a>> + Clone + 'a {
    any::<In<'a>, E<'a>>()
        .filter(|c: &char| !is_syntax_char(*c))
        .repeated()
        .at_least(1)
        .collect::<String>()
        .map(|s| Node::Text(TextObj::Plain(s)))
}

/// `@@…@@` raw escape. The body is taken verbatim up to the next `@@` or EOL.
pub(crate) fn raw_escape<'a>() -> impl Parser<'a, In<'a>, Node, E<'a>> + Clone + 'a {
    just("@@")
        .ignore_then(read_until(&["@@"]).map(|s| Node::Text(TextObj::Plain(s.to_string()))))
        .then_ignore(just("@@").or_not())
}

/// `[!-- … --]` comment, routed through the content loop like any other block.
///
/// After the `[!--` opener, [`content_loop`] runs in comment mode (so it also
/// stops at `--]`). If it lands on the comment close, the parsed body is
/// discarded. If it stops on anything else (EOF, or a foreign `[[/…]]` closer
/// that cut the comment short — the only way a comment “fails”), the opener is
/// emitted verbatim as a [`Node::Raw`] followed by the body, exactly as a
/// broken block: nothing swallows the rest of the page.
pub(crate) fn comment<'a, P>(
    element: P,
) -> impl Parser<'a, In<'a>, (Content, Option<ContentExitReason>), E<'a>> + Clone + 'a
where
    P: Parser<'a, In<'a>, (Content, Option<ContentExitReason>), E<'a>> + Clone + 'a,
{
    raw_balanced(
        element,
        just("[!--"),
        ContentExitReason::ClosedComment,
        true,
        |_| Content::new(),
    )
}

/// Bare `http://` / `https://` URL that becomes a link whose text is the URL.
pub(crate) fn bare_http_link<'a>() -> impl Parser<'a, In<'a>, Node, E<'a>> + Clone + 'a {
    just("http")
        .ignore_then(just('s').or_not())
        .then_ignore(just("://"))
        .then(
            any::<In<'a>, E<'a>>()
                .filter(|c: &char| is_url_char(*c) || *c == '%')
                .repeated()
                .at_least(1)
                .collect::<String>(),
        )
        .map(|(secure, rest)| {
            let scheme = if secure.is_some() { "https" } else { "http" };
            let url = format!("{scheme}://{rest}");
            Node::Link {
                target: LinkTarget::Url(url.clone()),
                text: vec![Node::Text(TextObj::Plain(url))],
            }
        })
}

/// `[url text]` / `[url]` single-bracket link (e.g. `[/ Main]`,
/// `[http://x click]`). Rejected when preceded by `[` (so the inner bracket of
/// a `[[\u{2026}]]` construct like `[[toc]]` is not swallowed) and when the `[`
/// is followed by `[`, `!` (a comment) or `]`.
pub(crate) fn single_bracket_link<'a>() -> impl Parser<'a, In<'a>, Node, E<'a>> + Clone + 'a {
    custom(|inp: &mut InputRef<'a, '_, In<'a>, E<'a>>| {
        let full = inp.full_slice();
        let off = *inp.cursor().inner();
        let prev = off.checked_sub(1).and_then(|i| full.as_bytes().get(i));
        let next = full.as_bytes().get(off).copied();
        let after = full.as_bytes().get(off + 1).copied();
        if prev == Some(&b'[')
            || next != Some(b'[')
            || matches!(
                after,
                Some(b'[') | Some(b'!') | Some(b']') | Some(b'\n') | None
            )
        {
            return Err(perr(inp, "not a single-bracket link"));
        }
        let _ = inp.next(); // consume '['
        let raw = inp.parse(read_until(&[" ", "]"]))?.to_string();
        let text = match inp.peek() {
            Some(']') => {
                let _ = inp.next();
                Vec::new()
            }
            Some(' ') => {
                let _ = inp.next();
                let rest2 = &full[*inp.cursor().inner()..];
                let bytes2 = rest2.as_bytes();
                let mut t_end = 0;
                while t_end < bytes2.len() && bytes2[t_end] != b']' {
                    t_end += 1;
                }
                let t = rest2[..t_end].trim().to_string();
                for _ in 0..t_end {
                    let _ = inp.next();
                }
                if matches!(inp.peek(), Some(']')) {
                    let _ = inp.next();
                }
                vec![Node::Text(TextObj::Plain(t))]
            }
            _ => Vec::new(),
        };
        let target = parse_link_target(&raw);
        let text = if text.is_empty() {
            vec![Node::Text(TextObj::Plain(raw))]
        } else {
            text
        };
        Ok(Node::Link { target, text })
    })
}
