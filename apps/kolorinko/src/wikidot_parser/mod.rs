//! Wikidot markup parser: a chumsky lexer over raw bytes, a pairing pass
//! over the token stream, and a facts-guided tree builder.
//!
//! The grammar follows the reference at
//! <https://www.wikidot.com/doc-wiki-syntax:inline-formatting> and is a port of
//! the original PureScript modules `Pagx.Hered.Tipoj` / `Pagx.Hered.Analiz`.
//!
//! ## Architecture
//!
//! [`lex`] turns the page into a flat, total [`Token`] stream — rich tokens
//! carrying pre-split openers/closers, parsed attributes and byte spans.
//! [`pairer::pair`] then answers exactly one question per token — which
//! opener owns which closer — reporting intervals (and crossings) as plain
//! facts. [`builder`] folds those facts into [`Node`]s under one structural
//! rule: block frames sit at the bottom of the stack, inline frames on top,
//! and every block boundary splits the inline frames above it — which is
//! exactly Wikidot's interval semantics. Wrapping constructs live on the
//! builder's explicit frame stack (never the Rust call stack, so hostile
//! nesting is heap-bounded). Malformed input degrades gracefully: a stray
//! closer or an opener whose closer never came renders as [`Node::Raw`],
//! and a strikethrough nobody claimed is an em-dash. Both passes stay
//! linear.
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

/// The facts-guided tree builder: folds [`pairer`] intervals into nodes.
pub(crate) mod builder;
pub mod helpers;
pub(crate) mod lexer;
/// The pre-redesign single-pass merger; kept as the diff oracle until the
/// corpus finalises the open tables, then to be dissolved.
#[allow(dead_code)]
pub(crate) mod merge;
/// The pairing pass: opener↔closer intervals as plain facts.
pub(crate) mod pairer;

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
    /// `[[footnote]] … [[/footnote]]`.
    Footnote,
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
            ClosedTag::Footnote => "footnote".into(),
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
    builder::parse_toks(input, &toks)
}

#[cfg(test)]
mod tests {
    use super::*;
    /// Crossed containers split, not flatten: the span closes inside the
    /// div where `[[/div]]` cut it, then re-opens for the tail — Wikidot's
    /// `<div>hi1 <span>hi2</span></div><span>hi3</span>`.
    #[test]
    fn crossed_span_splits_across_div() {
        let c = parse("[[div]]\nhi1\n[[span]]\nhi2\n[[/div]]\nhi3\n[[/span]]\n");
        let span = |content: Content| Node::Container {
            kind: ContainerKind::Div {
                inline: true,
                block: false,
                params: Params::new(),
            },
            content,
        };
        assert_eq!(
            c,
            vec![
                Node::Container {
                    kind: ContainerKind::Div {
                        inline: false,
                        block: true,
                        params: Params::new(),
                    },
                    content: vec![txt("\nhi1\n"), span(vec![txt("\nhi2\n")])],
                },
                span(vec![txt("\nhi3\n")]),
                txt("\n"),
            ]
        );
    }

    /// A `[[module css]]` body runs Wikidot's verbatim-stylesheet pipeline
    /// ([`helpers::wikidot_verbatim`]): `&amp;` → `&amp;amp;` (a bare `&`
    /// stays as written), edges trimmed, one trailing newline.
    #[test]
    fn stylesheet_body_runs_the_verbatim_pipeline() {
        let c = parse("[[module css]]\n\na { content: \"A &amp; B\" }\nb&c\n\n[[/module]]");
        assert_eq!(
            c,
            vec![Node::Stylesheet(
                "a { content: \"A &amp;amp; B\" }\nb&c\n".into()
            )]
        );
    }

    /// Wikidot's Module rule is `^`-anchored and runs before the blockquote
    /// rule strips `> `, so a quote-prefixed CSS region never opens: it stays
    /// literal documentation code (with the URL still linking), unlike the
    /// same region at a line start.
    #[test]
    fn quoted_css_region_stays_literal() {
        let c = parse("> [[module css]]\n> @import url(https://x.example/t/1);\n> [[/module]]");
        let dump = format!("{c:?}");
        assert!(!dump.contains("Stylesheet"));
        assert!(dump.contains("Raw(\"[[module css]]\")"));
        assert!(dump.contains("Raw(\"[[/module]]\")"));
    }

    /// Marks split the same way: `**hi--hello**hey--` closes the strike
    /// inside the bold, then re-opens it for the tail.
    #[test]
    fn crossed_strike_splits_across_bold() {
        let c = parse("**hi--hello**hey--");
        let style = |style: TextStyle, content: Content| Node::Container {
            kind: ContainerKind::Style(style),
            content,
        };
        assert_eq!(
            c,
            vec![
                style(
                    TextStyle::Bold,
                    vec![
                        txt("hi"),
                        style(TextStyle::Strikethrough, vec![txt("hello")])
                    ]
                ),
                style(TextStyle::Strikethrough, vec![txt("hey")]),
            ]
        );
    }

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
            Node::Link { target, text, .. } => {
                assert!(matches!(target, LinkTarget::Page(_)));
                assert_eq!(text.len(), 1);
            }
            other => panic!("expected link, got {other:?}"),
        }
    }

    #[test]
    fn bare_url() {
        let c = parse("see https://example.com/x for info");
        let Node::Link { target, text, .. } = &c[1] else {
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
    fn anchor_href_bare_and_rooted_paths_are_pages() {
        // `[[a href="…"]]` is classified like any link: bare/rooted paths
        // become internal page references.
        for href in ["terrible-trio-event", "/terrible-trio-event"] {
            let c = parse(&format!("[[a href=\"{href}\"]]x[[/a]]"));
            let Node::Link { target, .. } = &c[0] else {
                panic!("expected link, got {:?}", c[0])
            };
            let LinkTarget::Page(p) = target else {
                panic!("expected page target, got {target:?}")
            };
            assert_eq!(p.space, None);
            assert_eq!(p.path, ["terrible-trio-event"]);
        }
    }

    #[test]
    fn anchor_href_url_and_fragment_stay_urls() {
        let c =
            parse("[[a href=\"https://x.io/y\"]]y[[/a]] [[a href=\"#toc\"]]top[[/a]] [[a]]x[[/a]]");
        for (i, want) in [(0, "https://x.io/y"), (2, "#toc"), (4, "#")] {
            let Node::Link { target, .. } = &c[i] else {
                panic!("expected link at {i}, got {:?}", c[i])
            };
            assert!(
                matches!(target, LinkTarget::Url(u) if u == want),
                "link {i}: {target:?}"
            );
        }
    }

    #[test]
    fn anchor_href_with_include_var_stays_unresolved() {
        // The variable slot must survive parsing inside the href; it stays
        // unresolved until a ListPages/substitution pass binds it.
        let c = parse("[[a href=\"https://www.obscurative.ru/{$page}\"]]x[[/a]]");
        let Node::Link { target, .. } = &c[0] else {
            panic!("expected link, got {:?}", c[0])
        };
        let LinkTarget::Unresolved(objs) = target else {
            panic!("expected unresolved target, got {target:?}")
        };
        assert!(matches!(
            objs.as_slice(),
            [TextObj::Plain(p), TextObj::IncludeVar { name, .. }]
                if p == "https://www.obscurative.ru/" && name == "page"
        ));
    }

    #[test]
    fn triple_link_var_target_is_unresolved() {
        // Any link target with variable slots — not only `[[a href=…]]` —
        // stays `Unresolved` until substitution classifies the flattened
        // text (the `%%fullname%%` used to hide as a literal `Page` path).
        let c = parse("[[[%%fullname%%|t]]] [[[{$page}]]]");
        let Node::Link { target, text, .. } = &c[0] else {
            panic!("expected link, got {:?}", c[0])
        };
        assert!(matches!(
            target,
            LinkTarget::Unresolved(o) if matches!(&o[..], [TextObj::ModuleVar { name, .. }] if name == "fullname")
        ));
        assert!(matches!(text.as_slice(), [Node::Text(TextObj::Plain(t))] if t == "t"));
        // No explicit text: the target's own objs become the visible text,
        // so a resolved `{$page}` shows through in the label too.
        let Node::Link { target, text, .. } = &c[2] else {
            panic!("expected link, got {:?}", c[2])
        };
        assert!(matches!(
            target,
            LinkTarget::Unresolved(o) if matches!(&o[..], [TextObj::IncludeVar { name, .. }] if name == "page")
        ));
        assert!(matches!(
            text.as_slice(),
            [Node::Text(TextObj::IncludeVar { name, .. })] if name == "page"
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
        assert!(c.iter().any(|n| matches!(
            n,
            Node::Container {
                kind: ContainerKind::Color(_),
                ..
            }
        )));
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
        assert!(matches!(c[0], Node::Module { .. }));
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
        assert_eq!(
            c,
            vec![Node::Module {
                name: "Rate".to_string(),
                params: Params::new()
            }]
        );
    }

    #[test]
    fn code_block_is_raw() {
        // `[[code]]` body is verbatim (not parsed as wikitext): the `>` stays
        // literal rather than becoming a blockquote. The interior is kept
        // byte-faithful (`/code/N` serves it as stored); render trims it.
        let c = parse("[[code]]\n> line one\n**not bold**\n[[/code]]");
        assert_eq!(
            c,
            vec![Node::Code {
                ty: None,
                raw: "\n> line one\n**not bold**\n".to_string(),
            }]
        );
    }

    #[test]
    fn collapsible_basic() {
        let c = parse("[[collapsible show=\"+\" hide=\"-\"]]\nbody **bold**\n[[/collapsible]]");
        let Node::Collapsible { header, body } = &c[0] else {
            panic!("expected collapsible, got {c:#?}");
        };
        assert!(matches!(
            &header[..],
            [Node::CollapsibleHeader {
                open,
                close,
                folded: true,
                ..
            }] if open == "+" && close == "-"
        ));
        assert!(body.iter().any(|n| matches!(
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
    /// container. The unmatched `[[div]]` opener renders as `Node::Raw` and
    /// its would-be body stays in flow — nothing leaks as stray text.
    #[test]
    fn mismatched_closer_claimed_by_ancestor() {
        let c = parse("[[span]] outer [[div]] inner [[/span]] tail");
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
            "unpaired div opener should render raw: {span:#?}"
        );
        assert!(
            span.iter()
                .any(|n| matches!(n, Node::Text(TextObj::Plain(s)) if s.contains("inner"))),
            "the div body stays in flow inside the span: {span:#?}"
        );
        assert!(
            c.iter()
                .any(|n| matches!(n, Node::Text(TextObj::Plain(s)) if s.contains("tail"))),
            "tail lost: {c:#?}"
        );
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
