//! Wikidot markup parser: a chumsky lexer over raw bytes plus a plain merge
//! pass.
//!
//! The grammar follows the reference at
//! <https://www.wikidot.com/doc-wiki-syntax:inline-formatting> and is a port of
//! the original PureScript modules `Pagx.Hered.Tipoj` / `Pagx.Hered.Analiz`.
//!
//! ## Architecture
//!
//! [`lex`] turns the page into a flat, total [`Token`] stream — rich tokens
//! carrying pre-split openers/closers, parsed attributes and byte spans — and
//! [`merge::Merger`] pairs them into [`Node`]s in one non-backtracking walk.
//! Every container (`[[div]]`, `[[span]]`, `[!--`, …) merges its body until
//! its own closer; on a mismatch or EOF it *flattens*: the verbatim opener
//! becomes a [`Node::Raw`] followed by the body it already merged, and the
//! foreign stop propagates to the ancestor that owns the consumed closer.
//! Malformed input therefore degrades gracefully and merging stays linear.
//!
//! ## Graceful degradation
//!
//! Like the original, the parser is total: any input parses to *something*.
//! Unknown `[[…]]` constructs and stray sigils fall through to text tokens,
//! and a final [`merge_text`] pass fuses adjacent fragments (so an unknown
//! `[[toc]]` reassembles into a single text node rather than seven).

pub mod types;

pub(crate) use crate::wikidot_parser::types::*;
pub(crate) use std::collections::HashMap;

mod helpers;
pub(crate) mod lexer;
pub(crate) mod merge;

pub(crate) use helpers::*;

// =========================================================================
// Closing tags
// =========================================================================

/// Every opening tag that pairs with a matching `[[/…]]` closer.
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
    Table,
    Row,
    /// `[[cell …]]` / `[[hcell …]]` — both close with `[[/cell]]` or
    /// `[[/hcell]]`, so the closer keyword maps to this single variant.
    Cell,
    Collapsible,
    /// `[[a href=…]] … [[/a]]` explicit anchor.
    Anchor,
    /// `[[code]] … [[/code]]`.
    Code,
    /// `[[<]]`, `[[=]]`, `[[>]]`, `[[==]]`, `[[f<]]`, `[[f>]]`. The closer
    /// mirrors the opener exactly (`[[/f<]]`, `[[/==]]`, …).
    Align {
        floating: bool,
        side: AlignSide,
    },
}

impl ClosedTag {
    /// The keyword/sequence after `[[` (and after `[[/` in the closer), used
    /// to reconstruct a stray closer as raw text.
    pub(crate) fn opener_str(&self) -> String {
        match self {
            ClosedTag::Div => "div".into(),
            ClosedTag::Span => "span".into(),
            ClosedTag::Size => "size".into(),
            ClosedTag::IfTags => "iftags".into(),
            ClosedTag::Module => "module".into(),
            ClosedTag::Tab => "tab".into(),
            ClosedTag::Tabview => "tabview".into(),
            ClosedTag::Table => "table".into(),
            ClosedTag::Row => "row".into(),
            ClosedTag::Cell => "cell".into(),
            ClosedTag::Collapsible => "collapsible".into(),
            ClosedTag::Anchor => "a".into(),
            ClosedTag::Code => "code".into(),
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

// =========================================================================
// Public entry points
// =========================================================================

/// Parse a whole page into [`Content`], fusing adjacent text fragments with
/// [`merge_text`]. Total: any input parses to something.
pub fn parse(input: &str) -> Content {
    let toks = lexer::lex(input);
    merge::parse_toks(input, &toks)
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

    /// All values bound to a given key, in source order.
    fn var_entries<'a>(vars: &'a [(String, Content)], key: &str) -> Vec<&'a Content> {
        vars.iter()
            .filter(|(k, _)| k == key)
            .map(|(_, v)| v)
            .collect()
    }

    /// Extract the single plain-text value of an include variable's first
    /// binding.
    fn var_str(vars: &[(String, Content)], key: &str) -> Option<String> {
        var_entries(vars, key)
            .into_iter()
            .next()
            .and_then(|v| match v.as_slice() {
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
        let Node::Link { target, text } = &c[1] else {
            panic!("expected bare-url link, got {:?}", c[1]);
        };
        // The scheme is preserved (not stripped), keeping the URL absolute for
        // correct rendering and content-addressed resolution.
        assert!(matches!(target, LinkTarget::Url(u) if u == "https://example.com/x"));
        assert!(matches!(
            text.as_slice(),
            [Node::Text(TextObj::Plain(t))] if t == "https://example.com/x"
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
        // Pipe syntax preserves duplicate keys in source order, so the
        // `k={$k}|k=default` idiom keeps both the passthrough reference and the
        // literal default; the first non-empty value wins at substitution.
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
        // `align`: the passthrough reference first, then the literal default.
        let align = var_entries(vars, "align");
        assert_eq!(align.len(), 2);
        assert!(matches!(
            align[0].as_slice(),
            [Node::Text(TextObj::IncludeVar { name, .. })] if name == "align"
        ));
        assert!(matches!(
            align[1].as_slice(),
            [Node::Text(TextObj::Plain(s))] if s == "right"
        ));
        assert_eq!(var_entries(vars, "votes").len(), 2);
        // `stars` has only the passthrough (no default given).
        let stars = var_entries(vars, "stars");
        assert_eq!(stars.len(), 1);
        assert!(matches!(
            stars[0].as_slice(),
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
        let tb = var_entries(vars, "translationblock")
            .into_iter()
            .next()
            .expect("translationblock var");
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
        // And the keyword match itself: `…[[include foo]]` should still recognize
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

    /// A deeply nested *unclosed* `[[div]]` used to catastrophic-backtrack and
    /// hang. It must now complete, flattening each broken opener to a raw node.
    #[test]
    fn nested_unclosed_div_completes() {
        let src = "[[div]]\n".repeat(500) + "body";
        let c = parse(&src);
        // Every broken opener flattens to a `Node::Raw`; none are swallowed or
        // collapsed into a single text blob.
        let raws = c.iter().filter(|n| matches!(n, Node::Raw(_))).count();
        assert_eq!(raws, 500, "expected 500 raw openers, got {c:#?}");
        // The trailing body text survives and is not eaten by the bug.
        assert!(
            c.iter()
                .any(|n| matches!(n, Node::Text(TextObj::Plain(s)) if s.contains("body")))
        );
    }

    /// Nested *unclosed* comments (`[!--`) must not eat the rest of the page and
    /// must complete (the old `read_until_lines` closer backtracked mildly
    /// superlinearly; the content-loop route must stay bounded).
    #[test]
    fn nested_unclosed_comment_does_not_eat_page() {
        let src = "[!--\n".repeat(50) + "after\n";
        let c = parse(&src);
        // `after` must remain as parseable text, not be discarded.
        assert!(
            c.iter()
                .any(|n| matches!(n, Node::Text(TextObj::Plain(s)) if s.contains("after"))),
            "page tail eaten: {c:#?}"
        );
        // Each broken `[!--` flattens to a raw opener.
        let raws = c.iter().filter(|n| matches!(n, Node::Raw(_))).count();
        assert_eq!(raws, 50, "expected 50 raw `[!--` openers, got {c:#?}");
    }

    /// A balanced comment around markup discards its contents entirely.
    #[test]
    fn balanced_comment_discards() {
        let c = parse("before [!-- [[div]] hidden [[/div]] --] after");
        // Nothing of the commented-out div should leak.
        assert!(
            c.iter().all(|n| !matches!(
                n,
                Node::Container {
                    kind: ContainerKind::Div { .. },
                    ..
                }
            )),
            "commented div leaked: {c:#?}"
        );
        assert!(
            c.iter()
                .any(|n| matches!(n, Node::Text(TextObj::Plain(s)) if s.contains("before")))
        );
        assert!(
            c.iter()
                .any(|n| matches!(n, Node::Text(TextObj::Plain(s)) if s.contains("after")))
        );
    }

    /// A foreign closer (`[[/span]]`) must be claimed by its matching open
    /// container, with the intervening mismatched container (`[[div]]`)
    /// flattened inside it — not left to leak as stray text.
    #[test]
    fn mismatched_closer_claimed_by_ancestor() {
        let c = parse("[[span]] outer [[div]] inner [[/span]] tail");
        // The span matches and wraps the broken div.
        let span = c
            .iter()
            .find_map(|n| match n {
                Node::Container {
                    kind: ContainerKind::Div { inline: true, .. },
                    content,
                } => Some(content),
                _ => None,
            })
            .expect("matched span container");
        assert!(
            span.iter()
                .any(|n| matches!(n, Node::Raw(s) if s == "[[div]]")),
            "div should flatten inside span: {span:#?}"
        );
        assert!(
            span.iter()
                .any(|n| matches!(n, Node::Text(TextObj::Plain(s)) if s.contains("inner")))
        );
        // The `[[/span]]` closer is consumed by the span, so `tail` is free text.
        assert!(
            c.iter()
                .any(|n| matches!(n, Node::Text(TextObj::Plain(s)) if s.contains("tail")))
        );
        // No stray raw closer leaks.
        assert!(
            c.iter()
                .all(|n| !matches!(n, Node::Raw(s) if s.contains("/span")))
        );
    }

    /// A top-level stray closer (`[[/div]]` with no opener) is absorbed as raw
    /// text and parsing resumes.
    #[test]
    fn stray_closer_absorbed_as_raw() {
        let c = parse("text [[/div]] more");
        assert!(
            c.iter()
                .any(|n| matches!(n, Node::Raw(s) if s.contains("/div")))
        );
        assert!(
            c.iter()
                .any(|n| matches!(n, Node::Text(TextObj::Plain(s)) if s.contains("more")))
        );
    }

    /// Module attribute names are case-insensitive: the documented `perPage`
    /// form must reach the same `per_page` parameter as `perpage`.
    #[test]
    fn listpages_perpage_is_case_insensitive() {
        let c = parse(
            "[[module ListPages category=\"rumor-a\" perPage=\"250\" separate=\"no\"]]\\n[[/module]]",
        );
        let Node::ListPages(lp) = &c[0] else {
            panic!("expected ListPages: {c:#?}")
        };
        assert_eq!(lp.params.category.as_deref(), Some("rumor-a"));
        assert_eq!(lp.params.per_page, Some(250));
        assert!(!lp.params.separate);
    }
}
