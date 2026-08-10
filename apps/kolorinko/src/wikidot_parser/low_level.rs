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

/// Recognize a closing tag `[[/KW]]` for any known tag, or a comment close
/// `--]` when `comment` is true — and **consume it**. Returns the matching
/// [`ContentExitReason`] so [`content_loop`] can report why it stopped.
///
/// Operates purely on the byte tail from the cursor: ASCII keywords and
/// delimiters only, so every slice lands on a UTF-8 boundary. Keywords are
/// matched longest-first so `[[/tabview]]` wins over `[[/tab]]` and `[[/==]]`
/// over `[[/=]]`; an unknown body (`[[/foobar]]`) is not a closer and falls
/// through to text.
pub(crate) fn closer_at(
    full: &str,
    off: usize,
    comment: bool,
) -> Option<(ContentExitReason, usize)> {
    let bytes = full.as_bytes();
    if comment && bytes[off..].starts_with(b"--]") {
        return Some((ContentExitReason::ClosedComment, 3));
    }
    if !bytes[off..].starts_with(b"[[") {
        return None;
    }
    let mut i = off + 2;
    while i < bytes.len() && bytes[i] == b' ' {
        i += 1;
    }
    if i >= bytes.len() || bytes[i] != b'/' {
        return None;
    }
    i += 1;
    while i < bytes.len() && bytes[i] == b' ' {
        i += 1;
    }
    // Known closer keywords, longest first (so prefixes don't shadow).
    const CLOSERS: &[(&[u8], ClosedTag)] = &[
        (b"collapsible", ClosedTag::Collapsible),
        (b"tabview", ClosedTag::Tabview),
        (b"iftags", ClosedTag::IfTags),
        (b"module", ClosedTag::Module),
        (b"table", ClosedTag::Table),
        (b"hcell", ClosedTag::Cell),
        (b"size", ClosedTag::Size),
        (b"span", ClosedTag::Span),
        (b"cell", ClosedTag::Cell),
        (b"code", ClosedTag::Code),
        (b"row", ClosedTag::Row),
        (b"div", ClosedTag::Div),
        (b"tab", ClosedTag::Tab),
        (
            b"f<",
            ClosedTag::Align {
                floating: true,
                side: AlignSide::Left,
            },
        ),
        (
            b"f>",
            ClosedTag::Align {
                floating: true,
                side: AlignSide::Right,
            },
        ),
        (
            b"==",
            ClosedTag::Align {
                floating: false,
                side: AlignSide::Justify,
            },
        ),
        (b"a", ClosedTag::Anchor),
        (
            b"<",
            ClosedTag::Align {
                floating: false,
                side: AlignSide::Left,
            },
        ),
        (
            b">",
            ClosedTag::Align {
                floating: false,
                side: AlignSide::Right,
            },
        ),
        (
            b"=",
            ClosedTag::Align {
                floating: false,
                side: AlignSide::Center,
            },
        ),
    ];
    let kw = &bytes[i..];
    for (key, tag) in CLOSERS {
        if kw.len() >= key.len() && kw[..key.len()].eq_ignore_ascii_case(key) {
            let mut j = i + key.len();
            while j < bytes.len() && bytes[j] == b' ' {
                j += 1;
            }
            if bytes[j..].starts_with(b"]]") {
                return Some((ContentExitReason::ClosedTag(tag.clone()), j + 2 - off));
            }
        }
    }
    None
}

/// The central, non-backtracking content loop. Parses one element at a time
/// and stops at the first end of input, closing tag `[[/…]]`, or (when
/// `comment` is true) comment close `--]` — **consuming** any closer it finds
/// and reporting which one via the returned reason.
///
/// Each element yields `(Content, Option<reason>)`. A `Some(reason)` means the
/// element was a mismatched container that hit a *foreign* closer and
/// flattened; that reason is propagated out so an ancestor can decide. This is
/// what makes `[[span]] … [[div]] … [[/span]]` close the span (the span's
/// closer was consumed at the innermost level and its reason bubbled up).
pub(crate) fn content_loop<'a, P>(
    element: P,
    comment: bool,
) -> impl Parser<'a, In<'a>, (Content, ContentExitReason), E<'a>> + Clone + 'a
where
    P: Parser<'a, In<'a>, (Content, Option<ContentExitReason>), E<'a>> + Clone + 'a,
{
    content_loop_until_closer(element, comment).map(|(nodes, _body_end, reason)| (nodes, reason))
}

/// Like [`content_loop`], but additionally reports the byte offset where the
/// body ended — i.e. where the consumed closer began (or EOF/propagated
/// element closed). Raw-bodied blocks (`code`, `module css`) slice their
/// verbatim body as `full[body_start..body_end]`; with the closer now
/// *consumed*, that slice would otherwise swallow the closer text.
pub(crate) fn content_loop_until_closer<'a, P>(
    element: P,
    comment: bool,
) -> impl Parser<'a, In<'a>, (Content, usize, ContentExitReason), E<'a>> + Clone + 'a
where
    P: Parser<'a, In<'a>, (Content, Option<ContentExitReason>), E<'a>> + Clone + 'a,
{
    custom(move |inp: &mut InputRef<'a, '_, In<'a>, E<'a>>| {
        let mut nodes = Content::new();
        loop {
            let full = inp.full_slice();
            let off = *inp.cursor().inner();
            if off >= full.len() {
                return Ok((nodes, off, ContentExitReason::Eof));
            }
            if let Some((reason, len)) = closer_at(full, off, comment) {
                for _ in 0..len {
                    let _ = inp.next();
                }
                return Ok((nodes, off, reason));
            }
            let cp = inp.save();
            match inp.parse(element.clone()) {
                Ok((sub, reason)) => {
                    nodes.extend(sub);
                    if let Some(reason) = reason {
                        return Ok((nodes, off, reason));
                    }
                }
                Err(_) => {
                    inp.rewind(cp);
                    if inp.next().is_none() {
                        return Ok((nodes, full.len(), ContentExitReason::Eof));
                    }
                }
            }
        }
    })
}

/// Lift a single-`Node` parser into the element grammar's `Content` result.
pub(crate) fn one<'a, P>(
    p: P,
) -> impl Parser<'a, In<'a>, (Content, Option<ContentExitReason>), E<'a>> + Clone + 'a
where
    P: Parser<'a, In<'a>, Node, E<'a>> + Clone + 'a,
{
    p.map(|n| (vec![n], None))
}

/// Parse a `[[KW …]] … [[/KW]]`-style block generically.
///
/// `opener` consumes the full opening tag (including its leading `[[`),
/// yielding opener data `T`. `closer` is the [`ContentExitReason`] this block
/// owns. [`content_loop`] consumes the closer it stopped at; if the reason
/// matches, `build` forms the node. Otherwise the block *flattens* — verbatim
/// opener as [`Node::Raw`] plus the body — and the foreign reason is returned
/// so an ancestor can see which closer was consumed.
pub(crate) fn balanced<'a, P, Op, T, F>(
    element: P,
    opener: Op,
    closer: ContentExitReason,
    build: F,
) -> impl Parser<'a, In<'a>, (Content, Option<ContentExitReason>), E<'a>> + Clone + 'a
where
    P: Parser<'a, In<'a>, (Content, Option<ContentExitReason>), E<'a>> + Clone + 'a,
    Op: Parser<'a, In<'a>, T, E<'a>> + Clone + 'a,
    F: Fn(T, Content) -> Node + Clone + 'a,
{
    custom(move |inp: &mut InputRef<'a, '_, In<'a>, E<'a>>| {
        let full = inp.full_slice();
        let opener_start = *inp.cursor().inner();
        let data = inp.parse(opener.clone())?;
        let opener_end = *inp.cursor().inner();
        let (body, reason) = inp.parse(content_loop(element.clone(), false))?;
        if reason == closer {
            Ok((vec![build(data, body)], None))
        } else {
            let mut out = Vec::with_capacity(body.len() + 1);
            out.push(Node::Raw(full[opener_start..opener_end].to_string()));
            out.extend(body);
            Ok((out, Some(reason)))
        }
    })
}

/// Like [`balanced`], but for *raw-bodied* blocks (`[[code]]`, `[[module css]]`,
/// `[!-- … --]`): the body is kept verbatim as a `&str` slice and handed to
/// `build` (which may trim it, turn it into a [`Node::Code`]/[`Node::Stylesheet`],
/// or — for comments — discard it by returning an empty [`Content`]). The
/// parsed body nodes are used only on the mismatch/flatten path.
///
/// `comment` selects comment mode for the inner [`content_loop_until_closer`]
/// (so `--]` also terminates the body).
pub(crate) fn raw_balanced<'a, P, Op, Od, F>(
    element: P,
    opener: Op,
    closer: ContentExitReason,
    comment: bool,
    build: F,
) -> impl Parser<'a, In<'a>, (Content, Option<ContentExitReason>), E<'a>> + Clone + 'a
where
    P: Parser<'a, In<'a>, (Content, Option<ContentExitReason>), E<'a>> + Clone + 'a,
    Op: Parser<'a, In<'a>, Od, E<'a>> + Clone + 'a,
    F: Fn(&str) -> Content + Clone + 'a,
{
    custom(move |inp: &mut InputRef<'a, '_, In<'a>, E<'a>>| {
        let full = inp.full_slice();
        let opener_start = *inp.cursor().inner();
        let _ = inp.parse(opener.clone())?;
        let body_start = *inp.cursor().inner();
        let (body, body_end, reason) =
            inp.parse(content_loop_until_closer(element.clone(), comment))?;
        if reason == closer {
            Ok((build(&full[body_start..body_end]), None))
        } else {
            let mut out = Vec::with_capacity(body.len() + 1);
            out.push(Node::Raw(full[opener_start..body_start].to_string()));
            out.extend(body);
            Ok((out, Some(reason)))
        }
    })
}

/// A container that collects typed children (`[[row]]`s of a `[[table]]`,
/// `[[tab]]`s of a `[[tabview]]`) interleaved with ordinary elements until its
/// own closer. `child` parses one typed child; on its failure the position is
/// rewound and a generic element is absorbed as stray content instead. On a
/// matching closer, `build(opener_data, children)` forms the node; on a
/// mismatch/EOF the opener flattens to [`Node::Raw`] followed by each child's
/// content ([`container_balanced`]'s `child_to_content`]) and the stray nodes,
/// and the foreign reason propagates.
pub(crate) fn container_balanced<'a, P, Op, Od, Cp, T, Cf, Bf>(
    element: P,
    opener: Op,
    closer: ContentExitReason,
    child: Cp,
    child_to_content: Cf,
    build: Bf,
) -> impl Parser<'a, In<'a>, (Content, Option<ContentExitReason>), E<'a>> + Clone + 'a
where
    P: Parser<'a, In<'a>, (Content, Option<ContentExitReason>), E<'a>> + Clone + 'a,
    Op: Parser<'a, In<'a>, Od, E<'a>> + Clone + 'a,
    Cp: Parser<'a, In<'a>, T, E<'a>> + Clone + 'a,
    Cf: Fn(T) -> Content + Clone + 'a,
    Bf: Fn(Od, Vec<T>) -> Node + Clone + 'a,
{
    custom(move |inp: &mut InputRef<'a, '_, In<'a>, E<'a>>| {
        let full = inp.full_slice();
        let opener_start = *inp.cursor().inner();
        let opener_data = inp.parse(opener.clone())?;
        let opener_end = *inp.cursor().inner();
        let mut collected: Vec<T> = Vec::new();
        let mut stray = Content::new();
        let mut stop = ContentExitReason::Eof;
        loop {
            while matches!(inp.peek(), Some(' ') | Some('\n')) {
                let _ = inp.next();
            }
            let off = *inp.cursor().inner();
            if off >= full.len() {
                break;
            }
            if let Some((reason, len)) = closer_at(full, off, false) {
                for _ in 0..len {
                    let _ = inp.next();
                }
                stop = reason;
                break;
            }
            let cp = inp.save();
            match inp.parse(child.clone()) {
                Ok(c) => collected.push(c),
                Err(_) => {
                    inp.rewind(cp);
                    let cp = inp.save();
                    match inp.parse(element.clone()) {
                        Ok((sub, _)) => stray.extend(sub),
                        Err(_) => {
                            inp.rewind(cp);
                            if inp.next().is_none() {
                                break;
                            }
                        }
                    }
                }
            }
        }
        if stop == closer {
            Ok((vec![build(opener_data, collected)], None))
        } else {
            let mut out = Vec::with_capacity(collected.len() + stray.len() + 1);
            out.push(Node::Raw(full[opener_start..opener_end].to_string()));
            for c in collected {
                out.extend(child_to_content(c));
            }
            out.extend(stray);
            Ok((out, Some(stop)))
        }
    })
}

/// Parse zero or more elements until `stop` matches (peeked, not consumed),
/// returning the content. Used for inline/line contexts (style spans,
/// headings, cells, link text) that end at a caller-supplied sigil rather than
/// at a closing tag.
pub(crate) fn content_before<'a, P, S>(
    element: P,
    stop: S,
) -> impl Parser<'a, In<'a>, Content, E<'a>> + Clone + 'a
where
    P: Parser<'a, In<'a>, (Content, Option<ContentExitReason>), E<'a>> + Clone + 'a,
    S: Parser<'a, In<'a>, (), E<'a>> + Clone + 'a,
{
    custom(move |inp: &mut InputRef<'a, '_, In<'a>, E<'a>>| {
        let mut nodes = Content::new();
        loop {
            let cp = inp.save();
            let is_stop = inp.check(stop.clone()).is_ok();
            inp.rewind(cp);
            if is_stop {
                return Ok(nodes);
            }
            let cp = inp.save();
            match inp.parse(element.clone()) {
                Ok((sub, reason)) => {
                    nodes.extend(sub);
                    if reason.is_some() {
                        return Ok(nodes);
                    }
                }
                Err(_) => {
                    inp.rewind(cp);
                    if inp.next().is_none() {
                        return Ok(nodes);
                    }
                }
            }
        }
    })
}
