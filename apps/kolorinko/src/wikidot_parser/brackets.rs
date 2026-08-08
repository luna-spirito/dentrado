//! Bracketed `[[…]]` constructs: containers, modules, includes, images.

use super::*;

/// `[[a href="url" …]] body [[/a]]` — an explicit anchor. The `href` attribute
/// is used verbatim (no site-prefixing); the body is inline wikitext.
pub(crate) fn anchor_block<'a, P: Parser<'a, In<'a>, Node, E<'a>> + Clone + 'a>(
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
pub(crate) fn bracket_syntax<'a, P: Parser<'a, In<'a>, Node, E<'a>> + Clone + 'a>(
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
pub(crate) fn link<'a, P: Parser<'a, In<'a>, Node, E<'a>> + Clone + 'a>(
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
pub(crate) fn div_span_block<'a, P: Parser<'a, In<'a>, Node, E<'a>> + Clone + 'a>(
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
pub(crate) fn div_open<'a>()
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
pub(crate) fn grid_table_block<'a, P: Parser<'a, In<'a>, Node, E<'a>> + Clone + 'a>(
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
pub(crate) fn grid_cell_block<'a, P: Parser<'a, In<'a>, Node, E<'a>> + Clone + 'a>(
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
pub(crate) fn tag_close<'a>(kw: &'static str) -> impl Parser<'a, In<'a>, (), E<'a>> + Clone + 'a {
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
pub(crate) fn container_open<'a>(
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
pub(crate) fn align_block<'a, P: Parser<'a, In<'a>, Node, E<'a>> + Clone + 'a>(
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

pub(crate) fn align_case<'a, P: Parser<'a, In<'a>, Node, E<'a>> + Clone + 'a>(
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
pub(crate) fn size_block<'a, P: Parser<'a, In<'a>, Node, E<'a>> + Clone + 'a>(
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
pub(crate) fn iftags_block<'a, P: Parser<'a, In<'a>, Node, E<'a>> + Clone + 'a>(
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
pub(crate) fn collapsible_block<'a, P: Parser<'a, In<'a>, Node, E<'a>> + Clone + 'a>(
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
pub(crate) fn code_block<'a>() -> impl Parser<'a, In<'a>, Node, E<'a>> + Clone + 'a {
    kw_ci("code".into())
        .ignore_then(read_until(&["]]"]).ignored())
        .then_ignore(just("]]"))
        .ignore_then(read_until_lines(&["[[/code"]).map(|s| s.to_string()))
        .then_ignore(choice((just("[[/code]]").ignored(), end())))
        .map(|s| Node::Code(s.trim().to_string()))
}

/// `[[module NAME …]] … [[/module]]`. Dispatches `css` (raw stylesheet) and
/// `ListPages` (template); other modules fall through to raw text.
pub(crate) fn module_block<'a, P: Parser<'a, In<'a>, Node, E<'a>> + Clone + 'a>(
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
pub(crate) fn module_name<'a>() -> impl Parser<'a, In<'a>, String, E<'a>> + Clone + 'a {
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
pub(crate) fn listpages_body<'a, P: Parser<'a, In<'a>, Node, E<'a>> + Clone + 'a>(
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
pub(crate) fn tabview_block<'a, P: Parser<'a, In<'a>, Node, E<'a>> + Clone + 'a>(
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
pub(crate) fn include_block<'a>() -> impl Parser<'a, In<'a>, Node, E<'a>> + Clone + 'a {
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
pub(crate) fn read_include_body<'a>() -> impl Parser<'a, In<'a>, &'a str, E<'a>> + Clone + 'a {
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
pub(crate) fn parse_include_args(raw: &str) -> (PageRef, HashMap<String, Content>) {
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
pub(crate) fn unquote(value: &str) -> &str {
    let t = value.trim();
    if t.len() >= 2 && t.starts_with('"') && t.ends_with('"') {
        &t[1..t.len() - 1]
    } else {
        t
    }
}

/// Record a `key=value` segment: split on the first `=`, parse the value as
/// wikitext markup. Quoted values are unwrapped first.
pub(crate) fn insert_kv(seg: &str, vars: &mut HashMap<String, Content>) {
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
pub(crate) fn has_depth0_pipe(s: &str) -> bool {
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

pub(crate) fn parse_pipe_vars(remainder: &str) -> HashMap<String, Content> {
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

pub(crate) fn parse_space_vars(remainder: &str) -> HashMap<String, Content> {
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
pub(crate) fn image_block<'a>() -> impl Parser<'a, In<'a>, Node, E<'a>> + Clone + 'a {
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
