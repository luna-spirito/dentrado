//! The lexing pass: a chumsky parser over raw bytes (`&'src [u8]`, not `&str`:
//! every delimiter and keyword is ASCII, so plain byte scans never split a
//! multibyte character, and `&str` payloads are recovered at the end via
//! [`sub`]). Total and lossless: every byte lands in exactly one token (the
//! only exceptions are the spaces a construct's own grammar eats: after a `>`
//! quote mark and inside a table cell prefix, both reconstructed from token
//! spans when they degrade), so no input can fail to lex.
//!
//! Tokens are rich: bracket constructs arrive pre-split into openers and
//! closers with their attributes parsed, inline markers (`//`, `##`, `%%`) are
//! their own tokens, and line constructs (quote marks, headings, rules, list
//! markers) are recognized at line starts. The merge pass then pairs tokens
//! into [`Node`]s without ever re-scanning source text; verbatim bodies
//! (`[[code]]`, `[[module css]]`) are sliced straight from the source via the
//! token spans.

use super::*;
use chumsky::{input::InputRef, prelude::*};

pub(crate) type Params = HashMap<String, Vec<TextObj>>;

type In<'a> = &'a [u8];
type E<'a> = extra::Err<Rich<'a, u8>>;

/// One dispatch step's worth of output, fully self-describing: every token
/// it expands to carries its byte offsets, computed right in the arm that
/// parsed it, so [`fold_tokens`] only flattens (the spaces a construct's
/// grammar eats belong to no token, exactly as the hand-written lexer had
/// it).
#[derive(Clone)]
enum TokenOut<'a> {
    One(Tok<'a>, usize, usize),
    /// One [`Tok::QuoteMark`] per `>` of the run, at `start + k`.
    Quote {
        start: usize,
        count: usize,
    },
    /// `||` plus its cell prefix.
    Pipe2 {
        /// The `||` at `[start, start + 2)`.
        start: usize,
        /// The header `~` at `[t, t + 1)`.
        tilde: Option<usize>,
        /// The alignment char's span `[s, e)` — it swallows the prefix spaces
        /// before it; the trailing ones are eaten by the step but belong to
        /// no token (merge needs them when the pipes degrade to text).
        align: Option<(usize, usize, AlignSide)>,
    },
}

/// One listpages body slot (`[[head]]` / `[[body]]` / `[[foot]]`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SectionSlot {
    Head,
    Body,
    Foot,
}

#[derive(Clone, Debug)]
pub(crate) struct Token<'src> {
    pub tok: Tok<'src>,
    pub start: usize,
    pub end: usize,
}

#[derive(Clone, Debug)]
pub(crate) enum Tok<'src> {
    /// A plain run; never begins (or contains the start of) another construct.
    Text(&'src str),
    Newline,
    /// `>` at a line start (one per `>` char, so `>>` nests two levels). The
    /// remainder of the line is lexed as if it were at a line start again:
    /// stripping the marker is exactly what the quote merge does.
    QuoteMark,
    /// `+`…`++++++` + spaces at a line start.
    Heading(u32),
    /// A full line of 4+ dashes at a line start. The newline terminates the
    /// rule but stays its own token — the merge's quote machinery needs it to
    /// end a `> ----` quote line.
    Rule,
    /// `*` / `#` list marker with its indentation (spaces / NBSPs); the
    /// marker's trailing spaces are eaten.
    ListMark {
        ordered: bool,
        indent: usize,
    },
    /// `=` at a line start (centered line), spaces eaten.
    CenterEq,
    /// `||`, opening a row at a line start or separating cells. The cell
    /// prefix (`~`? spaces alignment? spaces) is lexed as [`Tok::Tilde`] /
    /// [`Tok::CellAlign`] right after; spaces the prefix eats are not
    /// tokenized.
    Pipe2,
    /// `~` directly after `||` (header cell).
    Tilde,
    /// `<` / `=` / `>` after `||`'s optional spaces (cell alignment).
    CellAlign(AlignSide),
    /// `[[[target|text]]]`; `text` is sub-parsed by the merge.
    Link3 {
        target: &'src str,
        text: Option<&'src str>,
    },
    /// `[url text]`.
    Link1 {
        target: &'src str,
        text: Option<&'src str>,
    },
    Open(OpenTag<'src>),
    Close(ClosedTag),
    /// `[!--`
    CommentOpen,
    /// `--]` — only meaningful inside a comment; the merge degrades a stray
    /// one to the strikethrough markup the old parser saw.
    CommentClose,
    /// `//`, `**`, `__`, `--`. The old parser rejects `-- ` as a style opener
    /// (it is an em-dash); the merge decides, since a `--` with a following
    /// space still *closes* an open span.
    Mark(TextStyle),
    /// `^^`
    SupMark,
    /// `,,`
    SubMark,
    /// `##spec|`; the body runs to a `##` token or a newline.
    ColorOpen(&'src str),
    /// `##` with no `|` before the end of the line.
    ColorClose,
    /// `%%name|default%%`.
    ModuleVar {
        name: &'src str,
        default: Option<&'src str>,
    },
    /// `{$name//default}`.
    IncludeVar {
        name: &'src str,
        default: Option<&'src str>,
    },
    /// `@@body@@` / `@@body` (the closer, if present, is absorbed here).
    Escape(&'src str),
    /// A bare `http(s)://…` URL.
    Url(&'src str),
}

#[derive(Clone, Debug)]
pub(crate) enum OpenTag<'src> {
    Div {
        underscore: bool,
        params: Params,
    },
    Span {
        params: Params,
    },
    Anchor {
        params: Params,
    },
    Table {
        params: Params,
    },
    Row {
        params: Params,
    },
    Cell {
        header: bool,
        params: Params,
    },
    Collapsible {
        params: Params,
    },
    /// `[[size arg]]`
    Size(&'src str),
    /// `[[iftags filter]]`
    IfTags(&'src str),
    Align {
        floating: bool,
        side: AlignSide,
    },
    /// `[[tab name]]`; the name is sub-parsed by the merge. Recognized only
    /// as a tabview child — stray ones degrade to text.
    Tab {
        name: &'src str,
    },
    Tabview,
    Code,
    /// `[[module css …]]`
    Css,
    ListPages {
        params: Params,
    },
    /// A single-tag `[[module Name …]]` with no closer.
    Module(String),
    /// `[[include …]]`; `raw` is split by [`parse_include_args`].
    Include {
        raw: &'src str,
    },
    Image {
        align: Option<Align>,
        source: Vec<TextObj>,
        params: Params,
    },
    /// `[[head]]` / `[[body]]` / `[[foot]]` (open, with the slot) and
    /// `[[/head]]`-style closes (`None`). Recognized only inside a listpages
    /// body — stray ones degrade to text.
    Section(Option<SectionSlot>),
}

// =========================================================================
// Parser
// =========================================================================

pub(crate) fn lex(src: &str) -> Vec<Token<'_>> {
    tokens()
        .parse(src.as_bytes())
        .into_result()
        .expect("total lexer")
}

fn tokens<'a>() -> impl Parser<'a, In<'a>, Vec<Token<'a>>, E<'a>> + Clone + 'a {
    token_run().repeated().collect::<Vec<_>>().map(fold_tokens)
}

/// One dispatch step: a `>`-mark run, a `||` cell prefix, or any single token.
/// The choice is total — the [`single`] fallback's `text_run` always consumes
/// at least one byte — so `repeated` runs until end of input.
fn token_run<'a>() -> impl Parser<'a, In<'a>, TokenOut<'a>, E<'a>> + Clone + 'a {
    choice((
        quote_marks(),
        pipe2_prefix(),
        single().map_with(|tok, e| {
            let span: SimpleSpan = e.span();
            TokenOut::One(tok, span.start, span.end)
        }),
    ))
}

/// Flatten the dispatch steps into tokens — nothing is re-derived here, the
/// arms already computed every span.
fn fold_tokens(items: Vec<TokenOut<'_>>) -> Vec<Token<'_>> {
    let mut toks = Vec::with_capacity(items.len());
    for out in items {
        match out {
            TokenOut::One(tok, start, end) => toks.push(Token { tok, start, end }),
            TokenOut::Quote { start, count } => toks.extend((0..count).map(|k| Token {
                tok: Tok::QuoteMark,
                start: start + k,
                end: start + k + 1,
            })),
            TokenOut::Pipe2 {
                start,
                tilde,
                align,
            } => {
                toks.push(Token {
                    tok: Tok::Pipe2,
                    start,
                    end: start + 2,
                });
                if let Some(t) = tilde {
                    toks.push(Token {
                        tok: Tok::Tilde,
                        start: t,
                        end: t + 1,
                    });
                }
                if let Some((s, e, side)) = align {
                    toks.push(Token {
                        tok: Tok::CellAlign(side),
                        start: s,
                        end: e,
                    });
                }
            }
        }
    }
    toks
}

fn single<'a>() -> impl Parser<'a, In<'a>, Tok<'a>, E<'a>> + Clone + 'a {
    choice((
        list_mark(),
        just(b'\n').to(Tok::Newline),
        heading(),
        rule(),
        center_eq(),
        bracket(),
        just(b"--]").to(Tok::CommentClose),
        mark2(),
        just(b"^^").to(Tok::SupMark),
        just(b",,").to(Tok::SubMark),
        color_open(),
        module_var(),
        include_var(),
        escape(),
        url(),
        text_run(),
    ))
}

/// A zero-width assertion of the line-start context. True at the beginning of
/// input, right after a newline, and right after a `>`-mark run: the quote
/// grammar eats the marks and their trailing spaces, so the remainder of the
/// line lexes as if it were at a line start again (a `>`-run is the run of
/// `>`s and spaces that leads back to the line start through at least one
/// `>`).
fn at_line_start<'a>() -> impl Parser<'a, In<'a>, (), E<'a>> + Clone + 'a {
    custom(|inp: &mut InputRef<'a, '_, In<'a>, E<'a>>| {
        let b = inp.full_slice();
        let off = *inp.cursor().inner();
        if off == 0 || b[off - 1] == b'\n' {
            return Ok(());
        }
        let mut j = off;
        let mut saw_gt = false;
        while j > 0 && matches!(b[j - 1], b'>' | b' ') {
            saw_gt |= b[j - 1] == b'>';
            j -= 1;
        }
        if saw_gt && (j == 0 || b[j - 1] == b'\n') {
            return Ok(());
        }
        Err(perr(inp, "expected start of line"))
    })
}

/// Zero-width lookbehind: succeeds when the byte just before the cursor is
/// not `c` (at the beginning of input there is no such byte, so it succeeds).
fn not_after<'a>(c: u8) -> impl Parser<'a, In<'a>, (), E<'a>> + Clone + 'a {
    custom(move |inp: &mut InputRef<'a, '_, In<'a>, E<'a>>| {
        let b = inp.full_slice();
        let off = *inp.cursor().inner();
        if off > 0 && b[off - 1] == c {
            Err(perr(inp, "forbidden preceding byte"))
        } else {
            Ok(())
        }
    })
}

/// One indentation unit: a space or an NBSP (real pages indent lists with
/// copy-pasted non-breaking spaces).
fn ws<'a>() -> impl Parser<'a, In<'a>, (), E<'a>> + Clone + 'a {
    choice((just(b' ').to(()), just(b"\xc2\xa0").to(())))
}

/// Spaces before `inner`, counted in characters; the whole thing fails (and
/// the spaces are given back) unless `inner` matches right after them.
fn indentation<'a, P, O>(inner: P) -> impl Parser<'a, In<'a>, (usize, O), E<'a>> + Clone + 'a
where
    P: Parser<'a, In<'a>, O, E<'a>> + Clone + 'a,
    O: 'a,
{
    ws().repeated().count().then(inner)
}

/// Zero-width: the byte at the cursor differs from the one just before it —
/// a list marker may not immediately repeat (`**` is bold, not a list).
fn not_doubled<'a>() -> impl Parser<'a, In<'a>, (), E<'a>> + Clone + 'a {
    custom(|inp: &mut InputRef<'a, '_, In<'a>, E<'a>>| {
        let b = inp.full_slice();
        let off = *inp.cursor().inner();
        match (b.get(off), off.checked_sub(1)) {
            (Some(c), Some(p)) if b[p] == *c => Err(perr(inp, "doubled list mark")),
            _ => Ok(()),
        }
    })
}

/// The body up to `delim`, a newline, or end of input (the old `read_until`
/// semantics); the delimiter, when found, is consumed. Returns the body and
/// whether the delimiter (rather than the line's end) terminated it — the
/// callers' `choice` fallbacks turn a miss into a short degrade token.
fn read_until<'a>(
    delim: &'static [u8],
) -> impl Parser<'a, In<'a>, (&'a str, bool), E<'a>> + Clone + 'a {
    custom(move |inp: &mut InputRef<'a, '_, In<'a>, E<'a>>| {
        let b = inp.full_slice();
        let start = *inp.cursor().inner();
        let mut j = start;
        while j < b.len() && b[j] != b'\n' && !b[j..].starts_with(delim) {
            j += 1;
        }
        let found = b[j..].starts_with(delim);
        let end = if found { j + delim.len() } else { j };
        advance(inp, end - start);
        Ok((sub(b, start, j), found))
    })
}

/// A `>`-marker run: every `>` is one QuoteMark (a nesting level); the
/// run's spaces are eaten. See [`Tok::QuoteMark`].
fn quote_marks<'a>() -> impl Parser<'a, In<'a>, TokenOut<'a>, E<'a>> + Clone + 'a {
    at_line_start()
        .ignore_then(just(b'>').ignore_then(any().filter(|c| matches!(c, b'>' | b' ')).repeated()))
        .map_with(|(), e| {
            let span: SimpleSpan = e.span();
            TokenOut::Quote {
                start: span.start,
                count: e.slice().iter().filter(|&c| *c == b'>').count(),
            }
        })
}

fn list_mark<'a>() -> impl Parser<'a, In<'a>, Tok<'a>, E<'a>> + Clone + 'a {
    at_line_start().ignore_then(
        indentation(
            choice((just(b'*'), just(b'#')))
                .then_ignore(not_doubled())
                .then_ignore(ws().repeated()),
        )
        .map(|(indent, marker)| Tok::ListMark {
            ordered: marker == b'#',
            indent,
        }),
    )
}

/// `+`…`++++++` + spaces at a line start. `repeated` is greedy and does not
/// backtrack, but that never matters: after any shorter prefix of the same
/// run the next byte is still `+`, so only the full run can be followed by
/// the required space — a 7+ run hits the `at_most(6)` bound, fails the
/// space, and falls through to the one-byte degrade, exactly like the old
/// lexer's count check. (`count()` ignores `at_least`, so the lower bound is
/// a `filter`.)
fn heading<'a>() -> impl Parser<'a, In<'a>, Tok<'a>, E<'a>> + Clone + 'a {
    at_line_start().ignore_then(
        just(b'+')
            .repeated()
            .at_most(6)
            .count()
            .filter(|&n| n >= 1)
            .then_ignore(just(b' ').repeated().at_least(1))
            .map(|n| Tok::Heading(n as u32))
            .or(just(b'+').to(Tok::Text("+"))),
    )
}

/// A full line of 4+ dashes at a line start. The newline (or end of input)
/// terminates the run but is *not* consumed: it becomes the `Newline` token
/// that ends the line — merge's quote machinery relies on that for a
/// `> ----` quote line. The `\n`-or-EOF lookahead is zero-width, which pure
/// combinators cannot express, hence the `custom`.
fn rule<'a>() -> impl Parser<'a, In<'a>, Tok<'a>, E<'a>> + Clone + 'a {
    at_line_start().ignore_then(custom(|inp: &mut InputRef<'a, '_, In<'a>, E<'a>>| {
        let b = inp.full_slice();
        let start = *inp.cursor().inner();
        let mut j = start;
        while b.get(j) == Some(&b'-') {
            j += 1;
        }
        if j - start >= 4 && matches!(b.get(j), None | Some(&b'\n')) {
            advance(inp, j - start);
            Ok(Tok::Rule)
        } else {
            Err(perr(inp, "expected rule"))
        }
    }))
}

fn center_eq<'a>() -> impl Parser<'a, In<'a>, Tok<'a>, E<'a>> + Clone + 'a {
    at_line_start()
        .ignore_then(just(b'='))
        .ignore_then(just(b' ').repeated())
        .to(Tok::CenterEq)
}

/// `||` plus its cell prefix: `~` (header), then optional spaces around an
/// alignment char. The prefix's spaces are eaten (they are part of its
/// grammar); a [`Tok::CellAlign`]'s span swallows the spaces before the char
/// (merge needs them when the pipes degrade to text).
fn pipe2_prefix<'a>() -> impl Parser<'a, In<'a>, TokenOut<'a>, E<'a>> + Clone + 'a {
    just(b"||")
        .ignore_then(
            just(b'~')
                .map_with(|_, e| {
                    let span: SimpleSpan = e.span();
                    span.start
                })
                .or_not(),
        )
        .then(
            just(b' ')
                .repeated()
                .ignore_then(choice((just(b'<'), just(b'='), just(b'>'))).map(align_side))
                .map_with(|side, e| {
                    let span: SimpleSpan = e.span();
                    (span.start, span.end, side)
                })
                .then_ignore(just(b' ').repeated())
                .or_not(),
        )
        .map_with(|(tilde, align), e| {
            let span: SimpleSpan = e.span();
            TokenOut::Pipe2 {
                start: span.start,
                tilde,
                align,
            }
        })
}

fn align_side(c: u8) -> AlignSide {
    match c {
        b'<' => AlignSide::Left,
        b'=' => AlignSide::Center,
        b'>' => AlignSide::Right,
        _ => unreachable!(),
    }
}

/// `//`, `**`, `__`, `--` — or a lone sigil as a one-byte text token (the
/// old parser's character fallback).
fn mark2<'a>() -> impl Parser<'a, In<'a>, Tok<'a>, E<'a>> + Clone + 'a {
    choice((
        just(b"//").to(Tok::Mark(TextStyle::Italic)),
        just(b"**").to(Tok::Mark(TextStyle::Bold)),
        just(b"__").to(Tok::Mark(TextStyle::Underline)),
        just(b"--").to(Tok::Mark(TextStyle::Strikethrough)),
        just(b'/').to(Tok::Text("/")),
        just(b'*').to(Tok::Text("*")),
        just(b'_').to(Tok::Text("_")),
        just(b'-').to(Tok::Text("-")),
    ))
}

/// `##spec|` — the `|` must appear on this line (the old `read_until` stopped
/// at newlines); otherwise the `##` is a closer (or plain text when it
/// degrades).
fn color_open<'a>() -> impl Parser<'a, In<'a>, Tok<'a>, E<'a>> + Clone + 'a {
    choice((
        just(b"##")
            .ignore_then(read_until(b"|"))
            .filter(|(_, found)| *found)
            .map(|(spec, _)| Tok::ColorOpen(spec)),
        just(b"##").to(Tok::ColorClose),
    ))
}

fn module_var<'a>() -> impl Parser<'a, In<'a>, Tok<'a>, E<'a>> + Clone + 'a {
    choice((
        just(b"%%")
            .ignore_then(read_until(b"%%"))
            .filter(|(_, found)| *found)
            .map(|(raw, _)| match raw.split_once('|') {
                Some((name, default)) => Tok::ModuleVar {
                    name,
                    default: Some(default),
                },
                None => Tok::ModuleVar {
                    name: raw,
                    default: None,
                },
            }),
        just(b"%%").to(Tok::Text("%%")),
    ))
}

fn include_var<'a>() -> impl Parser<'a, In<'a>, Tok<'a>, E<'a>> + Clone + 'a {
    choice((
        just(b"{$")
            .ignore_then(read_until(b"}"))
            .filter(|(_, found)| *found)
            .map(|(raw, _)| match raw.split_once("//") {
                Some((name, default)) => Tok::IncludeVar {
                    name,
                    default: Some(default),
                },
                None => Tok::IncludeVar {
                    name: raw,
                    default: None,
                },
            }),
        just(b"{$").to(Tok::Text("{$")),
    ))
}

fn escape<'a>() -> impl Parser<'a, In<'a>, Tok<'a>, E<'a>> + Clone + 'a {
    just(b"@@")
        .ignore_then(read_until(b"@@"))
        .map(|(body, _)| Tok::Escape(body))
}

fn url<'a>() -> impl Parser<'a, In<'a>, Tok<'a>, E<'a>> + Clone + 'a {
    choice((just(b"http://").to(()), just(b"https://").to(())))
        .ignore_then(any().filter(|c| is_url_char(*c)).repeated())
        .map_with(|(), e| Tok::Url(str::from_utf8(e.slice()).expect("char boundary")))
}

/// A maximal run of bytes that cannot begin a construct. `>` `+` `=` never
/// stop it mid-line (their line constructs were already tried and declined),
/// and `h` stops it only when it starts a URL.
fn text_run<'a>() -> impl Parser<'a, In<'a>, Tok<'a>, E<'a>> + Clone + 'a {
    custom(|inp: &mut InputRef<'a, '_, In<'a>, E<'a>>| {
        let b = inp.full_slice();
        let start = *inp.cursor().inner();
        let mut j = start;
        while j < b.len() {
            let stop = match b[j] {
                b'\n' | b'[' | b'@' | b'%' | b'{' | b'#' | b'/' | b'*' | b'_' | b'-' | b'^'
                | b',' | b'|' => true,
                b'h' => b[j..].starts_with(b"http://") || b[j..].starts_with(b"https://"),
                _ => false,
            };
            if stop {
                break;
            }
            j += 1;
        }
        // The run can only stop at an ASCII byte; when it stops immediately
        // (a lone sigil), that byte is consumed alone so lexing stays total.
        // At end of input there is nothing to consume: fail so `repeated`
        // terminates.
        if j == start {
            if j >= b.len() {
                return Err(perr(inp, "expected any byte"));
            }
            j += 1;
        }
        advance(inp, j - start);
        Ok(Tok::Text(sub(b, start, j)))
    })
}

/// Everything starting at a `[`: `[!--`, `[[[…]]]`, a known `[[…]]`
/// construct, `[url text]`, or a lone `[`/`[[` degrade.
fn bracket<'a>() -> impl Parser<'a, In<'a>, Tok<'a>, E<'a>> + Clone + 'a {
    choice((
        just(b"[!--").to(Tok::CommentOpen),
        link3(),
        known_bracket(),
        not_after(b'[').ignore_then(link1()),
        just(b"[[")
            .to(Tok::Text("[["))
            .or(just(b"[").to(Tok::Text("["))),
    ))
}

/// `[[[target|text]]]` — or, when the construct does not close on this
/// shape, the two-bracket degrade.
fn link3<'a>() -> impl Parser<'a, In<'a>, Tok<'a>, E<'a>> + Clone + 'a {
    custom(|inp: &mut InputRef<'a, '_, In<'a>, E<'a>>| {
        let b = inp.full_slice();
        let start = *inp.cursor().inner();
        if !b[start..].starts_with(b"[[[") {
            return Err(perr(inp, "expected '[[['"));
        }
        match lex_link3(b, start) {
            Some((end, target, text)) => {
                advance(inp, end - start);
                Ok(Tok::Link3 { target, text })
            }
            // `[[[` whose body did not close: degrade to a `[[` text token
            // (the third `[` is then re-lexed by the next dispatch step).
            None => {
                advance(inp, 2);
                Ok(Tok::Text("[["))
            }
        }
    })
}

/// A known `[[…]]` construct: a closer, an opener, or a listpages section
/// marker (keywords matched by the [`OPENERS`]/[`CLOSERS`] tables).
fn known_bracket<'a>() -> impl Parser<'a, In<'a>, Tok<'a>, E<'a>> + Clone + 'a {
    custom(|inp: &mut InputRef<'a, '_, In<'a>, E<'a>>| {
        let b = inp.full_slice();
        let start = *inp.cursor().inner();
        if !b[start..].starts_with(b"[[") {
            return Err(perr(inp, "expected '[[ construct'"));
        }
        let Some((end, tok)) = lex_bracket(b, start) else {
            return Err(perr(inp, "unknown '[[ construct'"));
        };
        advance(inp, end - start);
        Ok(tok)
    })
}

/// `[url text]` / `[url]`, rejected when the `[` is followed by `[`, `!`,
/// `]` or a newline. The leading `[` is consumed by [`just`] so the cursor
/// entering [`lex_link1`] is past it (the helper reads from the bracket's
/// position, `after - 1`).
fn link1<'a>() -> impl Parser<'a, In<'a>, Tok<'a>, E<'a>> + Clone + 'a {
    just(b'[').ignore_then(custom(|inp: &mut InputRef<'a, '_, In<'a>, E<'a>>| {
        let b = inp.full_slice();
        let after = *inp.cursor().inner();
        let Some((end, target, text)) = lex_link1(b, after - 1) else {
            return Err(perr(inp, "expected link"));
        };
        advance(inp, end - after);
        Ok(Tok::Link1 { target, text })
    }))
}

// =========================================================================
// Byte helpers
// =========================================================================

fn perr<'a>(inp: &mut InputRef<'a, '_, In<'a>, E<'a>>, msg: &'static str) -> Rich<'a, u8> {
    let cur = inp.cursor();
    Rich::custom(inp.span_since(&cur), msg)
}

fn advance<'a>(inp: &mut InputRef<'a, '_, In<'a>, E<'a>>, n: usize) {
    for _ in 0..n {
        inp.next();
    }
}

/// A `&str` payload over a byte range. The scans only stop at ASCII
/// delimiters (or char starts when stepping by [`char_len_at`]), so both ends
/// are always char boundaries and the slice is valid UTF-8.
fn sub(b: &[u8], start: usize, end: usize) -> &str {
    str::from_utf8(&b[start..end]).expect("char boundary")
}

/// The length of the UTF-8 character starting at `k` (0 past the end; a
/// continuation byte — unreachable at scan positions — reads as 1).
fn char_len_at(b: &[u8], k: usize) -> usize {
    match b.get(k) {
        None => 0,
        Some(0xC0..=0xDF) => 2,
        Some(0xE0..=0xEF) => 3,
        Some(0xF0..=0xF4) => 4,
        Some(_) => 1,
    }
}

/// Attribute-context whitespace: real pages carry copy-pasted non-breaking
/// spaces inside module headers (`[[module ListPages\u{a0}category=…]]`).
fn is_param_ws(b: &[u8], i: usize) -> bool {
    b.get(i) == Some(&b' ') || b[i..].starts_with(&[0xC2, 0xA0])
}

fn is_prop_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, b'_' | b'#')
}

fn is_url_char(c: u8) -> bool {
    c.is_ascii_alphanumeric()
        || matches!(
            c,
            b'-' | b'.'
                | b'_'
                | b'~'
                | b':'
                | b'/'
                | b'?'
                | b'#'
                | b'@'
                | b'!'
                | b'$'
                | b'&'
                | b'\''
                | b'('
                | b')'
                | b'*'
                | b'+'
                | b','
                | b';'
                | b'='
        )
}

/// The closing-tag keywords, longest first so prefixes don't shadow
/// (`[[/tabview]]` over `[[/tab]]`, `[[/==]]` over `[[/=]]`).
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

/// The tail parser of an opener keyword: bytes right after the keyword →
/// the construct's end offset and its parsed tag.
type Tail = fn(&[u8], usize) -> Option<(usize, OpenTag)>;

/// The opener keywords and their tail parsers (a keyword matches by prefix,
/// exactly like the old `kw_ci` combinators; `hcell` shadows `cell`, `table`
/// and `tabview` shadow `tab`).
const OPENERS: &[(&[u8], Tail)] = &[
    (b"collapsible", collapsible_tail),
    (b"tabview", tabview_tail),
    (b"hcell", hcell_tail),
    (b"table", table_tail),
    (b"iftags", iftags_tail),
    (b"module", module_tail),
    (b"include", include_tail),
    (b"image", image_tail),
    (b"cell", cell_tail),
    (b"size", size_tail),
    (b"span", span_tail),
    (b"code", code_tail),
    (b"head", head_tail),
    (b"body", body_tail),
    (b"foot", foot_tail),
    (b"div", div_tail),
    (b"tab", tab_top_tail),
    (b"row", row_tail),
    (b"a", anchor_tail),
];

fn ci_starts(rest: &[u8], kw: &[u8]) -> bool {
    rest.len() >= kw.len() && rest[..kw.len()].eq_ignore_ascii_case(kw)
}

fn skip_spaces(b: &[u8], mut k: usize) -> usize {
    while b.get(k) == Some(&b' ') {
        k += 1;
    }
    k
}

/// Earliest of `delim` searching from `k`, not crossing a newline (the old
/// `read_until` semantics: delimiters, `\n`, or end of input — earliest wins).
/// `None` when `delim` never occurs before the line ends.
fn read_to(b: &[u8], k: usize, delim: &[u8]) -> Option<usize> {
    let mut j = k;
    while j < b.len() {
        if b[j] == b'\n' {
            return None;
        }
        if b[j..].starts_with(delim) {
            return Some(j);
        }
        j += 1;
    }
    None
}

/// `[[[target|text]]]` — one token spanning to the first `]]]`.
fn lex_link3(b: &[u8], i: usize) -> Option<(usize, &str, Option<&str>)> {
    let mut j = i + 3;
    while j < b.len() && b[j] != b'|' && b[j] != b'\n' && !b[j..].starts_with(b"]]]") {
        j += 1;
    }
    if b[j..].starts_with(b"]]]") {
        return Some((j + 3, sub(b, i + 3, j), None));
    }
    if b.get(j) != Some(&b'|') {
        return None;
    }
    let target = sub(b, i + 3, j);
    let mut k = j + 1;
    while k < b.len() && !b[k..].starts_with(b"]]]") {
        // The old `content_before` body stopped at any `[[/…]]` closer, and
        // the then-required `]]]` failed — so the whole link degrades.
        if b[k..].starts_with(b"[[/") {
            return None;
        }
        k += 1;
    }
    if !b.get(k..).is_some_and(|r| r.starts_with(b"]]]")) {
        return None;
    }
    Some((k + 3, target, Some(sub(b, j + 1, k))))
}

/// `[url text]` / `[url]`, rejected when the `[` is followed by `[`, `!`,
/// `]` or a newline (the predecessor check lives in the caller).
fn lex_link1(b: &[u8], i: usize) -> Option<(usize, &str, Option<&str>)> {
    match b.get(i + 1) {
        Some(b'[') | Some(b'!') | Some(b']') | Some(b'\n') | None => return None,
        _ => {}
    }
    let mut j = i + 1;
    while j < b.len() && b[j] != b' ' && b[j] != b']' && b[j] != b'\n' {
        j += 1;
    }
    let target = sub(b, i + 1, j);
    match b.get(j) {
        Some(b']') => Some((j + 1, target, None)),
        Some(b' ') => {
            let mut t_end = j;
            while t_end < b.len() && b[t_end] != b']' {
                t_end += 1;
            }
            let text = sub(b, j + 1, t_end);
            let end = if t_end < b.len() { t_end + 1 } else { t_end };
            Some((end, target, Some(text)))
        }
        _ => Some((j, target, None)),
    }
}

/// A known `[[…]]` construct at `i`: a closer, an opener, or a listpages
/// section marker. Keywords sit directly after `[[` (the only exceptions:
/// closers and section markers may carry inner spaces, and `[[ tab]]`).
fn lex_bracket<'src>(b: &'src [u8], i: usize) -> Option<(usize, Tok<'src>)> {
    let j = i + 2;
    if b.get(j) == Some(&b'/') {
        let mut k = j + 1;
        while b.get(k) == Some(&b' ') {
            k += 1;
        }
        for (kw, tag) in CLOSERS {
            if ci_starts(&b[k..], kw) {
                let mut m = k + kw.len();
                while b.get(m) == Some(&b' ') {
                    m += 1;
                }
                if b[m..].starts_with(b"]]") {
                    return Some((m + 2, Tok::Close(tag.clone())));
                }
            }
        }
        for kw in [b"head", b"body", b"foot"] {
            if ci_starts(&b[k..], kw)
                && let Some(end) = marker_end(b, k + kw.len())
            {
                return Some((end, Tok::Open(OpenTag::Section(None))));
            }
        }
        return None;
    }
    // Exact alignment openers (no params, no inner spaces): `[[f<]]`, `[[==]]`…
    for (form, floating, side) in [
        (&b"f<]]"[..], true, AlignSide::Left),
        (&b"f>]]"[..], true, AlignSide::Right),
        (&b"==]]"[..], false, AlignSide::Justify),
        (&b"<]]"[..], false, AlignSide::Left),
        (&b">]]"[..], false, AlignSide::Right),
        (&b"=]]"[..], false, AlignSide::Center),
    ] {
        if b[j..].starts_with(form) {
            return Some((j + form.len(), Tok::Open(OpenTag::Align { floating, side })));
        }
    }
    if let Some((end, tag)) = lex_image(b, j) {
        return Some((end, Tok::Open(tag)));
    }
    for (kw, tail) in OPENERS {
        if ci_starts(&b[j..], kw)
            && let Some((end, tag)) = tail(b, j + kw.len())
        {
            return Some((end, Tok::Open(tag)));
        }
    }
    // `[[ tab name]]`: the only opener tolerating spaces before its keyword.
    let mut k = j;
    while b.get(k) == Some(&b' ') {
        k += 1;
    }
    if k > j
        && ci_starts(&b[k..], b"tab")
        && let Some((end, name)) = tab_name(b, k + 3)
    {
        return Some((end, Tok::Open(OpenTag::Tab { name })));
    }
    None
}

/// `kw spaces ]]` — the shared tail of the section markers (open form).
fn marker_end(b: &[u8], mut m: usize) -> Option<usize> {
    while b.get(m) == Some(&b' ') {
        m += 1;
    }
    b.get(m..)
        .is_some_and(|r| r.starts_with(b"]]"))
        .then_some(m + 2)
}

fn div_tail(b: &[u8], j: usize) -> Option<(usize, OpenTag<'_>)> {
    let underscore = b.get(j) == Some(&b'_');
    let k = if underscore { j + 1 } else { j };
    let mut params = Params::new();
    let k = lex_params(b, k, &mut params);
    let k = skip_spaces(b, k);
    b.get(k..)
        .is_some_and(|r| r.starts_with(b"]]"))
        .then(|| (k + 2, OpenTag::Div { underscore, params }))
}

fn span_tail(b: &[u8], j: usize) -> Option<(usize, OpenTag<'_>)> {
    container_tail(b, j, |params| OpenTag::Span { params })
}

fn table_tail(b: &[u8], j: usize) -> Option<(usize, OpenTag<'_>)> {
    container_tail(b, j, |params| OpenTag::Table { params })
}

fn row_tail(b: &[u8], j: usize) -> Option<(usize, OpenTag<'_>)> {
    container_tail(b, j, |params| OpenTag::Row { params })
}

fn anchor_tail(b: &[u8], j: usize) -> Option<(usize, OpenTag<'_>)> {
    container_tail(b, j, |params| OpenTag::Anchor { params })
}

/// `[[KW _? params spaces ]]` — the shape shared by span/table/row/a.
fn container_tail<'src, F: Fn(Params) -> OpenTag<'src>>(
    b: &'src [u8],
    j: usize,
    build: F,
) -> Option<(usize, OpenTag<'src>)> {
    let k = if b.get(j) == Some(&b'_') { j + 1 } else { j };
    let mut params = Params::new();
    let k = lex_params(b, k, &mut params);
    let k = skip_spaces(b, k);
    b.get(k..)
        .is_some_and(|r| r.starts_with(b"]]"))
        .then(|| (k + 2, build(params)))
}

fn cell_tail(b: &[u8], j: usize) -> Option<(usize, OpenTag<'_>)> {
    cell_tail_with(b, j, false)
}

fn hcell_tail(b: &[u8], j: usize) -> Option<(usize, OpenTag<'_>)> {
    cell_tail_with(b, j, true)
}

fn cell_tail_with(b: &[u8], j: usize, header: bool) -> Option<(usize, OpenTag<'_>)> {
    let mut params = Params::new();
    let k = lex_params(b, j, &mut params);
    let k = skip_spaces(b, k);
    b.get(k..)
        .is_some_and(|r| r.starts_with(b"]]"))
        .then(|| (k + 2, OpenTag::Cell { header, params }))
}

fn collapsible_tail(b: &[u8], j: usize) -> Option<(usize, OpenTag<'_>)> {
    let mut params = Params::new();
    let k = lex_params(b, j, &mut params);
    let k = skip_spaces(b, k);
    b.get(k..)
        .is_some_and(|r| r.starts_with(b"]]"))
        .then(|| (k + 2, OpenTag::Collapsible { params }))
}

fn tabview_tail(b: &[u8], j: usize) -> Option<(usize, OpenTag<'_>)> {
    let mut params = Params::new();
    let k = lex_params(b, j, &mut params);
    let k = skip_spaces(b, k);
    b.get(k..)
        .is_some_and(|r| r.starts_with(b"]]"))
        .then_some((k + 2, OpenTag::Tabview))
}

fn size_tail(b: &[u8], j: usize) -> Option<(usize, OpenTag<'_>)> {
    if b.get(j) != Some(&b' ') {
        return None;
    }
    let k = skip_spaces(b, j);
    let end = read_to(b, k, b"]]")?;
    let arg = sub(b, k, end).trim();
    Some((end + 2, OpenTag::Size(arg)))
}

fn iftags_tail(b: &[u8], j: usize) -> Option<(usize, OpenTag<'_>)> {
    if b.get(j) != Some(&b' ') {
        return None;
    }
    let k = skip_spaces(b, j);
    let end = read_to(b, k, b"]]")?;
    Some((end + 2, OpenTag::IfTags(sub(b, k, end))))
}

fn code_tail(b: &[u8], j: usize) -> Option<(usize, OpenTag<'_>)> {
    let end = read_to(b, j, b"]]")?;
    Some((end + 2, OpenTag::Code))
}

fn head_tail(b: &[u8], j: usize) -> Option<(usize, OpenTag<'_>)> {
    marker_end(b, j).map(|end| (end, OpenTag::Section(Some(SectionSlot::Head))))
}

fn body_tail(b: &[u8], j: usize) -> Option<(usize, OpenTag<'_>)> {
    marker_end(b, j).map(|end| (end, OpenTag::Section(Some(SectionSlot::Body))))
}

fn foot_tail(b: &[u8], j: usize) -> Option<(usize, OpenTag<'_>)> {
    marker_end(b, j).map(|end| (end, OpenTag::Section(Some(SectionSlot::Foot))))
}

/// `[[tab name]]` at top level (no leading spaces; the spaced form is a
/// special case in [`lex_bracket`]).
fn tab_top_tail(b: &[u8], j: usize) -> Option<(usize, OpenTag<'_>)> {
    tab_name(b, j).map(|(end, name)| (end, OpenTag::Tab { name }))
}

/// The `name]]` tail of a tab opener, spaces before the name included. The
/// name runs to `]]` crossing newlines (the old body stopped at `]]` only).
fn tab_name(b: &[u8], j: usize) -> Option<(usize, &str)> {
    let k = skip_spaces(b, j);
    let mut end = k;
    while end < b.len() && !b[end..].starts_with(b"]]") {
        end += 1;
    }
    b.get(end..)
        .is_some_and(|r| r.starts_with(b"]]"))
        .then_some((end + 2, sub(b, k, end)))
}

fn module_tail(b: &[u8], j: usize) -> Option<(usize, OpenTag<'_>)> {
    if b.get(j) != Some(&b' ') {
        return None;
    }
    let k = skip_spaces(b, j);
    let lower = b[k..].to_ascii_lowercase();
    if lower.starts_with(b"css") {
        let end = read_to(b, k + 3, b"]]")?;
        return Some((end + 2, OpenTag::Css));
    }
    if lower.starts_with(b"listpages") {
        let mut m = k + 9;
        let mut params = Params::new();
        m = lex_params(b, m, &mut params);
        m = skip_spaces(b, m);
        if b.get(m..).is_some_and(|r| r.starts_with(b"]]")) {
            return Some((m + 2, OpenTag::ListPages { params }));
        }
        return None;
    }
    let mut n = 0;
    while b.get(k + n).is_some_and(|c| c.is_ascii_alphabetic()) {
        n += 1;
    }
    if n == 0 {
        return None;
    }
    let name = sub(b, k, k + n).to_string();
    let mut m = k + n;
    while m < b.len() && b[m] != b'\n' && !b[m..].starts_with(b"]]") {
        m += 1;
    }
    if b[m..].starts_with(b"]]") {
        m += 2;
    }
    Some((m, OpenTag::Module(name)))
}

fn include_tail(b: &[u8], j: usize) -> Option<(usize, OpenTag<'_>)> {
    if b.get(j) != Some(&b' ') {
        return None;
    }
    let k = skip_spaces(b, j);
    // Balanced `]]` scan: a nested `[[…]]` inside a value must not close the
    // directive (port of the old `read_include_body`).
    let bytes = &b[k..];
    let mut p = 0usize;
    let mut depth = 1i32;
    while p + 1 < bytes.len() {
        if bytes[p] == b'[' && bytes[p + 1] == b'[' {
            depth += 1;
            p += 2;
        } else if bytes[p] == b']' && bytes[p + 1] == b']' {
            depth -= 1;
            if depth == 0 {
                return Some((
                    k + p + 2,
                    OpenTag::Include {
                        raw: sub(b, k, k + p),
                    },
                ));
            }
            p += 2;
        } else {
            p += 1;
        }
    }
    None
}

fn image_tail(b: &[u8], j: usize) -> Option<(usize, OpenTag<'_>)> {
    lex_image(b, j)
}

/// `[[<image source params]]` with an optional alignment prefix.
fn lex_image(b: &[u8], j: usize) -> Option<(usize, OpenTag<'_>)> {
    let (align, mut m) = if b[j..].starts_with(b"f<") {
        (
            Some(Align {
                floating: true,
                side: AlignSide::Left,
            }),
            j + 2,
        )
    } else if b[j..].starts_with(b"f>") {
        (
            Some(Align {
                floating: true,
                side: AlignSide::Right,
            }),
            j + 2,
        )
    } else {
        let side = match b.get(j) {
            Some(b'<') => Some(AlignSide::Left),
            Some(b'>') => Some(AlignSide::Right),
            Some(b'=') => Some(AlignSide::Center),
            _ => None,
        };
        match side {
            Some(side) => (
                Some(Align {
                    floating: false,
                    side,
                }),
                j + 1,
            ),
            None => (None, j),
        }
    };
    if !ci_starts(&b[m..], b"image") {
        return None;
    }
    m += 5;
    if b.get(m) != Some(&b' ') {
        return None;
    }
    m = skip_spaces(b, m);
    let (after_source, source) = collect_text_objs(b, m, &[b" ", b"]]"]);
    let mut params = Params::new();
    let end = lex_params(b, after_source, &mut params);
    let end = skip_spaces(b, end);
    b.get(end..).is_some_and(|r| r.starts_with(b"]]")).then(|| {
        (
            end + 2,
            OpenTag::Image {
                align,
                source,
                params,
            },
        )
    })
}

/// `key="value"` / `key=value` attributes. Ports the old `params_block`
/// byte-for-byte, including the quirk that a key without `=` consumes the one
/// character that follows it before giving up.
fn lex_params(b: &[u8], mut k: usize, out: &mut Params) -> usize {
    loop {
        while k < b.len() && is_param_ws(b, k) {
            k += char_len_at(b, k);
        }
        match b.get(k) {
            None | Some(b']') | Some(b'\n') => return k,
            _ => {}
        }
        let key_start = k;
        while b.get(k).is_some_and(|c| is_prop_char(*c)) {
            k += 1;
        }
        if k == key_start {
            return k;
        }
        let key = sub(b, key_start, k).to_ascii_lowercase();
        if b.get(k) != Some(&b'=') {
            return k + char_len_at(b, k);
        }
        k += 1;
        let value = if b.get(k) == Some(&b'"') {
            k += 1;
            let (nk, v) = collect_text_objs(b, k, &[b"\""]);
            k = nk;
            if b.get(k) == Some(&b'"') {
                k += 1;
            }
            v
        } else {
            let (nk, v) = collect_text_objs(b, k, &[b" ", b"\xc2\xa0", b"]"]);
            k = nk;
            v
        };
        out.insert(key, value);
    }
}

/// A run of [`TextObj`]s — plain text interleaved with `%%var%%` and
/// `{$var}` substitutions — up to any of `stops`, a newline, or EOF. Ports
/// the old `collect_text_objs` including its variable-reading quirks (a
/// default may run past a newline; an include default is sub-parsed markup).
pub(crate) fn collect_text_objs(b: &[u8], mut k: usize, stops: &[&[u8]]) -> (usize, Vec<TextObj>) {
    let mut result = Vec::new();
    let mut buf = String::new();
    loop {
        if k >= b.len() || b[k] == b'\n' || stops.iter().any(|s| b[k..].starts_with(s)) {
            break;
        }
        if b[k..].starts_with(b"%%") {
            if !buf.is_empty() {
                result.push(TextObj::Plain(std::mem::take(&mut buf)));
            }
            k += 2;
            let name_start = k;
            while b.get(k).is_some_and(|c| is_prop_char(*c)) {
                k += 1;
            }
            let name = sub(b, name_start, k).to_string();
            let default = if b.get(k) == Some(&b'|') {
                k += 1;
                let d_start = k;
                while k < b.len() && !b[k..].starts_with(b"%%") {
                    k += 1;
                }
                let d = sub(b, d_start, k).to_string();
                if b[k..].starts_with(b"%%") {
                    k += 2;
                }
                Some(d)
            } else {
                if b[k..].starts_with(b"%%") {
                    k += 2;
                }
                None
            };
            result.push(TextObj::ModuleVar { name, default });
            continue;
        }
        if b[k..].starts_with(b"{$") {
            if !buf.is_empty() {
                result.push(TextObj::Plain(std::mem::take(&mut buf)));
            }
            k += 2;
            let name_start = k;
            while b.get(k).is_some_and(|c| is_prop_char(*c)) {
                k += 1;
            }
            let name = sub(b, name_start, k).to_string();
            let default = if b[k..].starts_with(b"//") {
                k += 2;
                let d_start = k;
                while k < b.len() && b[k] != b'}' {
                    k += 1;
                }
                let d = sub(b, d_start, k);
                if b.get(k) == Some(&b'}') {
                    k += 1;
                }
                Some(parse(d))
            } else {
                if b.get(k) == Some(&b'}') {
                    k += 1;
                }
                None
            };
            result.push(TextObj::IncludeVar { name, default });
            continue;
        }
        let l = char_len_at(b, k);
        buf.push_str(sub(b, k, k + l));
        k += l;
    }
    if !buf.is_empty() {
        result.push(TextObj::Plain(buf));
    }
    (k, result)
}
