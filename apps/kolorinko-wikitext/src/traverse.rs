//! Structural traversal over the node tree — the shared one-level descent
//! that every tree walk in the pipeline (substitution, collection,
//! evaluation) is built on.
//!
//! [`Node::map_node`] / [`Node::visit_node`] apply `f` to each [`Content`]
//! nested *directly* inside one node — every item body of a nested `List`
//! and each slot of a `ListPages` module included — and nothing else: link
//! targets, attributes, image sources and include variables pass through
//! untouched, and `f`'s results are not re-visited. A deep transform
//! therefore follows the pattern
//!
//! ```text
//! fn walk(content: Content) -> Content {
//!     content.into_iter().map(|node| match node {
//!         Node::Special(..) => /* this level's concern */,
//!         other => other.map_node(&mut walk),
//!     }).collect()
//! }
//! ```
//!
//! — `map_node` supplies the structural recursion into one node's children,
//! `walk` itself the recursion into their results.

use super::{BlockCell, BlockRow, BlockTable, Content, List, ListItem, Node, Tab, TableCell};

impl Node {
    /// Rebuild the node with every nested [`Content`] replaced by `f`'s
    /// output.
    pub fn map_node(self, f: &mut impl FnMut(Content) -> Content) -> Node {
        match self {
            Node::Container { kind, content } => Node::Container {
                kind,
                content: f(content),
            },
            Node::Heading {
                level,
                anchor,
                content,
            } => Node::Heading {
                level,
                anchor,
                content: f(content),
            },
            Node::Table(rows) => Node::Table(
                rows.into_iter()
                    .map(|row| {
                        row.into_iter()
                            .map(|cell| TableCell {
                                content: f(cell.content),
                                ..cell
                            })
                            .collect()
                    })
                    .collect(),
            ),
            Node::BlockTable(t) => Node::BlockTable(BlockTable {
                rows: t
                    .rows
                    .into_iter()
                    .map(|r| BlockRow {
                        content: f(r.content),
                        ..r
                    })
                    .collect(),
                ..t
            }),
            Node::BlockCell(c) => Node::BlockCell(BlockCell {
                content: f(c.content),
                ..c
            }),
            Node::SupSubscript { sup, sub } => Node::SupSubscript {
                sup: f(sup),
                sub: f(sub),
            },
            Node::Link {
                target,
                text,
                class,
            } => Node::Link {
                target,
                text: f(text),
                class,
            },
            Node::IfExpr { cond, then, els } => Node::IfExpr {
                cond,
                then: f(then),
                els: f(els),
            },
            Node::Collapsible {
                folded,
                show,
                hide,
                content,
            } => Node::Collapsible {
                folded,
                show,
                hide,
                content: f(content),
            },
            Node::ModuleBlock { name, params, body } => Node::ModuleBlock {
                name,
                params,
                body: f(body),
            },
            Node::Footnote(c) => Node::Footnote(f(c)),
            Node::FootnoteBlock(bodies) => Node::FootnoteBlock(bodies.into_iter().map(f).collect()),
            Node::Tabview { id, tabs } => Node::Tabview {
                id,
                tabs: tabs
                    .into_iter()
                    .map(|t| Tab {
                        name: f(t.name),
                        content: f(t.content),
                    })
                    .collect(),
            },
            Node::ListPages(mut lp) => {
                lp.prepend = f(lp.prepend);
                lp.repeat = f(lp.repeat);
                lp.append = f(lp.append);
                Node::ListPages(lp)
            }
            Node::List(list) => Node::List(map_list(list, f)),
            other => other,
        }
    }

    /// Call `f` for every [`Content`] nested inside the node.
    pub fn visit_node(&self, f: &mut impl FnMut(&Content)) {
        match self {
            Node::Container { content, .. } | Node::Heading { content, .. } => f(content),
            Node::Table(rows) => rows.iter().for_each(|row| {
                row.iter().for_each(|cell| f(&cell.content));
            }),
            Node::BlockTable(t) => t.rows.iter().for_each(|r| f(&r.content)),
            Node::BlockCell(c) => f(&c.content),
            Node::SupSubscript { sup, sub } => {
                f(sup);
                f(sub);
            }
            Node::IfExpr { then: content, .. }
            | Node::Collapsible { content, .. }
            | Node::ModuleBlock { body: content, .. }
            | Node::Footnote(content)
            | Node::Link { text: content, .. } => f(content),
            Node::IfExpr { els, .. } => f(els),
            Node::FootnoteBlock(bodies) => bodies.iter().for_each(f),
            Node::Tabview { tabs, .. } => tabs.iter().for_each(|t| {
                f(&t.name);
                f(&t.content);
            }),
            Node::ListPages(lp) => {
                f(&lp.prepend);
                f(&lp.repeat);
                f(&lp.append);
            }
            Node::List(list) => visit_list(list, f),
            _ => {}
        }
    }
}

fn map_list(list: List, f: &mut impl FnMut(Content) -> Content) -> List {
    List {
        ordered: list.ordered,
        items: list
            .items
            .into_iter()
            .map(|item| ListItem {
                content: f(item.content),
                sublist: item.sublist.map(|sub| Box::new(map_list(*sub, f))),
            })
            .collect(),
    }
}

fn visit_list(list: &List, f: &mut impl FnMut(&Content)) {
    for item in &list.items {
        f(&item.content);
        if let Some(sub) = &item.sublist {
            visit_list(sub, f);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Content, Node, Tab, TextObj};

    fn text(s: &str) -> Content {
        vec![Node::Text(TextObj::Plain(s.into()))]
    }

    #[test]
    fn map_descends_every_nested_content() {
        let node = Node::Tabview {
            id: 0,
            tabs: vec![Tab {
                name: text("a"),
                content: text("b"),
            }],
        };
        let out = node.map_node(&mut |c| {
            c.into_iter()
                .map(|n| match n {
                    Node::Text(TextObj::Plain(s)) => Node::Text(TextObj::Plain(format!("<{s}>"))),
                    other => other,
                })
                .collect()
        });
        let Node::Tabview { tabs, .. } = out else {
            panic!("expected tabview")
        };
        let mut flat = String::new();
        Node::Tabview { id: 0, tabs }.visit_node(&mut |c| {
            for n in c {
                if let Node::Text(TextObj::Plain(s)) = n {
                    flat.push_str(s);
                }
            }
        });
        assert_eq!(flat, "<a><b>");
    }

    #[test]
    fn map_leaves_leaves_untouched() {
        let raw = Node::Raw("verbatim".into());
        let Node::Raw(s) = raw.map_node(&mut |_| vec![]) else {
            panic!("expected raw")
        };
        assert_eq!(s, "verbatim");
    }
}
