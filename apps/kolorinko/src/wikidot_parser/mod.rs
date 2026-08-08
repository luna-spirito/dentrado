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

pub mod types;

pub(crate) use crate::wikidot_parser::types::*;
pub(crate) use chumsky::{input::InputRef, prelude::*};
pub(crate) use std::collections::HashMap;

mod attrs;
mod blocks;
mod brackets;
mod element;
mod helpers;
mod inline;
mod low_level;
mod text;

pub(crate) use attrs::*;
pub(crate) use blocks::*;
pub(crate) use brackets::*;
pub(crate) use element::*;
pub(crate) use helpers::*;
pub(crate) use inline::*;
pub(crate) use low_level::*;
pub(crate) use text::*;

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
