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
    Align, AlignSide, ContainerKind, Content, Include, LinkTarget, ListPages, ListPagesParams,
    Node, PageRef, TableCell, TextObj, TextStyle,
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
        centered_line(element),
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

// =========================================================================
// Bracketed `[[…]]` syntax
// =========================================================================

/// Dispatch over everything that can follow `[[`.
fn bracket_syntax<'a, P: Parser<'a, In<'a>, Node, E<'a>> + Clone + 'a>(
    element: P,
) -> impl Parser<'a, In<'a>, Node, E<'a>> + Clone + 'a {
    choice((
        // `[[[target|text]]]` / `[[[target]]]`. The third `[` is consumed here.
        just('[').ignore_then(link(element.clone())),
        div_span_block(element.clone()),
        align_block(element.clone()),
        size_block(element.clone()),
        iftags_block(element.clone()),
        module_block(element.clone()),
        tabview_block(element.clone()),
        include_block(),
        image_block(),
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
    let div = container_open("div")
        .then(content_until(
            element.clone(),
            closing_tag(ClosedTag::Div).to(ContentExitReason::EndOfTag(ClosedTag::Div)),
        ))
        .map(|(params, (content, _))| Node::Container {
            kind: ContainerKind::Div {
                inline: false,
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
                params,
            },
            content,
        });
    div.or(span)
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

/// `[[module NAME …]] … [[/module]]`. Dispatches `css` (raw stylesheet) and
/// `ListPages` (template); other modules fall through to raw text.
fn module_block<'a, P: Parser<'a, In<'a>, Node, E<'a>> + Clone + 'a>(
    element: P,
) -> impl Parser<'a, In<'a>, Node, E<'a>> + Clone + 'a {
    let css = kw_ci("css".into())
        .ignore_then(read_until(&["]]"]).ignored())
        .then_ignore(just("]]"))
        .ignore_then(read_until(&["[[/module"]).map(|s| s.to_string()))
        .then_ignore(choice((just("[[/module]]").ignored(), end())))
        .map(Node::Stylesheet);

    let listpages = kw_ci("listpages".into())
        .ignore_then(read_until(&["]]"]).ignored())
        .then_ignore(just("]]"))
        .ignore_then(listpages_body(element));

    kw_ci("module".into())
        .ignore_then(spaces1())
        .ignore_then(css.or(listpages))
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
        .ignore_then(read_until(&["]]"]))
        .then_ignore(just("]]"))
        .map(|raw: &str| {
            let mut parts = raw.split_whitespace();
            let source = parts.next().map(parse_page_ref).unwrap_or(PageRef {
                space: None,
                path: Vec::new(),
            });
            Node::Include(Include {
                source,
                vars: HashMap::new(),
            })
        })
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
                default: Some(vec![Node::Text(TextObj::Plain(d.to_string()))]),
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
        Some(vec![Node::Text(TextObj::Plain(d))])
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
    fn integration_realistic_chunk() {
        // A mixed chunk resembling the project's syntax guide: a self-closing
        // unknown module, a div with an image + centered lines, a heading, a
        // table, and inline styling.
        let src = "[[module Rate]]\n\n[[div style=\"color:red\"]]\n[[f<image https://x/y.png width=\"128\"]]= Hello there\n[[/div]]\n\n+ Heading\n\n||~ A ||~ B ||\n|| 1 || 2 ||\n\n//**bold italic**// and ##00FF00|green##.\n";
        let c = parse(src);
        // Should parse to several distinct nodes, not collapse to a single text
        // blob (which would indicate the parser gave up).
        assert!(c.len() > 4, "len = {}, nodes = {:#?}", c.len(), c);
        // The unknown `[[module Rate]]` self-closing tag reassembles to text.
        assert!(matches!(c[0], Node::Text(_)));
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
    fn self_closing_module_is_text() {
        // `[[module Rate]]` is not a known module; it must not be eaten as a
        // half-parsed block — it should fall through to a single text node.
        let c = parse("[[module Rate]]");
        assert_eq!(c, vec![txt("[[module Rate]]")]);
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
