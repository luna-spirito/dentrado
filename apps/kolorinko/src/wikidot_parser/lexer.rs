//! One linear scan turning a page into rich [`Tok`]s. Total and lossless:
//! every byte lands in exactly one token (the only exceptions are the spaces a
//! construct's own grammar eats: after a `>` quote mark and inside a table
//! cell prefix, both reconstructed from token spans when they degrade), so no
//! input can fail to lex.
//!
//! Tokens are rich: bracket constructs arrive pre-split into openers and
//! closers with their attributes parsed, inline markers (`//`, `##`, `%%`) are
//! their own tokens, and line constructs (quote marks, headings, rules, list
//! markers) are recognized at line starts. The merge pass then pairs tokens
//! into [`Node`]s without ever re-scanning source text; verbatim bodies
//! (`[[code]]`, `[[module css]]`) are sliced straight from the source via the
//! token spans.

use super::*;

pub(crate) type Params = HashMap<String, Vec<TextObj>>;

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
    /// A full line of 4+ dashes at a line start.
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

/// Attribute-context whitespace: real pages carry copy-pasted non-breaking
/// spaces inside module headers (`[[module ListPages\u{a0}category=…]]`).
fn is_param_ws(src: &str, i: usize) -> bool {
    src[i..].starts_with(' ') || src[i..].starts_with('\u{a0}')
}

fn is_prop_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '#'
}

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

/// The opener keywords and their tail parsers (a keyword matches by prefix,
/// exactly like the old `kw_ci` combinators; `hcell` shadows `cell`, `table`
/// and `tabview` shadow `tab`).
const OPENERS: &[(&[u8], fn(&str, usize) -> Option<(usize, OpenTag)>)] = &[
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
fn read_to(src: &str, k: usize, delim: &[u8]) -> Option<usize> {
    let b = src.as_bytes();
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

pub(crate) fn lex(src: &str) -> Vec<Token<'_>> {
    let b = src.as_bytes();
    let mut toks: Vec<Token> = Vec::new();
    let mut i = 0usize;
    let mut at_ls = true;

    macro_rules! push {
        ($tok:expr, $start:expr, $end:expr) => {{
            let is_nl = matches!($tok, Tok::Newline);
            toks.push(Token {
                tok: $tok,
                start: $start,
                end: $end,
            });
            at_ls = is_nl;
        }};
    }

    while i < b.len() {
        // A list marker, possibly indented, may only appear at a line start.
        // On a miss the general dispatch below still applies (`**bold**`,
        // `##color##` are inline markup, indent spaces are text).
        if at_ls
            && matches!(b[i], b'*' | b'#' | b' ' | 0xC2)
            && let Some((end, ordered, indent)) = lex_list_mark(src, i)
        {
            let start = i;
            i = end;
            push!(Tok::ListMark { ordered, indent }, start, i);
            continue;
        }
        let start = i;
        match b[i] {
            b'\n' => {
                i += 1;
                push!(Tok::Newline, start, i);
            }
            // A `>`-marker run: `>`s and spaces from the line start. Every `>`
            // is one QuoteMark (a nesting level). The rest of the line is
            // lexed as a fresh line start — stripping one marker level (what
            // the quote merge does) restores exactly that context, so `> + h`
            // is a heading and `> ----` a rule *inside* the quote.
            b'>' if at_ls => {
                while i < b.len() && (b[i] == b'>' || b[i] == b' ') {
                    if b[i] == b'>' {
                        let s = i;
                        i += 1;
                        toks.push(Token {
                            tok: Tok::QuoteMark,
                            start: s,
                            end: i,
                        });
                    } else {
                        i += 1;
                    }
                }
                at_ls = true;
            }
            b'+' if at_ls => {
                let mut n = 0;
                while i + n < b.len() && b[i + n] == b'+' {
                    n += 1;
                }
                if (1..=6).contains(&n) && b.get(i + n) == Some(&b' ') {
                    i += n;
                    while i < b.len() && b[i] == b' ' {
                        i += 1;
                    }
                    push!(Tok::Heading(n as u32), start, i);
                } else {
                    i += 1;
                    push!(Tok::Text(&src[start..i]), start, i);
                }
            }
            b'-' if at_ls && {
                let mut j = i;
                while j < b.len() && b[j] == b'-' {
                    j += 1;
                }
                j - i >= 4 && matches!(b.get(j), None | Some(b'\n'))
            } =>
            {
                while i < b.len() && b[i] == b'-' {
                    i += 1;
                }
                push!(Tok::Rule, start, i);
            }
            b'=' if at_ls => {
                i += 1;
                while i < b.len() && b[i] == b' ' {
                    i += 1;
                }
                push!(Tok::CenterEq, start, i);
            }
            b'|' if b.get(i + 1) == Some(&b'|') => {
                i += 2;
                push!(Tok::Pipe2, start, i);
                // Cell prefix: `~` immediately after `||`, then optional
                // spaces around an alignment char. Spaces are eaten (they are
                // part of the prefix grammar) and restored from the span when
                // the pipe degrades to text.
                if b.get(i) == Some(&b'~') {
                    let s = i;
                    i += 1;
                    push!(Tok::Tilde, s, i);
                }
                let after_tilde = i;
                let mut j = i;
                while j < b.len() && b[j] == b' ' {
                    j += 1;
                }
                let side = match b.get(j) {
                    Some(b'<') => Some(AlignSide::Left),
                    Some(b'=') => Some(AlignSide::Center),
                    Some(b'>') => Some(AlignSide::Right),
                    _ => None,
                };
                if let Some(side) = side {
                    i = j + 1;
                    push!(Tok::CellAlign(side), after_tilde, i);
                    while i < b.len() && b[i] == b' ' {
                        i += 1;
                    }
                }
            }
            b'[' => {
                if src[i..].starts_with("[!--") {
                    i += 4;
                    push!(Tok::CommentOpen, start, i);
                } else if src[i..].starts_with("[[[") {
                    if let Some((end, target, text)) = lex_link3(src, i) {
                        i = end;
                        push!(Tok::Link3 { target, text }, start, i);
                    } else {
                        i += 2;
                        push!(Tok::Text("[["), start, i);
                    }
                } else if src[i..].starts_with("[[") {
                    if let Some((end, tok)) = lex_bracket(src, i) {
                        i = end;
                        push!(tok, start, i);
                    } else {
                        i += 2;
                        push!(Tok::Text("[["), start, i);
                    }
                } else if b.get(i.wrapping_sub(1)) != Some(&b'[')
                    && let Some((end, target, text)) = lex_link1(src, i)
                {
                    i = end;
                    push!(Tok::Link1 { target, text }, start, i);
                } else {
                    i += 1;
                    push!(Tok::Text("["), start, i);
                }
            }
            b'-' if src[i..].starts_with("--]") => {
                i += 3;
                push!(Tok::CommentClose, start, i);
            }
            b'-' | b'/' | b'*' | b'_' => {
                let style = match b[i] {
                    b'/' => TextStyle::Italic,
                    b'*' => TextStyle::Bold,
                    b'_' => TextStyle::Underline,
                    _ => TextStyle::Strikethrough,
                };
                if b.get(i + 1) == Some(&b[i]) {
                    i += 2;
                    push!(Tok::Mark(style), start, i);
                } else {
                    i += 1;
                    push!(Tok::Text(&src[start..i]), start, i);
                }
            }
            b'^' if b.get(i + 1) == Some(&b'^') => {
                i += 2;
                push!(Tok::SupMark, start, i);
            }
            b',' if b.get(i + 1) == Some(&b',') => {
                i += 2;
                push!(Tok::SubMark, start, i);
            }
            b'#' if b.get(i + 1) == Some(&b'#') => {
                // `##spec|` — the `|` must appear on this line (the old
                // `read_until` stopped at newlines); otherwise the `##` is a
                // closer (or plain text when it degrades).
                let mut j = i + 2;
                while j < b.len() && b[j] != b'|' && b[j] != b'\n' {
                    j += 1;
                }
                if b.get(j) == Some(&b'|') {
                    let spec = &src[i + 2..j];
                    i = j + 1;
                    push!(Tok::ColorOpen(spec), start, i);
                } else {
                    i += 2;
                    push!(Tok::ColorClose, start, i);
                }
            }
            b'%' if b.get(i + 1) == Some(&b'%') => {
                let mut j = i + 2;
                while j < b.len() && b[j] != b'\n' && !src[j..].starts_with("%%") {
                    j += src[j..].chars().next().map_or(1, char::len_utf8);
                }
                if src[j..].starts_with("%%") {
                    let raw = &src[i + 2..j];
                    let (name, default) = match raw.split_once('|') {
                        Some((n, d)) => (n, Some(d)),
                        None => (raw, None),
                    };
                    i = j + 2;
                    push!(Tok::ModuleVar { name, default }, start, i);
                } else {
                    i += 2;
                    push!(Tok::Text("%%"), start, i);
                }
            }
            b'{' if src[i..].starts_with("{$") => {
                let mut j = i + 2;
                while j < b.len() && b[j] != b'}' && b[j] != b'\n' {
                    j += 1;
                }
                if b.get(j) == Some(&b'}') {
                    let raw = &src[i + 2..j];
                    let (name, default) = match raw.split_once("//") {
                        Some((n, d)) => (n, Some(d)),
                        None => (raw, None),
                    };
                    i = j + 1;
                    push!(Tok::IncludeVar { name, default }, start, i);
                } else {
                    i += 2;
                    push!(Tok::Text("{$"), start, i);
                }
            }
            b'@' if b.get(i + 1) == Some(&b'@') => {
                let mut j = i + 2;
                while j < b.len() && b[j] != b'\n' && !src[j..].starts_with("@@") {
                    j += src[j..].chars().next().map_or(1, char::len_utf8);
                }
                let body = &src[i + 2..j];
                i = if src[j..].starts_with("@@") { j + 2 } else { j };
                push!(Tok::Escape(body), start, i);
            }
            b'h' if src[i..].starts_with("http://") || src[i..].starts_with("https://") => {
                let scheme_len = if src[i..].starts_with("https://") {
                    8
                } else {
                    7
                };
                let mut j = i + scheme_len;
                while src[j..].chars().next().is_some_and(is_url_char) {
                    j += src[j..].chars().next().map_or(1, char::len_utf8);
                }
                i = j;
                push!(Tok::Url(&src[start..i]), start, i);
            }
            _ => {
                i = text_run_end(src, i, at_ls);
                if i == start {
                    i += src[start..].chars().next().map_or(1, char::len_utf8);
                }
                push!(Tok::Text(&src[start..i]), start, i);
            }
        }
    }
    toks
}

/// A maximal run of characters that cannot begin a construct. `>` `+` `=`
/// only break the run at a line start (there they have line constructs); `h`
/// only when it starts a URL.
fn text_run_end(src: &str, from: usize, at_ls_start: bool) -> usize {
    let b = src.as_bytes();
    let mut j = from;
    let mut at_ls = at_ls_start;
    while j < b.len() {
        let stop = match b[j] {
            b'\n' | b'[' | b'@' | b'%' | b'{' | b'#' | b'/' | b'*' | b'_' | b'-' | b'^' | b','
            | b'|' => true,
            b'h' => src[j..].starts_with("http://") || src[j..].starts_with("https://"),
            b'>' | b'+' | b'=' => at_ls,
            _ => false,
        };
        if stop {
            break;
        }
        j += src[j..].chars().next().map_or(1, char::len_utf8);
        at_ls = false;
    }
    j
}

/// An indented (or bare) `*` / `#` list marker. Returns the end offset (past
/// the marker's trailing spaces / NBSPs), the ordering, and the indent width
/// in characters.
fn lex_list_mark(src: &str, i: usize) -> Option<(usize, bool, usize)> {
    let b = src.as_bytes();
    let mut j = i;
    let mut indent = 0usize;
    while b.get(j) == Some(&b' ') || src[j..].starts_with('\u{a0}') {
        indent += 1;
        j += src[j..].chars().next().map_or(1, char::len_utf8);
    }
    let ordered = match b.get(j) {
        Some(b'*') => false,
        Some(b'#') => true,
        _ => return None,
    };
    // `**bold**` / `##color##` are inline markup, not lists.
    if b.get(j + 1) == Some(&b[j]) {
        return None;
    }
    j += 1;
    while is_param_ws(src, j) {
        j += src[j..].chars().next().map_or(1, char::len_utf8);
    }
    Some((j, ordered, indent))
}

/// `[[[target|text]]]` — one token spanning to the first `]]]`.
fn lex_link3(src: &str, i: usize) -> Option<(usize, &str, Option<&str>)> {
    let b = src.as_bytes();
    let mut j = i + 3;
    while j < b.len() && b[j] != b'|' && b[j] != b'\n' && !b[j..].starts_with(b"]]]") {
        j += 1;
    }
    if b.get(j..).is_some_and(|r| r.starts_with(b"]]]")) {
        return Some((j + 3, &src[i + 3..j], None));
    }
    if b.get(j) != Some(&b'|') {
        return None;
    }
    let target = &src[i + 3..j];
    let mut k = j + 1;
    while k < b.len() && !b[k..].starts_with(b"]]]") {
        // The old `content_before` body stopped at any `[[/…]]` closer, and
        // the then-required `]]]` failed — so the whole link degrades.
        if b[k..].starts_with(b"[[/") {
            return None;
        }
        k += src[k..].chars().next().map_or(1, char::len_utf8);
    }
    if !b.get(k..).is_some_and(|r| r.starts_with(b"]]]")) {
        return None;
    }
    Some((k + 3, target, Some(&src[j + 1..k])))
}

/// `[url text]` / `[url]`, rejected when the `[` is followed by `[`, `!`,
/// `]` or a newline (the predecessor check lives in the caller).
fn lex_link1(src: &str, i: usize) -> Option<(usize, &str, Option<&str>)> {
    let b = src.as_bytes();
    match b.get(i + 1) {
        Some(b'[') | Some(b'!') | Some(b']') | Some(b'\n') | None => return None,
        _ => {}
    }
    let mut j = i + 1;
    while j < b.len() && b[j] != b' ' && b[j] != b']' && b[j] != b'\n' {
        j += 1;
    }
    let target = &src[i + 1..j];
    match b.get(j) {
        Some(b']') => Some((j + 1, target, None)),
        Some(b' ') => {
            let mut t_end = j;
            while t_end < b.len() && b[t_end] != b']' {
                t_end += 1;
            }
            let text = &src[j + 1..t_end];
            let end = if t_end < b.len() { t_end + 1 } else { t_end };
            Some((end, target, Some(text)))
        }
        _ => Some((j, target, None)),
    }
}

/// A known `[[…]]` construct at `i`: a closer, an opener, or a listpages
/// section marker. Keywords sit directly after `[[` (the only exceptions:
/// closers and section markers may carry inner spaces, and `[[ tab]]`).
fn lex_bracket<'src>(src: &'src str, i: usize) -> Option<(usize, Tok<'src>)> {
    let b = src.as_bytes();
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
        ("f<]]", true, AlignSide::Left),
        ("f>]]", true, AlignSide::Right),
        ("==]]", false, AlignSide::Justify),
        ("<]]", false, AlignSide::Left),
        (">]]", false, AlignSide::Right),
        ("=]]", false, AlignSide::Center),
    ] {
        if src[j..].starts_with(form) {
            return Some((j + form.len(), Tok::Open(OpenTag::Align { floating, side })));
        }
    }
    if let Some((end, tag)) = lex_image(src, j) {
        return Some((end, Tok::Open(tag)));
    }
    for (kw, tail) in OPENERS {
        if ci_starts(&b[j..], kw)
            && let Some((end, tag)) = tail(src, j + kw.len())
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
        && let Some((end, name)) = tab_name(src, k + 3)
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

fn div_tail(src: &str, j: usize) -> Option<(usize, OpenTag<'_>)> {
    let b = src.as_bytes();
    let underscore = b.get(j) == Some(&b'_');
    let k = if underscore { j + 1 } else { j };
    let mut params = Params::new();
    let k = lex_params(src, k, &mut params);
    let k = skip_spaces(b, k);
    b.get(k..)
        .is_some_and(|r| r.starts_with(b"]]"))
        .then(|| (k + 2, OpenTag::Div { underscore, params }))
}

fn span_tail(src: &str, j: usize) -> Option<(usize, OpenTag<'_>)> {
    container_tail(src, j, |params| OpenTag::Span { params })
}

fn table_tail(src: &str, j: usize) -> Option<(usize, OpenTag<'_>)> {
    container_tail(src, j, |params| OpenTag::Table { params })
}

fn row_tail(src: &str, j: usize) -> Option<(usize, OpenTag<'_>)> {
    container_tail(src, j, |params| OpenTag::Row { params })
}

fn anchor_tail(src: &str, j: usize) -> Option<(usize, OpenTag<'_>)> {
    container_tail(src, j, |params| OpenTag::Anchor { params })
}

/// `[[KW _? params spaces ]]` — the shape shared by span/table/row/a.
fn container_tail<'src, F: Fn(Params) -> OpenTag<'src>>(
    src: &'src str,
    j: usize,
    build: F,
) -> Option<(usize, OpenTag<'src>)> {
    let b = src.as_bytes();
    let k = if b.get(j) == Some(&b'_') { j + 1 } else { j };
    let mut params = Params::new();
    let k = lex_params(src, k, &mut params);
    let k = skip_spaces(b, k);
    b.get(k..)
        .is_some_and(|r| r.starts_with(b"]]"))
        .then(|| (k + 2, build(params)))
}

fn cell_tail(src: &str, j: usize) -> Option<(usize, OpenTag<'_>)> {
    cell_tail_with(src, j, false)
}

fn hcell_tail(src: &str, j: usize) -> Option<(usize, OpenTag<'_>)> {
    cell_tail_with(src, j, true)
}

fn cell_tail_with(src: &str, j: usize, header: bool) -> Option<(usize, OpenTag<'_>)> {
    let b = src.as_bytes();
    let mut params = Params::new();
    let k = lex_params(src, j, &mut params);
    let k = skip_spaces(b, k);
    b.get(k..)
        .is_some_and(|r| r.starts_with(b"]]"))
        .then(|| (k + 2, OpenTag::Cell { header, params }))
}

fn collapsible_tail(src: &str, j: usize) -> Option<(usize, OpenTag<'_>)> {
    let b = src.as_bytes();
    let mut params = Params::new();
    let k = lex_params(src, j, &mut params);
    let k = skip_spaces(b, k);
    b.get(k..)
        .is_some_and(|r| r.starts_with(b"]]"))
        .then(|| (k + 2, OpenTag::Collapsible { params }))
}

fn tabview_tail(src: &str, j: usize) -> Option<(usize, OpenTag<'_>)> {
    let b = src.as_bytes();
    let mut params = Params::new();
    let k = lex_params(src, j, &mut params);
    let k = skip_spaces(b, k);
    b.get(k..)
        .is_some_and(|r| r.starts_with(b"]]"))
        .then_some((k + 2, OpenTag::Tabview))
}

fn size_tail(src: &str, j: usize) -> Option<(usize, OpenTag<'_>)> {
    let b = src.as_bytes();
    if b.get(j) != Some(&b' ') {
        return None;
    }
    let mut k = j;
    while b.get(k) == Some(&b' ') {
        k += 1;
    }
    let end = read_to(src, k, b"]]")?;
    let arg = src[k..end].trim();
    b.get(end..)
        .is_some_and(|r| r.starts_with(b"]]"))
        .then(|| (end + 2, OpenTag::Size(arg)))
}

fn iftags_tail(src: &str, j: usize) -> Option<(usize, OpenTag<'_>)> {
    let b = src.as_bytes();
    if b.get(j) != Some(&b' ') {
        return None;
    }
    let mut k = j;
    while b.get(k) == Some(&b' ') {
        k += 1;
    }
    let end = read_to(src, k, b"]]")?;
    let filter = &src[k..end];
    b.get(end..)
        .is_some_and(|r| r.starts_with(b"]]"))
        .then(|| (end + 2, OpenTag::IfTags(filter)))
}

fn code_tail(src: &str, j: usize) -> Option<(usize, OpenTag<'_>)> {
    let b = src.as_bytes();
    let end = read_to(src, j, b"]]")?;
    b.get(end..)
        .is_some_and(|r| r.starts_with(b"]]"))
        .then_some((end + 2, OpenTag::Code))
}

fn head_tail(src: &str, j: usize) -> Option<(usize, OpenTag<'_>)> {
    marker_end(src.as_bytes(), j).map(|end| (end, OpenTag::Section(Some(SectionSlot::Head))))
}

fn body_tail(src: &str, j: usize) -> Option<(usize, OpenTag<'_>)> {
    marker_end(src.as_bytes(), j).map(|end| (end, OpenTag::Section(Some(SectionSlot::Body))))
}

fn foot_tail(src: &str, j: usize) -> Option<(usize, OpenTag<'_>)> {
    marker_end(src.as_bytes(), j).map(|end| (end, OpenTag::Section(Some(SectionSlot::Foot))))
}

/// `[[tab name]]` at top level (no leading spaces; the spaced form is a
/// special case in [`lex_bracket`]).
fn tab_top_tail(src: &str, j: usize) -> Option<(usize, OpenTag<'_>)> {
    tab_name(src, j).map(|(end, name)| (end, OpenTag::Tab { name }))
}

/// The `name]]` tail of a tab opener, spaces before the name included. The
/// name runs to `]]` crossing newlines (the old body stopped at `]]` only).
fn tab_name(src: &str, j: usize) -> Option<(usize, &str)> {
    let b = src.as_bytes();
    let mut k = j;
    while b.get(k) == Some(&b' ') {
        k += 1;
    }
    let mut end = k;
    while end < b.len() && !b[end..].starts_with(b"]]") {
        end += src[end..].chars().next().map_or(1, char::len_utf8);
    }
    b.get(end..)
        .is_some_and(|r| r.starts_with(b"]]"))
        .then_some((end + 2, &src[k..end]))
}

fn module_tail(src: &str, j: usize) -> Option<(usize, OpenTag<'_>)> {
    let b = src.as_bytes();
    if b.get(j) != Some(&b' ') {
        return None;
    }
    let mut k = j;
    while b.get(k) == Some(&b' ') {
        k += 1;
    }
    let lower = src[k..].to_ascii_lowercase();
    if lower.starts_with("css") {
        let end = read_to(src, k + 3, b"]]")?;
        return b
            .get(end..)
            .is_some_and(|r| r.starts_with(b"]]"))
            .then_some((end + 2, OpenTag::Css));
    }
    if lower.starts_with("listpages") {
        let mut m = k + 9;
        let mut params = Params::new();
        m = lex_params(src, m, &mut params);
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
    let name = src[k..k + n].to_string();
    let mut m = k + n;
    while m < b.len() && b[m] != b'\n' && !src[m..].starts_with("]]") {
        m += src[m..].chars().next().map_or(1, char::len_utf8);
    }
    if src[m..].starts_with("]]") {
        m += 2;
    }
    Some((m, OpenTag::Module(name)))
}

fn include_tail(src: &str, j: usize) -> Option<(usize, OpenTag<'_>)> {
    let b = src.as_bytes();
    if b.get(j) != Some(&b' ') {
        return None;
    }
    let mut k = j;
    while b.get(k) == Some(&b' ') {
        k += 1;
    }
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
                        raw: &src[k..k + p],
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

fn image_tail(src: &str, j: usize) -> Option<(usize, OpenTag<'_>)> {
    lex_image(src, j)
}

/// `[[<image source params]]` with an optional alignment prefix.
fn lex_image<'src>(src: &'src str, j: usize) -> Option<(usize, OpenTag<'src>)> {
    let b = src.as_bytes();
    let (align, mut m) = if src[j..].starts_with("f<") {
        (
            Some(Align {
                floating: true,
                side: AlignSide::Left,
            }),
            j + 2,
        )
    } else if src[j..].starts_with("f>") {
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
    while b.get(m) == Some(&b' ') {
        m += 1;
    }
    let (after_source, source) = collect_text_objs(src, m, &[" ", "]]"], &[]);
    let mut params = Params::new();
    let end = lex_params(src, after_source, &mut params);
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
fn lex_params(src: &str, mut k: usize, out: &mut Params) -> usize {
    let b = src.as_bytes();
    loop {
        while k < b.len() && is_param_ws(src, k) {
            k += src[k..].chars().next().map_or(1, char::len_utf8);
        }
        match b.get(k) {
            None | Some(b']') | Some(b'\n') => return k,
            _ => {}
        }
        let key_start = k;
        while src[k..].chars().next().is_some_and(is_prop_char) {
            k += src[k..].chars().next().map_or(1, char::len_utf8);
        }
        if k == key_start {
            return k;
        }
        let key = src[key_start..k].to_ascii_lowercase();
        if b.get(k) != Some(&b'=') {
            return k + src[k..].chars().next().map_or(0, char::len_utf8);
        }
        k += 1;
        let value = if b.get(k) == Some(&b'"') {
            k += 1;
            let (nk, v) = collect_text_objs(src, k, &[], &['"']);
            k = nk;
            if b.get(k) == Some(&b'"') {
                k += 1;
            }
            v
        } else {
            let (nk, v) = collect_text_objs(src, k, &[], &[' ', '\u{a0}', ']']);
            k = nk;
            v
        };
        out.insert(key, value);
    }
}

/// A run of [`TextObj`]s — plain text interleaved with `%%var%%` and
/// `{$var}` substitutions — up to any of `delims`, a `single_stop` char, a
/// newline, or EOF. Ports the old `collect_text_objs` including its
/// variable-reading quirks (a default may run past a newline; an include
/// default is sub-parsed markup).
fn collect_text_objs(
    src: &str,
    mut k: usize,
    delims: &[&str],
    single_stops: &[char],
) -> (usize, Vec<TextObj>) {
    let b = src.as_bytes();
    let mut result = Vec::new();
    let mut buf = String::new();
    loop {
        if k >= b.len() || b[k] == b'\n' || delims.iter().any(|d| src[k..].starts_with(d)) {
            break;
        }
        if let Some(c) = src[k..].chars().next()
            && single_stops.contains(&c)
        {
            break;
        }
        if src[k..].starts_with("%%") {
            if !buf.is_empty() {
                result.push(TextObj::Plain(std::mem::take(&mut buf)));
            }
            k += 2;
            let name_start = k;
            while src[k..].chars().next().is_some_and(is_prop_char) {
                k += src[k..].chars().next().map_or(1, char::len_utf8);
            }
            let name = src[name_start..k].to_string();
            let default = if b.get(k) == Some(&b'|') {
                k += 1;
                let d_start = k;
                while k < b.len() && !src[k..].starts_with("%%") {
                    k += src[k..].chars().next().map_or(1, char::len_utf8);
                }
                let d = src[d_start..k].to_string();
                if src[k..].starts_with("%%") {
                    k += 2;
                }
                Some(d)
            } else {
                if src[k..].starts_with("%%") {
                    k += 2;
                }
                None
            };
            result.push(TextObj::ModuleVar { name, default });
            continue;
        }
        if src[k..].starts_with("{$") {
            if !buf.is_empty() {
                result.push(TextObj::Plain(std::mem::take(&mut buf)));
            }
            k += 2;
            let name_start = k;
            while src[k..].chars().next().is_some_and(is_prop_char) {
                k += src[k..].chars().next().map_or(1, char::len_utf8);
            }
            let name = src[name_start..k].to_string();
            let default = if src[k..].starts_with("//") {
                k += 2;
                let d_start = k;
                while k < b.len() && b[k] != b'}' {
                    k += 1;
                }
                let d = src[d_start..k].to_string();
                if b.get(k) == Some(&b'}') {
                    k += 1;
                }
                Some(parse(&d))
            } else {
                if b.get(k) == Some(&b'}') {
                    k += 1;
                }
                None
            };
            result.push(TextObj::IncludeVar { name, default });
            continue;
        }
        let c = src[k..].chars().next().unwrap();
        buf.push(c);
        k += c.len_utf8();
    }
    if !buf.is_empty() {
        result.push(TextObj::Plain(buf));
    }
    (k, result)
}
