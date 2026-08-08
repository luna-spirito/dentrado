//! Link / page-ref / tag-filter helpers, colour normalisation, and the post-processing pass that fuses adjacent text fragments.

use super::*;

/// Turn a raw link target string into a [`LinkTarget`]: external URL if it
/// starts with `http://`/`https://`, otherwise an internal wiki page reference.
pub(crate) fn parse_link_target(raw: &str) -> LinkTarget {
    let trimmed = raw.trim();
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        return LinkTarget::Url(trimmed.to_string());
    }
    LinkTarget::Page(parse_page_ref(trimmed))
}

/// Parse a `[[include]]` source or internal link path into a [`PageRef`].
///
/// A leading `space:` segment is a cross-space reference; the rest is the path.
pub(crate) fn parse_page_ref(raw: &str) -> PageRef {
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
pub(crate) fn parse_tag_filter(raw: &str) -> (Vec<String>, Vec<String>) {
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
pub(crate) fn normalize_color(c: String) -> String {
    if [3, 4, 6, 8].contains(&c.len()) && c.chars().all(is_hex_char) {
        format!("#{c}")
    } else {
        c
    }
}

/// Recursively merge adjacent [`Node::Text(Plain(_))`] nodes so the fallback
/// single-char path doesn't fragment output (e.g. `[[toc]]` → one text node).
pub(crate) fn merge_text(content: Content) -> Content {
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
pub(crate) fn map_node_content<F: Fn(Content) -> Content>(node: Node, f: F) -> Node {
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
