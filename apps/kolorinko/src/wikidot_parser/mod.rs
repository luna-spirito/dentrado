//! Wikidot markup parser, built on `chumsky` 0.13.
//!
//! The grammar follows the reference at
//! <https://www.wikidot.com/doc-wiki-syntax:inline-formatting> and is a port of
//! the original PureScript modules `Pagx.Hered.Tipoj` / `Pagx.Hered.Analiz`.
//!
//! ## Architecture
//!
//! The central combinator is [`content_until`]: it parses a stream of [`Node`]s
//! until a *terminator* is reached, then consumes the terminator and reports
//! *why* parsing stopped (via [`ContentExitReason`]). Every container in the
//! language (`[[div]]`, `[[size]]`, a table cell, a style span, …) parses its
//! body with a terminator specialized to its closing construct, while the
//! top-level page parse uses EOF as its terminator. [`content_before`] is the
//! non-consuming variant for inline contexts (style spans, link text).
//!
//! The element grammar is left-recursive through containers, so the element
//! parser is tied into a knot with [`recursive`] inside [`build_element`].
//!
//! ## Input
//!
//! Input is `&'src str`, not `&[u8]`: Wikidot pages are UTF-8 with plenty of
//! non-ASCII (Cyrillic, etc.), and operating on `&str` lets us slice, search
//! and match characters directly.
//!
//! ## Graceful degradation
//!
//! Like the original, the parser is total: any input parses to *something*.
//! Unrecognized `[[…]]` constructs and stray sigils fall through to a
//! single-character fallback that becomes plain text, and a final
//! [`merge_text`] pass fuses the resulting fragments back together (so e.g. an
//! unknown `[[toc]]` reassembles into a single text node rather than seven).

use chumsky::{input::InputRef, prelude::*};
use std::collections::HashMap;

use crate::wikidot_parser::types::{
    Align, AlignSide, BlockCell, BlockRow, BlockTable, ContainerKind, Content, Include, LinkTarget,
    List, ListItem, ListPages, ListPagesParams, Node, PageRef, TableCell, TextObj, TextStyle,
};

pub mod types;

// =========================================================================
// Tags & exit reasons
// =========================================================================

/// The block-level opening tags that pair with a matching `[[/…]]` closer.
///
/// These are the only constructs whose body is parsed with a *dedicated*
/// closing terminator; everything else either self-closes (`[[image …]]`) or is
/// inline.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ClosedTag {
    Div,
    Span,
    Size,
    IfTags,
    /// `[[module …]] … [[/module]]`. Covers `css`, `ListPages` and (in
    /// principle) any other module — the body is dispatched on the name.
    Module,
    Tab,
    Tabview,
    /// `[[<]]`, `[[=]]`, `[[>]]`, `[[==]]`, `[[f<]]`, `[[f>]]`. The closer
    /// mirrors the opener exactly (`[[/f<]]`, `[[/==]]`, …).
    Align {
        floating: bool,
        side: AlignSide,
    },
}

impl ClosedTag {
    /// The keyword/sequence after `[[` (and after `[[/` in the closer), used to
    /// recognize the matching closing tag.
    fn opener_str(&self) -> String {
        match self {
            ClosedTag::Div => "div".into(),
            ClosedTag::Span => "span".into(),
            ClosedTag::Size => "size".into(),
            ClosedTag::IfTags => "iftags".into(),
            ClosedTag::Module => "module".into(),
            ClosedTag::Tab => "tab".into(),
            ClosedTag::Tabview => "tabview".into(),
            ClosedTag::Align { floating, side } => {
                let f = if *floating { "f" } else { "" };
                let s = match side {
                    AlignSide::Left => "<",
                    AlignSide::Center => "=",
                    AlignSide::Right => ">",
                    AlignSide::Justify => "==",
                };
                format!("{f}{s}")
            }
        }
    }
}

/// Why a [`content_until`] run stopped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ContentExitReason {
    /// Reached end of input.
    Eof,
    /// Recognized and consumed the matching closing tag.
    EndOfTag(ClosedTag),
}

// =========================================================================
// Type aliases
// =========================================================================

/// Parser input: a borrowed slice of the source page.
pub type In<'a> = &'a str;

/// Default parse extra: [`Rich`] errors over `char` tokens.
pub type E<'a> = extra::Err<Rich<'a, char>>;

// =========================================================================
// Public entry points
// =========================================================================

/// Top-level content parser: parses a whole page until EOF.
///
/// Matches the skeleton signature, but over `&str` rather than `&[u8]` (see the
/// module docs).
pub fn content<'a>() -> impl Parser<'a, In<'a>, (Content, ContentExitReason), E<'a>> + Clone + 'a {
    let element = build_element();
    content_until(element, end().to(ContentExitReason::Eof))
}

/// Parse a whole page, fusing adjacent text fragments with [`merge_text`].
///
/// Errors are collected but currently discarded (the parser is total and
/// produces output regardless); a future revision can surface them.
pub fn parse(input: &str) -> Content {
    let (content, _reason) = content()
        .parse(input)
        .into_result()
        .unwrap_or((Vec::new(), ContentExitReason::Eof));
    merge_text(content)
}

// =========================================================================
// Character classes
// =========================================================================

/// Characters that *might* begin a markup construct and therefore stop a plain
/// text run (PureScript `ebleSintaks = "{h/*_,^>+=|@[-\n#%"`).
fn is_syntax_char(c: char) -> bool {
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
fn is_url_char(c: char) -> bool {
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
fn is_prop_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '#'
}

fn is_hex_char(c: char) -> bool {
    c.is_ascii_hexdigit()
}

// =========================================================================
// Low-level custom parsers
// =========================================================================

/// A zero-width assertion that succeeds at the beginning of a line (start of
/// input, or immediately after a `\n`).
///
/// PureScript tracks this via `Position { column }`; chumsky gives us a byte
/// offset into the full slice, which is enough to peek at the previous byte
/// (ASCII `\n`, so UTF-8 boundaries are respected).
fn at_line_start<'a>() -> impl Parser<'a, In<'a>, (), E<'a>> + Clone + 'a {
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
fn perr<'a, 'b>(inp: &mut InputRef<'a, 'b, In<'a>, E<'a>>, msg: &'static str) -> Rich<'a, char> {
    let cur = inp.cursor();
    Rich::custom(inp.span_since(&cur), msg)
}

/// Read raw text up to (but not consuming) the earliest of the given
/// delimiters, a newline, or end of input. The returned slice borrows from the
/// input for `'a`.
fn read_until<'a>(delims: &'a [&'a str]) -> impl Parser<'a, In<'a>, &'a str, E<'a>> + Clone + 'a {
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
fn read_until_lines<'a>(
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
fn kw_ci<'a>(kw: String) -> impl Parser<'a, In<'a>, (), E<'a>> + Clone + 'a {
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
fn spaces<'a>() -> impl Parser<'a, In<'a>, (), E<'a>> + Clone + 'a {
    just(' ').repeated().ignored()
}

/// One or more spaces.
fn spaces1<'a>() -> impl Parser<'a, In<'a>, (), E<'a>> + Clone + 'a {
    just(' ').repeated().at_least(1).ignored()
}

/// A single trailing newline, or EOF (consumed).
fn line_end<'a>() -> impl Parser<'a, In<'a>, (), E<'a>> + Clone + 'a {
    choice((just('\n').ignored(), end()))
}

/// Recognize (without consuming) a closing tag `[[/KEYWORD]]` for `tag`,
/// yielding the tag back. Whitespace around the inner tokens is permitted.
fn closing_tag<'a>(tag: ClosedTag) -> impl Parser<'a, In<'a>, ClosedTag, E<'a>> + Clone + 'a {
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

// =========================================================================
// Content loops
// =========================================================================

/// Parse zero or more elements until `term` matches, then consume `term` and
/// return both the content and the exit reason.
///
/// The terminator is checked at every position via [`Parser::not`] (a
/// zero-width, non-consuming assertion), so element parsers never have to worry
/// about accidentally eating into their own closing tag.
fn content_until<'a, P, T>(
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
fn content_before<'a, P, S>(
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

// =========================================================================
// Element grammar (recursive knot)
// =========================================================================

/// The single-element parser, tied into a knot with [`recursive`] so containers
/// can recurse.
fn build_element<'a>() -> impl Parser<'a, In<'a>, Node, E<'a>> + Clone + 'a {
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
            just('[').ignore_then(just('[').ignore_then(bracket_syntax(element.clone()))),
            // Inline markup: `//`, `**`, `__`, `--`, `^^`, `,,`, `##`, vars.
            inline_syntax(element.clone()),
            // Fallback: a single arbitrary character (graceful degradation).
            any::<In<'a>, E<'a>>().map(|c| Node::Text(TextObj::Plain(c.to_string()))),
        ))
    })
}

// =========================================================================
// Text runs & escapes
// =========================================================================

/// A maximal run of characters that cannot begin any markup.
fn text_run<'a>() -> impl Parser<'a, In<'a>, Node, E<'a>> + Clone + 'a {
    any::<In<'a>, E<'a>>()
        .filter(|c: &char| !is_syntax_char(*c))
        .repeated()
        .at_least(1)
        .collect::<String>()
        .map(|s| Node::Text(TextObj::Plain(s)))
}

/// `@@…@@` raw escape. The body is taken verbatim up to the next `@@` or EOL.
fn raw_escape<'a>() -> impl Parser<'a, In<'a>, Node, E<'a>> + Clone + 'a {
    just("@@")
        .ignore_then(read_until(&["@@"]).map(|s| Node::Text(TextObj::Plain(s.to_string()))))
        .then_ignore(just("@@").or_not())
}

/// Bare `http://` / `https://` URL that becomes a link whose text is the URL.
fn bare_http_link<'a>() -> impl Parser<'a, In<'a>, Node, E<'a>> + Clone + 'a {
    just("http")
        .ignore_then(just('s').or_not())
        .ignore_then(just("://"))
        .ignore_then(
            any::<In<'a>, E<'a>>()
                .filter(|c: &char| is_url_char(*c) || *c == '%')
                .repeated()
                .at_least(1)
                .collect::<String>(),
        )
        .map(|url| Node::Link {
            target: LinkTarget::Url(url.clone()),
            text: vec![Node::Text(TextObj::Plain(url))],
        })
}

/// `[url text]` / `[url]` single-bracket link (e.g. `[/ Main]`,
/// `[http://x click]`). Rejected when preceded by `[` (so the inner bracket of
/// a `[[\u{2026}]]` construct like `[[toc]]` is not swallowed) and when the `[`
/// is followed by `[`, `!` (a comment) or `]`.
fn single_bracket_link<'a>() -> impl Parser<'a, In<'a>, Node, E<'a>> + Clone + 'a {
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
        let rest = &full[*inp.cursor().inner()..];
        let bytes = rest.as_bytes();
        let mut url_end = 0;
        while url_end < bytes.len() && !matches!(bytes[url_end], b' ' | b'\n' | b']') {
            url_end += 1;
        }
        let raw = rest[..url_end].to_string();
        for _ in 0..url_end {
            let _ = inp.next();
        }
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
// =========================================================================
// Line-start block constructs
// =========================================================================

/// All constructs that may only appear at the beginning of a line.
fn line_syntax<'a, P: Parser<'a, In<'a>, Node, E<'a>> + Clone + 'a>(
    element: P,
) -> impl Parser<'a, In<'a>, Node, E<'a>> + Clone + 'a {
    at_line_start().ignore_then(choice((
        heading(element.clone()),
        hr(),
        table_block(element.clone()),
        blockquote(element.clone()),
        centered_line(element.clone()),
        list_block(element),
    )))
}

/// `+` … `++++++` heading. Body is the rest of the line.
fn heading<'a, P: Parser<'a, In<'a>, Node, E<'a>> + Clone + 'a>(
    element: P,
) -> impl Parser<'a, In<'a>, Node, E<'a>> + Clone + 'a {
    just('+')
        .repeated()
        .at_least(1)
        .at_most(6)
        .collect::<String>()
        .map(|s: String| s.len() as u32)
        .then_ignore(spaces1())
        .then(content_before(element, line_end()))
        .then_ignore(line_end())
        .map(|(level, content)| Node::Heading { level, content })
}

/// `----` (four or more dashes) horizontal rule.
fn hr<'a>() -> impl Parser<'a, In<'a>, Node, E<'a>> + Clone + 'a {
    just("----")
        .ignore_then(just('-').repeated().ignored())
        .then_ignore(line_end())
        .to(Node::HorizontalRule)
}

/// A `||…||…` table: one or more consecutive `||`-prefixed lines. Cells are
/// separated by `||`; each cell may begin with `~` (header) and an alignment
/// marker (`<` / `=` / `>`).
fn table_block<'a, P: Parser<'a, In<'a>, Node, E<'a>> + Clone + 'a>(
    element: P,
) -> impl Parser<'a, In<'a>, Node, E<'a>> + Clone + 'a {
    custom(move |inp: &mut InputRef<'a, '_, In<'a>, E<'a>>| {
        let mut rows: Vec<Vec<TableCell>> = Vec::new();
        let cell_stop = choice((just("||").ignored(), just('\n').ignored(), end()));
        loop {
            let full = inp.full_slice();
            let off = *inp.cursor().inner();
            let at_ls = off == 0 || full.as_bytes().get(off - 1) == Some(&b'\n');
            if !at_ls || !full[off..].starts_with("||") {
                break;
            }
            inp.next();
            inp.next(); // consume opening "|"
            let mut row: Vec<TableCell> = Vec::new();
            loop {
                // cell: ~header? align? content
                let header = matches!(inp.peek(), Some('~'));
                if header {
                    inp.next();
                }
                while matches!(inp.peek(), Some(' ')) {
                    inp.next();
                }
                let side = match inp.peek() {
                    Some('<') => {
                        inp.next();
                        Some(AlignSide::Left)
                    }
                    Some('>') => {
                        inp.next();
                        Some(AlignSide::Right)
                    }
                    Some('=') => {
                        inp.next();
                        Some(AlignSide::Center)
                    }
                    _ => None,
                };
                while matches!(inp.peek(), Some(' ')) {
                    inp.next();
                }
                let content = inp
                    .parse(content_before(element.clone(), cell_stop.clone()))
                    .unwrap_or_default();
                row.push(TableCell {
                    colspan: 1,
                    header,
                    align: side.map(|s| Align {
                        floating: false,
                        side: s,
                    }),
                    content,
                });
                // Now at "||", "\n", or EOF.
                let f = inp.full_slice();
                let o = *inp.cursor().inner();
                if f[o..].starts_with("||") {
                    inp.next();
                    inp.next();
                    // Trailing "||" right before newline/EOF ends the row.
                    if matches!(inp.peek(), Some('\n')) {
                        inp.next();
                        break;
                    }
                    if inp.peek().is_none() {
                        break;
                    }
                    continue;
                } else if matches!(inp.peek(), Some('\n')) {
                    inp.next();
                    break;
                } else {
                    break; // EOF
                }
            }
            rows.push(row);
        }
        if rows.is_empty() {
            return Err(perr(inp, "expected table"));
        }
        Ok(Node::Table(rows))
    })
}

/// One or more `>` blockquote lines merged into a single quote container.
fn blockquote<'a, P: Parser<'a, In<'a>, Node, E<'a>> + Clone + 'a>(
    element: P,
) -> impl Parser<'a, In<'a>, Node, E<'a>> + Clone + 'a {
    let line = just('>')
        .repeated()
        .at_least(1)
        .ignored()
        .ignore_then(spaces())
        .ignore_then(content_before(element, line_end()))
        .then_ignore(line_end());

    line.repeated()
        .at_least(1)
        .collect::<Vec<Content>>()
        .map(|lines| {
            let mut content = Content::new();
            for (i, mut line) in lines.into_iter().enumerate() {
                if i > 0 {
                    content.push(Node::Text(TextObj::Plain("\n".to_string())));
                }
                content.append(&mut line);
            }
            Node::Container {
                kind: ContainerKind::Quote,
                content,
            }
        })
}

/// `= text` — a single centered line.
fn centered_line<'a, P: Parser<'a, In<'a>, Node, E<'a>> + Clone + 'a>(
    element: P,
) -> impl Parser<'a, In<'a>, Node, E<'a>> + Clone + 'a {
    just('=')
        .ignore_then(spaces())
        .ignore_then(content_before(element, line_end()))
        .then_ignore(line_end())
        .map(|content| Node::Container {
            kind: ContainerKind::Align(Align {
                floating: false,
                side: AlignSide::Center,
            }),
            content,
        })
}
/// `* item` / `# item` bullet lists, nestable by leading-space indentation.
/// Consecutive lines (one or more) form the list; a line without a marker
/// (or non-increasing indentation) ends it. Each item's body is parsed as
/// inline markup; deeper-indented lines become a [`ListItem::sublist`].
fn list_block<'a, P: Parser<'a, In<'a>, Node, E<'a>> + Clone + 'a>(
    element: P,
) -> impl Parser<'a, In<'a>, Node, E<'a>> + Clone + 'a {
    custom(move |inp: &mut InputRef<'a, '_, In<'a>, E<'a>>| {
        let element = element.clone();
        let mut lines: Vec<(usize, bool, Content)> = Vec::new();
        loop {
            let full = inp.full_slice();
            let off = *inp.cursor().inner();
            let at_ls = off == 0 || full.as_bytes().get(off - 1) == Some(&b'\n');
            if !at_ls {
                break;
            }
            let rest = &full[off..];
            // Count leading indentation: a regular space or a non-breaking
            // space (U+00A0) — Wikidot authors indent sub-items with NBSP.
            let mut chars = rest.chars();
            let mut indent = 0;
            let mut peek = chars.next();
            while matches!(peek, Some(' ') | Some('\u{00A0}')) {
                indent += 1;
                peek = chars.next();
            }
            let ordered = match peek {
                Some('*') => false,
                Some('#') => true,
                _ => break,
            };
            // `##color##` and `**bold**` are inline markup, not lists: a marker
            // immediately followed by the same character is not a list item.
            if chars.next() == peek {
                break;
            }
            for _ in 0..(indent + 1) {
                let _ = inp.next();
            }
            while matches!(inp.peek(), Some(' ') | Some('\u{00A0}')) {
                let _ = inp.next();
            }
            let content = inp
                .parse(content_before(element.clone(), line_end()))
                .unwrap_or_default();
            let _ = inp.parse(line_end());
            lines.push((indent, ordered, content));
        }
        if lines.is_empty() {
            return Err(perr(inp, "expected list"));
        }
        Ok(Node::List(build_list(&lines)))
    })
}

/// Fold flat indented list lines into a nested [`List`]. Lines at the minimum
/// indent are top-level items; each is followed by its deeper-indented run
/// (which becomes the item's `sublist`).
fn build_list(lines: &[(usize, bool, Content)]) -> List {
    let root_indent = lines[0].0;
    let mut items: Vec<ListItem> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let (_, _, content) = &lines[i];
        let mut child: Vec<(usize, bool, Content)> = Vec::new();
        let mut j = i + 1;
        while j < lines.len() && lines[j].0 > root_indent {
            child.push((lines[j].0, lines[j].1, lines[j].2.clone()));
            j += 1;
        }
        let sublist = if child.is_empty() {
            None
        } else {
            Some(Box::new(build_list(&child)))
        };
        items.push(ListItem {
            content: content.clone(),
            sublist,
        });
        i = j;
    }
    List {
        ordered: lines[0].1,
        items,
    }
}

// =========================================================================
// Bracketed `[[…]]` syntax
// =========================================================================

/// `[[a href="url" …]] body [[/a]]` — an explicit anchor. The `href` attribute
/// is used verbatim (no site-prefixing); the body is inline wikitext.
fn anchor_block<'a, P: Parser<'a, In<'a>, Node, E<'a>> + Clone + 'a>(
    element: P,
) -> impl Parser<'a, In<'a>, Node, E<'a>> + Clone + 'a {
    let close = tag_close("a").to(ContentExitReason::Eof);
    kw_ci("a".to_string())
        .ignore_then(params_block())
        .then_ignore(spaces())
        .then_ignore(just("]]"))
        .then(content_until(element, close))
        .map(|(params, (content, _))| {
            let href = params
                .get("href")
                .and_then(|v| v.first())
                .and_then(|t| match t {
                    TextObj::Plain(s) => Some(s.clone()),
                    _ => None,
                })
                .unwrap_or_else(|| "#".to_string());
            Node::Link {
                target: LinkTarget::Url(href),
                text: content,
            }
        })
}

/// Dispatch over everything that can follow `[[`.
fn bracket_syntax<'a, P: Parser<'a, In<'a>, Node, E<'a>> + Clone + 'a>(
    element: P,
) -> impl Parser<'a, In<'a>, Node, E<'a>> + Clone + 'a {
    choice((
        // `[[[target|text]]]` / `[[[target]]]`. The third `[` is consumed here.
        just('[').ignore_then(link(element.clone())),
        div_span_block(element.clone()),
        anchor_block(element.clone()),
        grid_table_block(element.clone()),
        grid_cell_block(element.clone()),
        align_block(element.clone()),
        size_block(element.clone()),
        iftags_block(element.clone()),
        module_block(element.clone()),
        tabview_block(element.clone()),
        include_block(),
        image_block(),
        code_block(),
        collapsible_block(element.clone()),
    ))
}

/// `[[[target|text]]]` / `[[[target]]]`. The caller has consumed `[[[`.
fn link<'a, P: Parser<'a, In<'a>, Node, E<'a>> + Clone + 'a>(
    element: P,
) -> impl Parser<'a, In<'a>, Node, E<'a>> + Clone + 'a {
    let target = read_until(&["|", "]]]"]).map(|s| s.trim().to_string());
    let text = just('|').ignore_then(content_before(element, just("]]]").ignored()));

    target
        .then(text.or_not())
        .then_ignore(just("]]]"))
        .map(|(raw, text)| {
            let target = parse_link_target(&raw);
            let text = text.unwrap_or_else(|| vec![Node::Text(TextObj::Plain(raw))]);
            Node::Link { target, text }
        })
}

/// `[[div …]] … [[/div]]` / `[[span …]] … [[/span]]`.
fn div_span_block<'a, P: Parser<'a, In<'a>, Node, E<'a>> + Clone + 'a>(
    element: P,
) -> impl Parser<'a, In<'a>, Node, E<'a>> + Clone + 'a {
    let div = div_open()
        .then(content_until(
            element.clone(),
            closing_tag(ClosedTag::Div).to(ContentExitReason::EndOfTag(ClosedTag::Div)),
        ))
        .map(|((underscore, params), (content, _))| Node::Container {
            kind: ContainerKind::Div {
                inline: false,
                block: !underscore,
                params,
            },
            content,
        });
    let span = container_open("span")
        .then(content_until(
            element,
            closing_tag(ClosedTag::Span).to(ContentExitReason::EndOfTag(ClosedTag::Span)),
        ))
        .map(|(params, (content, _))| Node::Container {
            kind: ContainerKind::Div {
                inline: true,
                block: false,
                params,
            },
            content,
        });
    div.or(span)
}

/// `[[div _? params ]]` open tag, returning whether the `div_` (no-paragraph)
/// underscore was present and the attribute map.
fn div_open<'a>()
-> impl Parser<'a, In<'a>, (bool, HashMap<String, Vec<TextObj>>), E<'a>> + Clone + 'a {
    kw_ci("div".to_string())
        .ignore_then(just('_').or_not().map(|opt| opt.is_some()))
        .then(params_block())
        .then_ignore(spaces())
        .then_ignore(just("]]"))
}

/// `[[table …]] … [[row …]] … [[/row]] … [[/table]]` grid table. The leading
/// `[[` of `[[table]]` is consumed by [`bracket_syntax`]; each `[[row]]` opener
/// consumes its own `[[`. A row's body is generic content in which
/// `[[cell]]` / `[[hcell]]` appear as [`Node::BlockCell`] nodes — often wrapped
/// in `[[iftags]]` conditionals — produced by [`grid_cell_block`].
fn grid_table_block<'a, P: Parser<'a, In<'a>, Node, E<'a>> + Clone + 'a>(
    element: P,
) -> impl Parser<'a, In<'a>, Node, E<'a>> + Clone + 'a {
    let ws = choice((just(' ').ignored(), just('\n').ignored()))
        .repeated()
        .ignored();
    let row = just("[[")
        .ignore_then(spaces())
        .ignore_then(kw_ci("row".into()))
        .ignore_then(params_block())
        .then_ignore(spaces())
        .then_ignore(just("]]"))
        .then(content_until(
            element.clone(),
            tag_close("row").to(ContentExitReason::Eof),
        ))
        .map(|(params, (content, _))| BlockRow { params, content });
    container_open("table")
        .then(
            ws.clone()
                .ignore_then(row.separated_by(ws.clone()).collect::<Vec<_>>())
                .then_ignore(ws),
        )
        .then_ignore(tag_close("table"))
        .map(|(params, rows)| Node::BlockTable(BlockTable { params, rows }))
}

/// `[[cell …]] … [[/cell]]` (`<td>`) or `[[hcell …]] … [[/hcell]]` (`<th>`),
/// closed by either `[[/cell]]` or `[[/hcell]]`. Registered in
/// [`bracket_syntax`] (not just inside the table) so that cells are recognised
/// when wrapped in `[[iftags]]` conditionals within a grid-table row.
fn grid_cell_block<'a, P: Parser<'a, In<'a>, Node, E<'a>> + Clone + 'a>(
    element: P,
) -> impl Parser<'a, In<'a>, Node, E<'a>> + Clone + 'a {
    let cell_close = just("[[")
        .ignore_then(spaces())
        .ignore_then(just('/'))
        .ignore_then(spaces())
        .ignore_then(choice((kw_ci("cell".into()), kw_ci("hcell".into()))))
        .ignore_then(spaces())
        .ignore_then(just("]]"))
        .to(ContentExitReason::Eof);
    choice((
        kw_ci("hcell".into()).to(true),
        kw_ci("cell".into()).to(false),
    ))
    .then(params_block())
    .then_ignore(spaces())
    .then_ignore(just("]]"))
    .then(content_until(element, cell_close))
    .map(|((header, params), (content, _))| {
        Node::BlockCell(BlockCell {
            header,
            params,
            content,
        })
    })
}

/// `[[/KW]]` closing tag, tolerant of inner whitespace.
fn tag_close<'a>(kw: &'static str) -> impl Parser<'a, In<'a>, (), E<'a>> + Clone + 'a {
    just("[[")
        .ignore_then(spaces())
        .ignore_then(just('/'))
        .ignore_then(spaces())
        .ignore_then(kw_ci(kw.to_string()))
        .ignore_then(spaces())
        .ignore_then(just("]]"))
        .to(())
}

/// Parse `[[KW _? params ]]` for an inline/block container, returning the
/// attribute map.
fn container_open<'a>(
    kw: &'static str,
) -> impl Parser<'a, In<'a>, HashMap<String, Vec<TextObj>>, E<'a>> + Clone + 'a {
    kw_ci(kw.to_string())
        .ignore_then(just('_').or_not().ignored())
        .ignore_then(params_block())
        .then_ignore(spaces())
        .then_ignore(just("]]"))
}

/// `[[<]]` / `[[=]]` / `[[>]]` / `[[==]]` / `[[f<]]` / `[[f>]]` alignment
/// blocks. The six forms are enumerated so the closer can be built from
/// compile-time-known data.
fn align_block<'a, P: Parser<'a, In<'a>, Node, E<'a>> + Clone + 'a>(
    element: P,
) -> impl Parser<'a, In<'a>, Node, E<'a>> + Clone + 'a {
    choice((
        align_case(element.clone(), "f<", true, AlignSide::Left),
        align_case(element.clone(), "f>", true, AlignSide::Right),
        align_case(element.clone(), "<", false, AlignSide::Left),
        align_case(element.clone(), ">", false, AlignSide::Right),
        align_case(element.clone(), "==", false, AlignSide::Justify),
        align_case(element, "=", false, AlignSide::Center),
    ))
}

fn align_case<'a, P: Parser<'a, In<'a>, Node, E<'a>> + Clone + 'a>(
    element: P,
    opener: &'static str,
    floating: bool,
    side: AlignSide,
) -> impl Parser<'a, In<'a>, Node, E<'a>> + Clone + 'a {
    let tag = ClosedTag::Align { floating, side };
    just(opener)
        .ignore_then(just("]]"))
        .ignore_then(content_until(
            element,
            closing_tag(tag.clone()).to(ContentExitReason::EndOfTag(tag)),
        ))
        .map(move |(content, _)| Node::Container {
            kind: ContainerKind::Align(Align { floating, side }),
            content,
        })
}

/// `[[size ARG]] … [[/size]]`.
fn size_block<'a, P: Parser<'a, In<'a>, Node, E<'a>> + Clone + 'a>(
    element: P,
) -> impl Parser<'a, In<'a>, Node, E<'a>> + Clone + 'a {
    kw_ci("size".into())
        .ignore_then(spaces1())
        .ignore_then(read_until(&["]]"]).map(|s| s.trim().to_string()))
        .then_ignore(just("]]"))
        .then(content_until(
            element,
            closing_tag(ClosedTag::Size).to(ContentExitReason::EndOfTag(ClosedTag::Size)),
        ))
        .map(|(arg, (content, _))| Node::Container {
            kind: ContainerKind::Size(arg),
            content,
        })
}

/// `[[iftags +a -b c]] … [[/iftags]]`.
fn iftags_block<'a, P: Parser<'a, In<'a>, Node, E<'a>> + Clone + 'a>(
    element: P,
) -> impl Parser<'a, In<'a>, Node, E<'a>> + Clone + 'a {
    kw_ci("iftags".into())
        .ignore_then(spaces1())
        .ignore_then(read_until(&["]]"]).map(|s| s.to_string()))
        .then_ignore(just("]]"))
        .then(content_until(
            element,
            closing_tag(ClosedTag::IfTags).to(ContentExitReason::EndOfTag(ClosedTag::IfTags)),
        ))
        .map(|(tags_raw, (content, _))| {
            let (has_all, has_none) = parse_tag_filter(&tags_raw);
            Node::Container {
                kind: ContainerKind::IfTags { has_all, has_none },
                content,
            }
        })
}

/// `[[collapsible show="…" hide="…"]] … [[/collapsible]]`. The body is parsed
/// wikitext, shown expanded (a static mirror has no JS); `show`/`hide` labels
/// are discarded. Modelled as a `collapsible-block` div so user themes apply.
fn collapsible_block<'a, P: Parser<'a, In<'a>, Node, E<'a>> + Clone + 'a>(
    element: P,
) -> impl Parser<'a, In<'a>, Node, E<'a>> + Clone + 'a {
    let close = tag_close("collapsible").to(ContentExitReason::Eof);
    kw_ci("collapsible".to_string())
        .ignore_then(params_block())
        .then_ignore(spaces())
        .then_ignore(just("]]"))
        .ignore_then(content_until(element, close))
        .map(|(content, _)| Node::Container {
            kind: ContainerKind::Div {
                inline: false,
                block: true,
                params: [(
                    "class".to_string(),
                    vec![TextObj::Plain("collapsible-block".to_string())],
                )]
                .into(),
            },
            content,
        })
}

/// `[[code]] … [[/code]]` — verbatim source, taken raw (no wikitext parsing)
/// up to the closer. Optional `type="lang"` and other params on the open tag
/// are skipped.
fn code_block<'a>() -> impl Parser<'a, In<'a>, Node, E<'a>> + Clone + 'a {
    kw_ci("code".into())
        .ignore_then(read_until(&["]]"]).ignored())
        .then_ignore(just("]]"))
        .ignore_then(read_until_lines(&["[[/code"]).map(|s| s.to_string()))
        .then_ignore(choice((just("[[/code]]").ignored(), end())))
        .map(|s| Node::Code(s.trim().to_string()))
}

/// `[[module NAME …]] … [[/module]]`. Dispatches `css` (raw stylesheet) and
/// `ListPages` (template); other modules fall through to raw text.
fn module_block<'a, P: Parser<'a, In<'a>, Node, E<'a>> + Clone + 'a>(
    element: P,
) -> impl Parser<'a, In<'a>, Node, E<'a>> + Clone + 'a {
    let css = kw_ci("css".into())
        .ignore_then(read_until(&["]]"]).ignored())
        .then_ignore(just("]]"))
        .ignore_then(read_until_lines(&["[[/module"]).map(|s| s.to_string()))
        .then_ignore(choice((just("[[/module]]").ignored(), end())))
        .map(Node::Stylesheet);

    let listpages = kw_ci("listpages".into())
        .ignore_then(read_until(&["]]"]).ignored())
        .then_ignore(just("]]"))
        .ignore_then(listpages_body(element));

    // Any other single-tag module (`[[module Rate]]`, `[[module PageTree …]]`,
    // …) with no `[[/module]]` closer: consume its name + params up to `]]`
    // and emit a suppressed [`Node::Module`]. These are dynamic and have no
    // static representation.
    let inline = module_name()
        .then_ignore(read_until(&["]]"]).ignored())
        .then_ignore(just("]]").or_not())
        .map(Node::Module);

    kw_ci("module".into())
        .ignore_then(spaces1())
        .ignore_then(css.or(listpages).or(inline))
}

/// A single word (module name): letters/digits, case-insensitive-friendly.
fn module_name<'a>() -> impl Parser<'a, In<'a>, String, E<'a>> + Clone + 'a {
    any::<In<'a>, E<'a>>()
        .filter(|c: &char| c.is_ascii_alphabetic())
        .repeated()
        .at_least(1)
        .collect::<String>()
}

/// Body of a `[[module ListPages …]]`: everything up to `[[/module]]`.
///
/// TODO: split into `prependLine` / per-page template / `appendLine` using the
/// module parameters, and interpret the parameter string into
/// [`ListPagesParams`] (category, tags, dates, ordering).
fn listpages_body<'a, P: Parser<'a, In<'a>, Node, E<'a>> + Clone + 'a>(
    element: P,
) -> impl Parser<'a, In<'a>, Node, E<'a>> + Clone + 'a {
    let term = closing_tag(ClosedTag::Module)
        .to(ContentExitReason::EndOfTag(ClosedTag::Module))
        .or(end().to(ContentExitReason::Eof));
    content_until(element, term).map(|(repeat, _)| {
        Node::ListPages(ListPages {
            params: ListPagesParams {
                category: None,
                tags: None,
                created_by: None,
                created_at: None,
                updated_at: None,
                order: None,
                offset: None,
                limit: None,
            },
            prepend: Vec::new(),
            repeat,
            append: Vec::new(),
        })
    })
}

/// `[[tabview]] … [[tab Name]] … [[/tab]] … [[/tabview]]`.
fn tabview_block<'a, P: Parser<'a, In<'a>, Node, E<'a>> + Clone + 'a>(
    element: P,
) -> impl Parser<'a, In<'a>, Node, E<'a>> + Clone + 'a {
    let ws = choice((just(' ').ignored(), just('\n').ignored()))
        .repeated()
        .ignored();
    let tab_close = just("[[")
        .ignore_then(spaces())
        .ignore_then(just('/'))
        .ignore_then(spaces())
        .ignore_then(kw_ci("tab".into()))
        .ignore_then(spaces())
        .ignore_then(just("]]"))
        .to(ContentExitReason::EndOfTag(ClosedTag::Tab));

    let tab = just("[[")
        .ignore_then(spaces())
        .ignore_then(kw_ci("tab".into()))
        .ignore_then(spaces())
        .ignore_then(content_before(element.clone(), just("]]").ignored()))
        .then_ignore(just("]]"))
        .then(content_until(element, tab_close))
        .map(|(name, (content, _))| types::Tab { name, content });

    kw_ci("tabview".into())
        .ignore_then(params_block())
        .ignore_then(spaces())
        .ignore_then(just("]]"))
        .ignore_then(ws.clone())
        .ignore_then(tab.separated_by(ws.clone()).collect::<Vec<_>>())
        .then_ignore(ws)
        .then_ignore(just("[["))
        .then_ignore(spaces())
        .then_ignore(just('/'))
        .then_ignore(spaces())
        .then_ignore(kw_ci("tabview".into()))
        .then_ignore(spaces())
        .then_ignore(just("]]"))
        .map(|tabs: Vec<types::Tab>| Node::Tabview(tabs))
}

/// `[[include source key="value" …]]`.
fn include_block<'a>() -> impl Parser<'a, In<'a>, Node, E<'a>> + Clone + 'a {
    kw_ci("include".into())
        .ignore_then(spaces1())
        .ignore_then(read_include_body())
        .then_ignore(just("]]"))
        .map(|raw: &str| {
            let (source, vars) = parse_include_args(raw);
            Node::Include(Include { source, vars })
        })
}

/// Read the body of a `[[include ...]]`, tracking `[[`/`]]` nesting so a `]]`
/// that belongs to a nested construct (an `[[image ...]]` inside a value, a
/// `[[[link]]]`, ...) does not prematurely close the directive. Returns the body
/// up to (not consuming) the balanced closing `]]`; the caller consumes it.
/// Non-overlapping scan keeps `[[[...]]]` (one `[[` + one literal `[`, then one
/// `]]` + one literal `]`) depth-balanced. Only ASCII delimiters are touched,
/// so every slice lands on a UTF-8 boundary. If no balanced close is found the
/// whole remainder is returned leniently (the trailing `]]` consume then fails
/// and the directive falls through to literal text).
fn read_include_body<'a>() -> impl Parser<'a, In<'a>, &'a str, E<'a>> + Clone + 'a {
    custom(move |inp: &mut InputRef<'a, '_, In<'a>, E<'a>>| {
        let full = inp.full_slice();
        let start = *inp.cursor().inner();
        let rest = &full[start..];
        let b = rest.as_bytes();
        let mut i = 0usize;
        let mut depth: i32 = 1;
        while i + 1 < b.len() {
            if b[i] == b'[' && b[i + 1] == b'[' {
                depth += 1;
                i += 2;
            } else if b[i] == b']' && b[i + 1] == b']' {
                depth -= 1;
                if depth == 0 {
                    let body = &rest[..i];
                    for _ in body.chars() {
                        let _ = inp.next();
                    }
                    return Ok(body);
                }
                i += 2;
            } else {
                i += 1;
            }
        }
        for _ in rest.chars() {
            let _ = inp.next();
        }
        Ok(rest)
    })
}

/// Split the body of a `[[include ...]]` into the source page reference and
/// its variable substitution map. Values are parsed as real wikitext markup
/// ([`Content`]), so `{$x}` becomes an [`TextObj::IncludeVar`] node (enabling
/// nested passthrough) and `[[image ...]]` becomes an [`Node::Image`].
///
/// Two assignment syntaxes are recognised, distinguished by a depth-0 `|`:
/// • pipe-separated — `source | k1=v1 | k2=v2` (a value runs to the next
///   depth-0 `|`, so it may contain spaces and balanced `[[...]]` markup).
/// • space-separated — `source k1="v1" k2=v2` (quoted values, or bare values
///   running to the next depth-0 whitespace).
///
/// A later assignment to the same key overwrites the earlier one. Only ASCII
/// bytes act as delimiters and bracket pairs are scanned non-overlapping, so
/// every slice lands on a UTF-8 character boundary and `[[[...]]]` stays
/// depth-balanced (one `[[`/`]]` pair plus a literal `[`/`]`).
fn parse_include_args(raw: &str) -> (PageRef, HashMap<String, Content>) {
    let b = raw.as_bytes();
    let n = b.len();
    let mut i = 0;
    while i < n && b[i].is_ascii_whitespace() {
        i += 1;
    }
    let src_start = i;
    while i < n && !b[i].is_ascii_whitespace() && b[i] != b'|' {
        i += 1;
    }
    let source = parse_page_ref(&raw[src_start..i]);
    let remainder = &raw[i..];
    let vars = if has_depth0_pipe(remainder) {
        parse_pipe_vars(remainder)
    } else {
        parse_space_vars(remainder)
    };
    (source, vars)
}

/// Strip one layer of surrounding double quotes, if present.
fn unquote(value: &str) -> &str {
    let t = value.trim();
    if t.len() >= 2 && t.starts_with('"') && t.ends_with('"') {
        &t[1..t.len() - 1]
    } else {
        t
    }
}

/// Record a `key=value` segment: split on the first `=`, parse the value as
/// wikitext markup. Quoted values are unwrapped first.
fn insert_kv(seg: &str, vars: &mut HashMap<String, Content>) {
    let Some(eq) = seg.find('=') else {
        return;
    };
    let key = seg[..eq].trim();
    if key.is_empty() {
        return;
    }
    vars.insert(key.to_string(), parse(unquote(&seg[eq + 1..])));
}

/// Track `[[`/`]]` depth (and skip over `"..."` quotes) across `s`; return
/// whether a `|` occurs at bracket depth 0 outside quotes — the marker of the
/// pipe-separated assignment syntax.
fn has_depth0_pipe(s: &str) -> bool {
    let b = s.as_bytes();
    let mut i = 0;
    let mut depth = 0i32;
    let mut quote = false;
    while i < b.len() {
        if quote {
            if b[i] == b'"' {
                quote = false;
            }
            i += 1;
            continue;
        }
        if i + 1 < b.len() && b[i] == b'[' && b[i + 1] == b'[' {
            depth += 1;
            i += 2;
            continue;
        }
        if i + 1 < b.len() && b[i] == b']' && b[i + 1] == b']' {
            if depth > 0 {
                depth -= 1;
            }
            i += 2;
            continue;
        }
        if b[i] == b'"' {
            quote = true;
            i += 1;
            continue;
        }
        if depth == 0 && b[i] == b'|' {
            return true;
        }
        i += 1;
    }
    false
}

fn parse_pipe_vars(remainder: &str) -> HashMap<String, Content> {
    let b = remainder.as_bytes();
    let mut vars = HashMap::new();
    let mut seg_start = 0;
    let mut i = 0;
    let mut depth = 0i32;
    let mut quote = false;
    while i < b.len() {
        if quote {
            if b[i] == b'"' {
                quote = false;
            }
            i += 1;
            continue;
        }
        if i + 1 < b.len() && b[i] == b'[' && b[i + 1] == b'[' {
            depth += 1;
            i += 2;
            continue;
        }
        if i + 1 < b.len() && b[i] == b']' && b[i + 1] == b']' {
            if depth > 0 {
                depth -= 1;
            }
            i += 2;
            continue;
        }
        if b[i] == b'"' {
            quote = true;
            i += 1;
            continue;
        }
        if depth == 0 && b[i] == b'|' {
            insert_kv(&remainder[seg_start..i], &mut vars);
            seg_start = i + 1;
        }
        i += 1;
    }
    insert_kv(&remainder[seg_start..], &mut vars);
    vars
}

fn parse_space_vars(remainder: &str) -> HashMap<String, Content> {
    let b = remainder.as_bytes();
    let n = b.len();
    let mut i = 0;
    let mut vars: HashMap<String, Content> = HashMap::new();
    while i < n {
        while i < n && b[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= n {
            break;
        }
        let key_start = i;
        while i < n && b[i] != b'=' && !b[i].is_ascii_whitespace() {
            i += 1;
        }
        let key_end = i;
        if key_start == key_end || i >= n || b[i] != b'=' {
            while i < n && !b[i].is_ascii_whitespace() {
                i += 1;
            }
            continue;
        }
        i += 1;
        let value = if i < n && b[i] == b'"' {
            i += 1;
            let v_start = i;
            while i < n && b[i] != b'"' {
                i += 1;
            }
            let v = remainder[v_start..i].to_string();
            if i < n {
                i += 1;
            }
            v
        } else {
            let v_start = i;
            let mut depth = 0i32;
            while i < n {
                if i + 1 < n && b[i] == b'[' && b[i + 1] == b'[' {
                    depth += 1;
                    i += 2;
                    continue;
                }
                if i + 1 < n && b[i] == b']' && b[i + 1] == b']' {
                    if depth > 0 {
                        depth -= 1;
                    }
                    i += 2;
                    continue;
                }
                if depth == 0 && b[i].is_ascii_whitespace() {
                    break;
                }
                i += 1;
            }
            remainder[v_start..i].to_string()
        };
        let key = remainder[key_start..key_end].trim();
        if !key.is_empty() {
            vars.insert(key.to_string(), parse(value.trim()));
        }
    }
    vars
}

/// `[[image SOURCE attr="val" …]]` with optional `f<`/`f>`/`<`/`>`/`=` prefix.
fn image_block<'a>() -> impl Parser<'a, In<'a>, Node, E<'a>> + Clone + 'a {
    let align = choice((
        just("f<").to(Some(Align {
            floating: true,
            side: AlignSide::Left,
        })),
        just("f>").to(Some(Align {
            floating: true,
            side: AlignSide::Right,
        })),
        just('<').to(Some(Align {
            floating: false,
            side: AlignSide::Left,
        })),
        just('>').to(Some(Align {
            floating: false,
            side: AlignSide::Right,
        })),
        just('=').to(Some(Align {
            floating: false,
            side: AlignSide::Center,
        })),
        empty().to(None),
    ));
    align
        .then_ignore(kw_ci("image".into()))
        .then_ignore(spaces1())
        .then(text_objs(&[" ", "]]"]))
        .then(params_block())
        .then_ignore(spaces())
        .then_ignore(just("]]"))
        .map(|((align, source), params)| Node::Image {
            align,
            source,
            params,
        })
}

// =========================================================================
// Inline markup
// =========================================================================

/// All inline (non-line-start) markup.
fn inline_syntax<'a, P: Parser<'a, In<'a>, Node, E<'a>> + Clone + 'a>(
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
fn style<'a, P: Parser<'a, In<'a>, Node, E<'a>> + Clone + 'a>(
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
fn superscript<'a, P: Parser<'a, In<'a>, Node, E<'a>> + Clone + 'a>(
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
fn subscript<'a, P: Parser<'a, In<'a>, Node, E<'a>> + Clone + 'a>(
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
fn color_span<'a, P: Parser<'a, In<'a>, Node, E<'a>> + Clone + 'a>(
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
fn module_var<'a>() -> impl Parser<'a, In<'a>, Node, E<'a>> + Clone + 'a {
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
fn include_var<'a>() -> impl Parser<'a, In<'a>, Node, E<'a>> + Clone + 'a {
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

// =========================================================================
// Attributes / TextObj runs
// =========================================================================

/// Parse `key="value"` / `key=value` attributes until `]` or newline. Values
/// may contain `%%vars%%` and `{$vars$}`.
fn params_block<'a>() -> impl Parser<'a, In<'a>, HashMap<String, Vec<TextObj>>, E<'a>> + Clone + 'a
{
    custom(|inp: &mut InputRef<'a, '_, In<'a>, E<'a>>| {
        let mut map: HashMap<String, Vec<TextObj>> = HashMap::new();
        loop {
            while matches!(inp.peek(), Some(' ')) {
                inp.next();
            }
            let full = inp.full_slice();
            let off = *inp.cursor().inner();
            let rest = &full[off..];
            if rest.is_empty() || rest.starts_with(']') || rest.starts_with('\n') {
                break;
            }
            let key_start = *inp.cursor().inner();
            while matches!(inp.peek(), Some(c) if is_prop_char(c)) {
                inp.next();
            }
            let key_end = *inp.cursor().inner();
            if key_end == key_start {
                break;
            }
            let key = full[key_start..key_end].to_ascii_lowercase();
            if !matches!(inp.next(), Some('=')) {
                break;
            }
            let value = if matches!(inp.peek(), Some('"')) {
                inp.next();
                let v = collect_text_objs(inp, &[], &['"']);
                if matches!(inp.peek(), Some('"')) {
                    inp.next();
                }
                v
            } else {
                collect_text_objs(inp, &[], &[' ', ']'])
            };
            map.insert(key, value);
        }
        Ok(map)
    })
}

/// A run of [`TextObj`]s — plain text chunks interleaved with `%%var%%` and
/// `{$var$}` substitutions — up to any of `delims`, a newline, or EOF.
fn text_objs<'a>(
    delims: &'static [&'static str],
) -> impl Parser<'a, In<'a>, Vec<TextObj>, E<'a>> + Clone + 'a {
    custom(move |inp: &mut InputRef<'a, '_, In<'a>, E<'a>>| Ok(collect_text_objs(inp, delims, &[])))
}

/// Imperative core shared by [`params_block`] and [`text_objs`].
///
/// Accumulates plain text into a buffer, flushing it as [`TextObj::Plain`]
/// whenever a `%%var%%` or `{$var$}` substitution is encountered, and stops at
/// any of: a multi-char `delim`, a `single_stop` char, a newline, or EOF.
fn collect_text_objs<'a, 'b>(
    inp: &mut InputRef<'a, 'b, In<'a>, E<'a>>,
    delims: &[&str],
    single_stops: &[char],
) -> Vec<TextObj> {
    let mut result: Vec<TextObj> = Vec::new();
    let mut buf = String::new();
    let flush = |buf: &mut String, result: &mut Vec<TextObj>| {
        if !buf.is_empty() {
            result.push(TextObj::Plain(std::mem::take(buf)));
        }
    };
    loop {
        let full = inp.full_slice();
        let off = *inp.cursor().inner();
        let rest = &full[off..];
        if rest.is_empty() || rest.starts_with('\n') || delims.iter().any(|d| rest.starts_with(d)) {
            break;
        }
        if let Some(c) = rest.chars().next() {
            if single_stops.contains(&c) {
                break;
            }
        }
        // %%name|default%%
        if rest.starts_with("%%") {
            flush(&mut buf, &mut result);
            inp.next();
            inp.next();
            let (name, default) = read_named_var(inp, "%%");
            result.push(TextObj::ModuleVar { name, default });
            continue;
        }
        // {$name//default}
        if rest.starts_with("{$") {
            flush(&mut buf, &mut result);
            inp.next();
            inp.next();
            let (name, default) = read_include_var(inp);
            result.push(TextObj::IncludeVar { name, default });
            continue;
        }
        if let Some(c) = inp.next() {
            buf.push(c);
        }
    }
    flush(&mut buf, &mut result);
    result
}

/// Read `name` (prop chars) then, if `closer` follows optionally after
/// `|default`, consume through `closer`. Returns `(name, default)`.
fn read_named_var<'a, 'b>(
    inp: &mut InputRef<'a, 'b, In<'a>, E<'a>>,
    closer: &str,
) -> (String, Option<String>) {
    let full = inp.full_slice();
    let name_start = *inp.cursor().inner();
    while matches!(inp.peek(), Some(c) if is_prop_char(c)) {
        inp.next();
    }
    let name_end = *inp.cursor().inner();
    let name = full[name_start..name_end].to_string();
    let default = if matches!(inp.peek(), Some('|')) {
        inp.next();
        let d_start = *inp.cursor().inner();
        loop {
            let f = inp.full_slice();
            let o = *inp.cursor().inner();
            if f[o..].starts_with(closer) {
                break;
            }
            if inp.next().is_none() {
                break;
            }
        }
        let d_end = *inp.cursor().inner();
        let d = full[d_start..d_end].to_string();
        consume_prefix(inp, closer);
        Some(d)
    } else {
        consume_prefix(inp, closer);
        None
    };
    (name, default)
}

/// Read `{$name//default}`'s tail (after `{$`): name, optional `//default`,
/// then `}`.
fn read_include_var<'a, 'b>(
    inp: &mut InputRef<'a, 'b, In<'a>, E<'a>>,
) -> (String, Option<Content>) {
    let full = inp.full_slice();
    let name_start = *inp.cursor().inner();
    while matches!(inp.peek(), Some(c) if is_prop_char(c)) {
        inp.next();
    }
    let name_end = *inp.cursor().inner();
    let name = full[name_start..name_end].to_string();
    let default = if {
        let f = inp.full_slice();
        let o = *inp.cursor().inner();
        f[o..].starts_with("//")
    } {
        inp.next();
        inp.next();
        let d_start = *inp.cursor().inner();
        loop {
            let f = inp.full_slice();
            let o = *inp.cursor().inner();
            if f[o..].starts_with('}') {
                break;
            }
            if inp.next().is_none() {
                break;
            }
        }
        let d_end = *inp.cursor().inner();
        let d = full[d_start..d_end].to_string();
        if matches!(inp.peek(), Some('}')) {
            inp.next();
        }
        Some(parse(&d))
    } else {
        if matches!(inp.peek(), Some('}')) {
            inp.next();
        }
        None
    };
    (name, default)
}

/// Consume `prefix` from the input if it's next.
fn consume_prefix<'a, 'b>(inp: &mut InputRef<'a, 'b, In<'a>, E<'a>>, prefix: &str) {
    let f = inp.full_slice();
    let o = *inp.cursor().inner();
    if f[o..].starts_with(prefix) {
        for _ in 0..prefix.chars().count() {
            inp.next();
        }
    }
}

// =========================================================================
// Link / page-ref / tag-filter helpers
// =========================================================================

/// Turn a raw link target string into a [`LinkTarget`]: external URL if it
/// starts with `http://`/`https://`, otherwise an internal wiki page reference.
fn parse_link_target(raw: &str) -> LinkTarget {
    let trimmed = raw.trim();
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        return LinkTarget::Url(trimmed.to_string());
    }
    LinkTarget::Page(parse_page_ref(trimmed))
}

/// Parse a `[[include]]` source or internal link path into a [`PageRef`].
///
/// A leading `space:` segment is a cross-space reference; the rest is the path.
fn parse_page_ref(raw: &str) -> PageRef {
    let raw = raw.trim().trim_start_matches('/');
    let lower = raw.to_ascii_lowercase();
    let parts: Vec<&str> = lower.split(':').collect();
    match parts.as_slice() {
        [] | [""] => PageRef {
            space: None,
            path: Vec::new(),
        },
        [single] => PageRef {
            space: None,
            path: vec![(*single).to_string()],
        },
        [space, rest @ ..] => PageRef {
            space: Some((*space).to_string()),
            path: rest.iter().map(|s| (*s).to_string()).collect(),
        },
    }
}

/// Parse a `[[iftags …]]` argument string into `(has_all, has_none)` per
/// PureScript `objFiltr` (plain tags and `+tag` both required, `-tag` excluded;
/// the OR-distinction between plain tags is intentionally collapsed).
fn parse_tag_filter(raw: &str) -> (Vec<String>, Vec<String>) {
    let mut has_all = Vec::new();
    let mut has_none = Vec::new();
    for token in raw.split([',', ' ']) {
        let tok = token.trim();
        if tok.is_empty() {
            continue;
        }
        match tok.chars().next() {
            Some('+') => has_all.push(tok[1..].to_string()),
            Some('-') => has_none.push(tok[1..].to_string()),
            _ => has_all.push(tok.to_string()),
        }
    }
    (has_all, has_none)
}

/// Normalize a `##color|` argument: prefix with `#` if it's a bare hex triplet
/// of a valid length (3/4/6/8 digits).
fn normalize_color(c: String) -> String {
    if [3, 4, 6, 8].contains(&c.len()) && c.chars().all(is_hex_char) {
        format!("#{c}")
    } else {
        c
    }
}

// =========================================================================
// Post-processing: fuse adjacent text fragments
// =========================================================================

/// Recursively merge adjacent [`Node::Text(Plain(_))`] nodes so the fallback
/// single-char path doesn't fragment output (e.g. `[[toc]]` → one text node).
fn merge_text(content: Content) -> Content {
    let mut out: Content = Vec::with_capacity(content.len());
    for node in content {
        match node {
            Node::Text(TextObj::Plain(s)) => {
                if let Some(Node::Text(TextObj::Plain(prev))) = out.last_mut() {
                    prev.push_str(&s);
                } else {
                    out.push(Node::Text(TextObj::Plain(s)));
                }
            }
            other => out.push(map_node_content(other, merge_text)),
        }
    }
    out
}

/// Apply a transformation to every nested [`Content`] within a node.
fn map_node_content<F: Fn(Content) -> Content>(node: Node, f: F) -> Node {
    match node {
        Node::Container { kind, content } => Node::Container {
            kind,
            content: f(content),
        },
        Node::Heading { level, content } => Node::Heading {
            level,
            content: f(content),
        },
        Node::Image {
            align,
            source,
            params,
        } => Node::Image {
            align,
            source,
            params,
        },
        Node::Table(rows) => Node::Table(
            rows.into_iter()
                .map(|row| {
                    row.into_iter()
                        .map(|cell| TableCell {
                            colspan: cell.colspan,
                            header: cell.header,
                            align: cell.align,
                            content: f(cell.content),
                        })
                        .collect()
                })
                .collect(),
        ),
        Node::BlockTable(t) => Node::BlockTable(BlockTable {
            params: t.params,
            rows: t
                .rows
                .into_iter()
                .map(|r| BlockRow {
                    params: r.params,
                    content: f(r.content),
                })
                .collect(),
        }),
        Node::BlockCell(c) => Node::BlockCell(BlockCell {
            header: c.header,
            params: c.params,
            content: f(c.content),
        }),
        Node::SupSubscript { sup, sub } => Node::SupSubscript {
            sup: f(sup),
            sub: f(sub),
        },
        Node::Link { target, text } => Node::Link {
            target,
            text: f(text),
        },
        Node::Footnote(c) => Node::Footnote(f(c)),
        Node::Tabview(tabs) => Node::Tabview(
            tabs.into_iter()
                .map(|t| types::Tab {
                    name: f(t.name),
                    content: f(t.content),
                })
                .collect(),
        ),
        Node::ListPages(mut lp) => {
            lp.prepend = f(lp.prepend);
            lp.repeat = f(lp.repeat);
            lp.append = f(lp.append);
            Node::ListPages(lp)
        }
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn txt(s: &str) -> Node {
        Node::Text(TextObj::Plain(s.to_string()))
    }

    /// Concatenate the plain-text content of an optional attribute value.
    fn plain_of(objs: Option<&Vec<TextObj>>) -> String {
        objs.unwrap_or(&vec![])
            .iter()
            .map(|o| match o {
                TextObj::Plain(s) => s.as_str(),
                _ => "",
            })
            .collect()
    }

    /// Extract the single plain-text value of an include variable.
    fn var_str(vars: &HashMap<String, Content>, key: &str) -> Option<String> {
        vars.get(key).and_then(|v| match v.as_slice() {
            [Node::Text(TextObj::Plain(s))] => Some(s.clone()),
            _ => None,
        })
    }

    #[test]
    fn plain_text() {
        let c = parse("hello world");
        assert_eq!(c, vec![txt("hello world")]);
    }

    #[test]
    fn unknown_tag_reassembles() {
        // `[[toc]]` is not a known construct: it must collapse back to a single
        // text node rather than fragmenting into characters.
        let c = parse("[[toc]]");
        assert_eq!(c, vec![txt("[[toc]]")]);
    }

    #[test]
    fn bold_italic() {
        let c = parse("//italic// and **bold**");
        assert!(matches!(
            c[0],
            Node::Container {
                kind: ContainerKind::Style(TextStyle::Italic),
                ..
            }
        ));
        assert!(matches!(c[1], Node::Text(_)));
        assert!(matches!(
            c[2],
            Node::Container {
                kind: ContainerKind::Style(TextStyle::Bold),
                ..
            }
        ));
    }

    #[test]
    fn heading_and_hr() {
        let c = parse("++ Title\n----\nbody");
        assert!(matches!(c[0], Node::Heading { level: 2, .. }));
        assert!(matches!(c[1], Node::HorizontalRule));
        assert!(matches!(c[2], Node::Text(_)));
    }

    #[test]
    fn triple_link() {
        let c = parse("[[[science|Science page]]]");
        match &c[0] {
            Node::Link { target, text } => {
                assert!(matches!(target, LinkTarget::Page(_)));
                assert_eq!(text.len(), 1);
            }
            other => panic!("expected link, got {other:?}"),
        }
    }

    #[test]
    fn bare_url() {
        let c = parse("see https://example.com/x for info");
        assert!(matches!(
            c[1],
            Node::Link {
                target: LinkTarget::Url(_),
                ..
            }
        ));
    }

    #[test]
    fn div_block() {
        let c = parse("[[div style=\"color:red\"]]\nhi **there**\n[[/div]]");
        assert!(matches!(
            c[0],
            Node::Container {
                kind: ContainerKind::Div { .. },
                ..
            }
        ));
    }

    #[test]
    fn table_basic() {
        let c = parse("||~ H ||~ H2 ||\n|| a || b ||\n");
        match &c[0] {
            Node::Table(rows) => {
                assert_eq!(rows.len(), 2);
                assert_eq!(rows[0].len(), 2);
                assert!(rows[0][0].header);
                assert!(!rows[1][0].header);
            }
            other => panic!("expected table, got {other:?}"),
        }
    }

    #[test]
    fn table_align_cell() {
        // `||=` is a centered cell, not a header.
        let c = parse("||= centered ||< left ||\n");
        match &c[0] {
            Node::Table(rows) => {
                assert_eq!(rows[0][0].align.map(|a| a.side), Some(AlignSide::Center));
                assert_eq!(rows[0][1].align.map(|a| a.side), Some(AlignSide::Left));
            }
            other => panic!("expected table, got {other:?}"),
        }
    }

    #[test]
    fn grid_table_block() {
        let c = parse(concat!(
            "[[table class=\"t\"]]\n",
            "[[row class=\"r\"]]\n",
            "[[cell class=\"c\" style=\"display: none\"]]body[[/cell]]\n",
            "[[hcell]]head[[/cell]]\n",
            "[[/row]]\n",
            "[[/table]]",
        ));
        let Node::BlockTable(t) = &c[0] else {
            panic!("expected block table, got {:?}", c[0]);
        };
        assert_eq!(plain_of(t.params.get("class")), "t");
        assert_eq!(t.rows.len(), 1);
        let row = &t.rows[0];
        assert_eq!(plain_of(row.params.get("class")), "r");
        let cells: Vec<&BlockCell> = row
            .content
            .iter()
            .filter_map(|n| match n {
                Node::BlockCell(cell) => Some(cell),
                _ => None,
            })
            .collect();
        assert_eq!(cells.len(), 2);
        assert!(!cells[0].header);
        assert!(cells[1].header);
        assert_eq!(plain_of(cells[0].params.get("style")), "display: none");
    }

    #[test]
    fn grid_table_iftags_wrapped_cell() {
        // A cell wrapped in [[iftags]] (the rate-module pattern) must parse
        // without leaking, sitting inside the row alongside bare cells.
        let c = parse(concat!(
            "[[table]]\n",
            "[[row]]\n",
            "[[iftags top10]]\n",
            "[[cell class=\"badge\"]]A[[/cell]]\n",
            "[[/iftags]]\n",
            "[[cell]]B[[/cell]]\n",
            "[[/row]]\n",
            "[[/table]]",
        ));
        let Node::BlockTable(t) = &c[0] else {
            panic!("expected block table, got {:?}", c[0]);
        };
        assert_eq!(t.rows.len(), 1);
        let row = &t.rows[0];
        // The bare cell is a direct child; the iftags-wrapped cell is nested.
        let bare = row
            .content
            .iter()
            .filter(|n| matches!(n, Node::BlockCell(_)))
            .count();
        assert_eq!(bare, 1);
        let wrapped = row.content.iter().any(|n| match n {
            Node::Container {
                kind: ContainerKind::IfTags { .. },
                content,
            } => content.iter().any(|m| matches!(m, Node::BlockCell(_))),
            _ => false,
        });
        assert!(wrapped, "iftags should wrap a cell");
    }

    #[test]
    fn blockquote_and_color() {
        let c = parse("> quoted text\n> more\n##red|red text##");
        assert!(matches!(
            c[0],
            Node::Container {
                kind: ContainerKind::Quote,
                ..
            }
        ));
        assert!(matches!(
            c[1],
            Node::Container {
                kind: ContainerKind::Color(_),
                ..
            }
        ));
    }

    #[test]
    fn color_hex_normalized() {
        // Bare 6-digit hex should be prefixed with `#`.
        let c = parse("##FFA500|orange##");
        match &c[0] {
            Node::Container {
                kind: ContainerKind::Color(col),
                ..
            } => assert_eq!(col, "#FFA500"),
            other => panic!("expected color, got {other:?}"),
        }
    }

    #[test]
    fn include_directive() {
        let c = parse("[[include component:foo]]");
        match &c[0] {
            Node::Include(Include { source, .. }) => {
                assert_eq!(source.space.as_deref(), Some("component"));
                assert_eq!(source.path, vec!["foo".to_string()]);
            }
            other => panic!("expected include, got {other:?}"),
        }
    }

    #[test]
    fn include_pipe_vars() {
        // Pipe syntax: a later assignment to the same key wins, so the
        // `k={$k}|k=default` idiom resolves to the default.
        let c = parse(concat!(
            "[[include component:rate-base ",
            "align={$align}|align=right|",
            "votes={$votes}|votes=right|",
            "stars={$stars}]]",
        ));
        let Node::Include(Include { source, vars }) = &c[0] else {
            panic!("expected include, got {:?}", c[0]);
        };
        assert_eq!(source.path, vec!["rate-base".to_string()]);
        assert_eq!(var_str(vars, "align"), Some("right".to_string()));
        assert_eq!(var_str(vars, "votes"), Some("right".to_string()));
        // No default for stars — the passthrough value is an IncludeVar node
        // (a real reference to the outer `stars` var), not a literal string.
        let stars = vars.get("stars").expect("stars var");
        assert!(matches!(
            stars.as_slice(),
            [Node::Text(TextObj::IncludeVar { name, .. })] if name == "stars"
        ));
    }

    #[test]
    fn include_value_with_nested_brackets() {
        // The translationblock value carries three `[[image ...]]` blocks; the
        // balanced reader must NOT close the include at the first image's `]]`.
        // The directive ends at the `]]` that returns bracket depth to 0.
        let c = parse(concat!(
            "[[include component:rate ten=show|",
            "translationblock=[[image http://a/c.png link=\"http://x/1\"]] ",
            "[[image http://a/p.png link=\"http://x/2\"]] ",
            "[[image http://a/ar.png link=\"http://x/3\"]]]]",
        ));
        let Node::Include(Include { source, vars }) = &c[0] else {
            panic!("expected include, got {:?}", c[0]);
        };
        assert_eq!(source.path, vec!["rate".to_string()]);
        assert_eq!(var_str(vars, "ten"), Some("show".to_string()));
        // The translationblock value is parsed markup: three image nodes (plus
        // whitespace text between them), not a truncated string.
        let tb = vars.get("translationblock").expect("translationblock var");
        let images = tb
            .iter()
            .filter(|n| matches!(n, Node::Image { .. }))
            .count();
        assert_eq!(images, 3, "translationblock = {:#?}", tb);
        // Nothing leaks after the directive.
        assert_eq!(c.len(), 1, "trailing nodes leaked: {:#?}", &c[1..]);
    }

    #[test]
    fn integration_realistic_chunk() {
        // A mixed chunk resembling the project's syntax guide: a self-closing
        // unknown module, a div with an image + centered lines, a heading, a
        // table, and inline styling.
        let src = "[[module Rate]]\n\n[[div style=\"color:red\"]]\n[[f<image https://x/y.png width=\"128\"]]= Hello there\n[[/div]]\n\n+ Heading\n\n||~ A ||~ B ||\n|| 1 || 2 ||\n\n//**bold italic**// and ##00FF00|green##.\n";
        let c = parse(src);
        // Should parse to several distinct nodes, not collapse to a single text
        // blob (which would indicate the parser gave up).
        assert!(c.len() > 4, "len = {}, nodes = {:#?}", c.len(), c);
        // The unknown `[[module Rate]]` becomes a suppressed Module node.
        assert!(matches!(c[0], Node::Module(_)));
        // A div container appears somewhere.
        assert!(
            c.iter()
                .any(|n| matches!(n, Node::Container { kind, .. } if matches!(
                    kind,
                    ContainerKind::Div { .. }
                ))),
            "no div container found: {:#?}",
            c
        );
        // A table appears.
        assert!(c.iter().any(|n| matches!(n, Node::Table(_))));
        // A heading appears.
        assert!(c.iter().any(|n| matches!(n, Node::Heading { .. })));
    }

    #[test]
    fn self_closing_module_is_suppressed() {
        // `[[module Rate]]` is not a known module; it is consumed (not leaked
        // as text) and represented as a suppressed Module node.
        let c = parse("[[module Rate]]");
        assert_eq!(c, vec![Node::Module("Rate".to_string())]);
    }

    #[test]
    fn code_block_is_raw() {
        // `[[code]]` body is verbatim (not parsed as wikitext): the `>` stays
        // literal rather than becoming a blockquote, and the body is trimmed.
        let c = parse("[[code]]\n> line one\n**not bold**\n[[/code]]");
        assert_eq!(c, vec![Node::Code("> line one\n**not bold**".to_string())]);
    }

    #[test]
    fn collapsible_is_div_container() {
        let c = parse("[[collapsible show=\"+\" hide=\"-\"]]\nbody **bold**\n[[/collapsible]]");
        let Node::Container { kind, content } = &c[0] else {
            panic!("expected container, got {c:#?}");
        };
        let ContainerKind::Div {
            inline,
            block,
            params,
        } = kind
        else {
            panic!("expected div, got {kind:#?}");
        };
        assert!(!inline);
        assert!(block);
        assert_eq!(
            params
                .get("class")
                .and_then(|v| v.first())
                .and_then(|t| match t {
                    TextObj::Plain(s) => Some(s.as_str()),
                    _ => None,
                }),
            Some("collapsible-block")
        );
        // The body parsed as wikitext (bold span present).
        assert!(content.iter().any(|n| matches!(
            n,
            Node::Container {
                kind: ContainerKind::Style(TextStyle::Bold),
                ..
            }
        )));
    }

    #[test]
    fn nested_bullet_list_nbsp_indent() {
        // Sub-items indented with a non-breaking space (U+00A0), as in
        // rpcauthority's nav:top. `##color##` must not be mistaken for a list.
        let c = parse("* parent\n\u{00A0}* child1\n\u{00A0}* child2\n* sibling\n##ff0000|red##");
        let txt = |content: &Content| -> String {
            content
                .iter()
                .filter_map(|n| match n {
                    Node::Text(TextObj::Plain(s)) => Some(s.as_str()),
                    _ => None,
                })
                .collect()
        };
        let list = c
            .iter()
            .filter_map(|n| match n {
                Node::List(l) => Some(l),
                _ => None,
            })
            .next()
            .expect("a list");
        assert!(!list.ordered);
        assert_eq!(list.items.len(), 2); // parent, sibling
        let parent = &list.items[0];
        assert_eq!(txt(&parent.content), "parent");
        let sub = parent.sublist.as_ref().expect("sublist");
        assert_eq!(sub.items.len(), 2);
        assert_eq!(txt(&sub.items[0].content), "child1");
        assert_eq!(txt(&list.items[1].content), "sibling");
    }

    #[test]
    fn multibyte_before_keyword_no_panic() {
        // Regression for the `kw_ci` panic: a 3-byte char (`…`) immediately
        // before a keyword whose byte length lands inside that char used to
        // panic with "end byte index N is not a char boundary". The whole
        // document must parse without panicking.
        let src = "…module Rate]]";
        let _ = parse(src); // must not panic
        // And the keyword match itself: `…include foo` should still recognize
        // the include directive through the multibyte prefix.
        let c = parse("…[[include foo]]");
        assert!(c.iter().any(|n| matches!(n, Node::Text(_))));
    }

    #[test]
    fn module_var() {
        let c = parse("hello %%name|friend%%!");
        // should produce at least one ModuleVar text obj
        let mut found = false;
        for n in &c {
            if let Node::Text(TextObj::ModuleVar { name, default }) = n {
                assert_eq!(name, "name");
                assert_eq!(default.as_deref(), Some("friend"));
                found = true;
            }
        }
        assert!(found, "no ModuleVar parsed");
    }
}
