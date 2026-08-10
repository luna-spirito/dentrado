//! Line-start block constructs: headings, rules, tables, quotes, lists.

use super::*;

/// All constructs that may only appear at the beginning of a line.
pub(crate) fn line_syntax<
    'a,
    P: Parser<'a, In<'a>, (Content, Option<ContentExitReason>), E<'a>> + Clone + 'a,
>(
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
pub(crate) fn heading<
    'a,
    P: Parser<'a, In<'a>, (Content, Option<ContentExitReason>), E<'a>> + Clone + 'a,
>(
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
pub(crate) fn hr<'a>() -> impl Parser<'a, In<'a>, Node, E<'a>> + Clone + 'a {
    just("----")
        .ignore_then(just('-').repeated().ignored())
        .then_ignore(line_end())
        .to(Node::HorizontalRule)
}

/// A `||…||…` table: one or more consecutive `||`-prefixed lines. Cells are
/// separated by `||`; each cell may begin with `~` (header) and an alignment
/// marker (`<` / `=` / `>`).
pub(crate) fn table_block<
    'a,
    P: Parser<'a, In<'a>, (Content, Option<ContentExitReason>), E<'a>> + Clone + 'a,
>(
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
pub(crate) fn blockquote<
    'a,
    P: Parser<'a, In<'a>, (Content, Option<ContentExitReason>), E<'a>> + Clone + 'a,
>(
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
pub(crate) fn centered_line<
    'a,
    P: Parser<'a, In<'a>, (Content, Option<ContentExitReason>), E<'a>> + Clone + 'a,
>(
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
pub(crate) fn list_block<
    'a,
    P: Parser<'a, In<'a>, (Content, Option<ContentExitReason>), E<'a>> + Clone + 'a,
>(
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
pub(crate) fn build_list(lines: &[(usize, bool, Content)]) -> List {
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
