use super::*;

// =========================================================================
// Variable substitution — ListPages module vars (`%%x%%`). The include
// vars (`{$x}`) never reach this pass: include assembly is textual (see
// [`super::includes`]), so by the time a tree exists every `{$x}` has
// already been replaced by literal text.
// =========================================================================

/// Substitute every module variable throughout `content` per the visible
/// [`Vars`].
pub(super) fn apply_vars(content: Content, vars: &Vars) -> Content {
    content
        .into_iter()
        .flat_map(|n| subst_node(n, vars))
        .collect()
}

/// The variable bindings visible while a ListPages template is instantiated
/// for one listed page — that page's module vars (`%%x%%`). Outside an
/// instantiation they pass through untouched.
pub(super) struct Vars<'a> {
    module: Option<ModuleVars<'a>>,
}

/// The `%%x%%` bindings of one ListPages template instantiation. Unsupported
/// variables resolve to `None` and keep their literal `%%name%%` form for the
/// render fallback.
struct ModuleVars<'a> {
    site: &'a SafePathComponent,
    page: Option<&'a ListedPage>,
    /// The listed page's rendered body (`%%content%%`), fetched only for
    /// templates that reference it; `None` otherwise (and in prepend/append).
    content: Option<&'a Content>,
    index: i64,
    total: i64,
    limit: Option<i64>,
}

impl<'a> Vars<'a> {
    /// No module instantiation in scope — module vars pass through
    /// untouched.
    #[cfg(test)]
    pub(super) fn none() -> Self {
        Self { module: None }
    }

    /// The `%%x%%` bindings of one ListPages template instantiation: the
    /// listed `page`'s own properties (`None` in the once-rendered
    /// prepend/append), its rendered `content` body (fetched only for
    /// templates that reference it), and the module's reporting variables.
    pub(super) fn module(
        site: &'a SafePathComponent,
        page: Option<&'a ListedPage>,
        content: Option<&'a Content>,
        index: i64,
        total: i64,
        limit: Option<i64>,
    ) -> Self {
        Self {
            module: Some(ModuleVars {
                site,
                page,
                content,
                index,
                total,
                limit,
            }),
        }
    }
}

impl ModuleVars<'_> {
    fn resolve(&self, name: &str, arg: Option<&str>) -> Option<Content> {
        self.resolve_content(name)
            .or_else(|| self.resolve_page(name, arg))
            .or_else(|| self.resolve_reporting(name))
    }

    /// The listed page's rendered body (`%%content%%`/`%%body%%`), or its
    /// first paragraph (`%%first_paragraph%%`): available only for templates
    /// that reference one of them, `None` (and so left verbatim) otherwise —
    /// including in the once-rendered prepend/append.
    fn resolve_content(&self, name: &str) -> Option<Content> {
        match name {
            "content" | "body" => self.content.cloned(),
            "first_paragraph" => self.content.map(first_paragraph),
            _ => None,
        }
    }

    /// The listed page's own variables; `None` with no page in scope (the
    /// once-rendered prepend/append) or for an unsupported name.
    fn resolve_page(&self, name: &str, arg: Option<&str>) -> Option<Content> {
        let text = |s: String| vec![Node::Text(TextObj::Plain(s))];
        let date = |ts: i64| {
            vec![Node::Date {
                timestamp: ts,
                format: arg.map(str::to_string),
            }]
        };
        let page = self.page?;
        let link = |prefix: &str, tag: &str| {
            vec![Node::Link {
                target: LinkTarget::Url(format!("{prefix}{tag}")),
                text: text(tag.to_string()),
                class: None,
                new_tab: false,
            }]
        };
        let tags_linked = || {
            let prefix = arg.unwrap_or("/system:page-tags/tag/");
            let mut out = Content::new();
            for (i, tag) in page
                .tags
                .iter()
                .filter(|t| t.starts_with('_') == name.starts_with('_'))
                .enumerate()
            {
                if i > 0 {
                    out.push(Node::Text(TextObj::Plain(" ".into())));
                }
                out.extend(link(prefix, tag));
            }
            out
        };
        let tags = || {
            text(
                page.tags
                    .iter()
                    .filter(|t| t.starts_with('_') == name.starts_with('_'))
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(" "),
            )
        };
        Some(match name {
            "name" => text(page.name.clone()),
            "category" => text(page.category.clone().unwrap_or_default()),
            "fullname" | "page_unix_name" | "full_page_name" => text(page.fullname()),
            "title" => text(page.title.clone()),
            "title_linked" | "linked_title" => vec![Node::Link {
                target: LinkTarget::Page(PageRef {
                    space: page.category.clone(),
                    path: vec![page.name.clone()],
                }),
                text: text(page.title.clone()),
                class: None,
                new_tab: false,
            }],
            "created_by" | "author" => text(page.created_by.clone()),
            "updated_by" | "author_edited" | "user_edited" => text(page.updated_by.clone()),
            "created_at" | "date" => date(page.created_at),
            "updated_at" | "date_edited" => date(page.updated_at),
            "tags" | "_tags" => tags(),
            "tags_linked" | "_tags_linked" => tags_linked(),
            // No vote data exists in the export; a zero rating is what Wikidot
            // itself shows for an unvoted page.
            "rating" | "rating_votes" | "rating_percent" => text("0".into()),
            "rating_decimal" => text("0.00".into()),
            "revisions" => text(page.revisions.to_string()),
            "link" => text(match &page.category {
                Some(c) => format!("/{}/{}:{}", **self.site, c, page.name),
                None => format!("/{}/{}", **self.site, page.name),
            }),
            _ => return None,
        })
    }

    /// The module-level reporting variables, bound even in the once-rendered
    /// prepend/append (where no listed page is in scope).
    fn resolve_reporting(&self, name: &str) -> Option<Content> {
        let text = |s: String| vec![Node::Text(TextObj::Plain(s))];
        Some(match name {
            "index" => text(self.index.to_string()),
            "total" => text(self.total.to_string()),
            "limit" => text(self.limit.map(|l| l.to_string()).unwrap_or_default()),
            "total_or_limit" => text(
                self.limit
                    .map_or(self.total, |l| l.min(self.total))
                    .to_string(),
            ),
            _ => return None,
        })
    }
}

/// Substitute every variable inside one node; a node may expand to several
/// (a resolved module var) or to none.
fn subst_node(node: Node, vars: &Vars) -> Content {
    match node {
        // Include vars are resolved before the tree exists; a stray one can
        // only show its default.
        Node::Text(TextObj::IncludeVar { default, .. }) => {
            default.map(|d| apply_vars(d, vars)).unwrap_or_default()
        }
        // A module var resolves against the listed page in scope; without one
        // (outside a ListPages instantiation) it passes through untouched for
        // the later instantiation (or the render fallback).
        Node::Text(TextObj::ModuleVar {
            ref name,
            ref default,
        }) => match &vars.module {
            Some(m) => match m.resolve(name, default.as_deref()) {
                Some(content) => apply_vars(content, vars),
                None => match default {
                    Some(d) => vec![Node::Text(TextObj::Plain(d.clone()))],
                    None => vec![node],
                },
            },
            None => vec![node],
        },
        Node::Text(other) => vec![Node::Text(other)],
        // Wikidot substitutes includes textually before parsing, so a
        // `[[module css]]` body — carried verbatim as a raw string — sees
        // the values, not the `{$var}` slots.
        Node::Stylesheet(css) => vec![Node::Stylesheet(subst_stylesheet(css, vars))],
        Node::Container { kind, content } => vec![Node::Container {
            kind: subst_kind(kind, vars),
            content: apply_vars(content, vars),
        }],
        Node::Image {
            align,
            source,
            params,
        } => vec![Node::Image {
            align,
            source: subst_textobjs(source, vars),
            params: subst_params(params, vars),
        }],
        Node::Iframe { source, params } => vec![Node::Iframe {
            source: subst_textobjs(source, vars),
            params: subst_params(params, vars),
        }],
        Node::BlockTable(t) => vec![Node::BlockTable(BlockTable {
            params: subst_params(t.params, vars),
            rows: t
                .rows
                .into_iter()
                .map(|r| BlockRow {
                    params: subst_params(r.params, vars),
                    content: apply_vars(r.content, vars),
                })
                .collect(),
        })],
        Node::BlockCell(c) => vec![Node::BlockCell(BlockCell {
            header: c.header,
            params: subst_params(c.params, vars),
            content: apply_vars(c.content, vars),
        })],
        Node::Link {
            target,
            text,
            class,
            new_tab,
        } => vec![Node::Link {
            target: subst_link_target(target, vars),
            text: apply_vars(text, vars),
            class,
            new_tab,
        }],
        Node::Include(inc) => vec![Node::Include(Include {
            source: inc.source,
            vars: inc
                .vars
                .into_iter()
                .map(|(k, v)| (k, apply_vars(v, vars)))
                .collect(),
        })],
        // `[[user %%created_by%%]]` (the ListPages template idiom): the lexer
        // keeps the name as plain text carrying the var, so resolve each
        // `%%var%%` slice textually against the module vars.
        Node::User { name, avatar } => vec![Node::User {
            name: subst_user_name(&name, vars),
            avatar,
        }],
        other => vec![other.map_node(&mut |c| apply_vars(c, vars))],
    }
}

/// A module-var name character (the lexer's `is_prop_char`, mirrored — var
/// names are `created_by`-shaped).
fn is_var_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, b'_' | b'#' | b'-')
}

/// The leading inline run of a resolved body — Wikidot's
/// `%%first_paragraph%%`: everything up to the first block node or blank
/// line, inline formatting intact.
fn first_paragraph(content: &Content) -> Content {
    let mut out = Content::new();
    for node in content {
        match node {
            Node::Text(TextObj::Plain(s)) => match s.find("\n\n") {
                Some(p) => {
                    if !s[..p].trim().is_empty() {
                        out.push(Node::Text(TextObj::Plain(s[..p].to_string())));
                    }
                    return out;
                }
                None => out.push(node.clone()),
            },
            // Block nodes (headings, tables, images, divs, lists, …) end the
            // paragraph; anything else (links, styled spans, raw text) is
            // inline and belongs to it.
            Node::Heading { .. }
            | Node::Table(_)
            | Node::BlockTable(_)
            | Node::BlockCell(_)
            | Node::Image { .. }
            | Node::HorizontalRule
            | Node::Clearfloat(_)
            | Node::Tabview { .. }
            | Node::FootnoteBlock { .. }
            | Node::ListPages(_)
            | Node::Stylesheet(_)
            | Node::Include(_)
            | Node::Raw(_)
            | Node::Code { .. }
            | Node::ModuleBlock { .. }
            | Node::Module { .. }
            | Node::List(_)
            | Node::Collapsible { .. }
            | Node::Container {
                kind:
                    ContainerKind::Quote
                    | ContainerKind::Align(_)
                    | ContainerKind::Div { inline: false, .. }
                    | ContainerKind::List { .. }
                    | ContainerKind::IfTags { .. },
                ..
            } => return out,
            other => out.push(other.clone()),
        }
    }
    out
}

/// Textual `%%var%%` (± `|default`) substitution inside a `[[user …]]` name.
/// Unresolvable slices stay verbatim for the render fallback.
fn subst_user_name(name: &str, vars: &Vars) -> String {
    if !name.contains("%%") {
        return name.to_string();
    }
    let b = name.as_bytes();
    let mut out = String::with_capacity(name.len());
    let mut i = 0;
    while i < b.len() {
        if b[i..].starts_with(b"%%") {
            let n = i + 2;
            let name_end = b[n..]
                .iter()
                .position(|c| !is_var_char(*c))
                .map_or(b.len(), |p| n + p);
            let var = &name[n..name_end];
            let (default, after) = match b.get(name_end) == Some(&b'|') {
                true => {
                    let d_end = b[name_end + 1..]
                        .windows(2)
                        .position(|w| w == b"%%")
                        .map_or(b.len(), |p| name_end + 1 + p);
                    (
                        Some(name[name_end + 1..d_end].to_string()),
                        b.get(d_end..)
                            .is_some_and(|r| r.starts_with(b"%%"))
                            .then_some(2),
                    )
                }
                false => (None, None),
            };
            let step = after.unwrap_or(0);
            let resolved = vars
                .module
                .as_ref()
                .and_then(|m| m.resolve(var, default.as_deref()));
            match resolved.and_then(flatten_text) {
                Some(text) => out.push_str(&text),
                None => out.push_str(&name[i..name_end + step]),
            }
            i = name_end + step;
        } else {
            let n = b[i..]
                .iter()
                .position(|c| *c == b'%')
                .map_or(b.len(), |p| i + p);
            out.push_str(&name[i..n]);
            i = n;
        }
    }
    out
}

/// A resolved variable's content flattened to plain text, when it is nothing
/// but plain text (the shape every user-name-producing var resolves to).
fn flatten_text(content: Content) -> Option<String> {
    let mut out = String::new();
    for node in content {
        match node {
            Node::Text(TextObj::Plain(s)) => out.push_str(&s),
            _ => return None,
        }
    }
    Some(out)
}

/// Resolve a link target under the visible [`Vars`]: an
/// [`LinkTarget::Unresolved`] — a target from any link kind that carried
/// `{$var}`/`%%var%%` slots — is substituted text-wise and, once fully
/// literal, re-classified through [`parse_link_target`]; a still-unresolved
/// target stays verbatim for the render fallback. Every other target passes
/// through.
fn subst_link_target(target: LinkTarget, vars: &Vars) -> LinkTarget {
    let LinkTarget::Unresolved(objs) = target else {
        return target;
    };
    let objs = subst_textobjs(objs, vars);
    TextObj::plain_concat(&objs).map_or(LinkTarget::Unresolved(objs), |s| parse_link_target(&s))
}

fn subst_kind(kind: ContainerKind, vars: &Vars) -> ContainerKind {
    match kind {
        ContainerKind::Div {
            inline,
            block,
            params,
        } => ContainerKind::Div {
            inline,
            block,
            params: subst_params(params, vars),
        },
        ContainerKind::List { tag, params } => ContainerKind::List {
            tag,
            params: subst_params(params, vars),
        },
        other => other,
    }
}

fn subst_params(
    params: HashMap<String, Vec<TextObj>>,
    vars: &Vars,
) -> HashMap<String, Vec<TextObj>> {
    params
        .into_iter()
        .map(|(k, v)| (k, subst_textobjs(v, vars)))
        .collect()
}

/// Substitute the `%%var%%` slots of a stylesheet body line by line (the
/// slot grammar is line-scoped, like everywhere else): each line resolves
/// through the same text-only flattening as an attribute value, keeping
/// its literal form when a slot cannot flatten.
fn subst_stylesheet(css: String, vars: &Vars) -> String {
    let mut out = String::with_capacity(css.len());
    for (i, line) in css.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let objs = subst_textobjs(crate::wikidot_parser::text_objs_of(line), vars);
        match TextObj::plain_concat(&objs) {
            Some(s) => out.push_str(&s),
            None => out.push_str(line),
        }
    }
    out
}

/// Substitute variables inside one attribute value / image source — the
/// text-only contexts where a resolved value flattens to plain text.
fn subst_textobjs(objs: Vec<TextObj>, vars: &Vars) -> Vec<TextObj> {
    let mut out: Vec<TextObj> = Vec::new();
    for o in objs {
        let resolved: Vec<TextObj> = match o {
            TextObj::IncludeVar { default, .. } => default
                .map(|d| flatten_textobjs(&apply_vars(d, vars)))
                .unwrap_or_default(),
            TextObj::ModuleVar {
                ref name,
                ref default,
            } => match &vars.module {
                Some(m) => match m.resolve(name, default.as_deref()) {
                    Some(content) => flatten_textobjs(&apply_vars(content, vars)),
                    None => match default {
                        Some(d) => vec![TextObj::Plain(d.clone())],
                        None => vec![o],
                    },
                },
                None => vec![o],
            },
            other => vec![other],
        };
        for r in resolved {
            match (&r, out.last_mut()) {
                (TextObj::Plain(s), Some(TextObj::Plain(prev))) => prev.push_str(s),
                _ => out.push(r),
            }
        }
    }
    out
}

/// Flatten parsed [`Content`] back into plain [`TextObj`] text for the contexts
/// (attribute values, image sources) that only carry text.
fn flatten_textobjs(content: &Content) -> Vec<TextObj> {
    let mut s = String::new();
    collect_plain(content, &mut s);
    if s.is_empty() {
        Vec::new()
    } else {
        vec![TextObj::Plain(s)]
    }
}

/// Extract the plain-text projection of `content`: literal text, plus the
/// defaults of unresolved variables.
pub(super) fn collect_plain(content: &Content, out: &mut String) {
    for n in content {
        match n {
            Node::Text(TextObj::Plain(s)) => out.push_str(s),
            Node::Text(TextObj::IncludeVar {
                default: Some(d), ..
            }) => collect_plain(d, out),
            Node::Text(TextObj::ModuleVar {
                default: Some(d), ..
            }) => out.push_str(d),
            other => other.visit_node(&mut |c| collect_plain(c, out)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn listed(name: &str, cat: Option<&str>, title: &str, tags: &[&str]) -> ListedPage {
        ListedPage {
            name: name.into(),
            category: cat.map(str::to_string),
            page_id: "1".into(),
            title: title.into(),
            tags: tags.iter().map(|s| (*s).to_string()).collect(),
            created_by: "user".into(),
            created_at: 1_600_000_000,
            updated_by: "user".into(),
            updated_at: 1_610_000_000,
            revisions: 3,
        }
    }

    fn plain(content: &Content) -> String {
        let mut s = String::new();
        collect_plain(content, &mut s);
        s
    }

    fn page_vars<'a>(
        site: &'a SafePathComponent,
        page: &'a ListedPage,
        content: Option<&'a Content>,
    ) -> Vars<'a> {
        Vars::module(site, Some(page), content, 1, 5, None)
    }

    #[test]
    fn module_var_resolves_to_page_field() {
        let site = SafePathComponent::new("site".into()).unwrap();
        let page = listed("foo", Some("rumor-n"), "Foo Title", &["rumor"]);
        let vars = page_vars(&site, &page, None);
        let out = apply_vars(parse("%%title%% — %%fullname%%"), &vars);
        assert_eq!(plain(&out), "Foo Title — rumor-n:foo");
    }

    #[test]
    fn unknown_module_var_passes_through() {
        let vars = Vars::none();
        let out = apply_vars(parse("%%nope%%"), &vars);
        // Untouched: the module var survives for the render fallback.
        assert!(
            matches!(&out[..], [Node::Text(TextObj::ModuleVar { name, .. })] if name == "nope")
        );
    }

    #[test]
    fn bare_var_link_target_resolves() {
        let site = SafePathComponent::new("site".into()).unwrap();
        let page = listed("foo", Some("rumor-n"), "Foo", &[]);
        let vars = page_vars(&site, &page, None);
        let out = apply_vars(parse("[[[%%fullname%%|%%title%%]]]"), &vars);
        let Node::Link { target, text, .. } = &out[0] else {
            panic!("expected link: {out:?}")
        };
        let LinkTarget::Page(p) = target else {
            panic!("expected page target: {target:?}")
        };
        assert_eq!(p.space.as_deref(), Some("rumor-n"));
        assert_eq!(p.path, ["foo"]);
        assert_eq!(plain(text), "Foo");
    }

    #[test]
    fn unresolved_anchor_href_module_var_resolves_in_scope() {
        let site = SafePathComponent::new("site".into()).unwrap();
        let page = listed("foo", Some("rumor-n"), "Foo", &[]);
        let vars = page_vars(&site, &page, None);
        let out = apply_vars(parse("[[a href=\"%%name%%\"]]x[[/a]]"), &vars);
        let Node::Link { target, .. } = &out[0] else {
            panic!("expected link: {out:?}")
        };
        let LinkTarget::Page(p) = target else {
            panic!("expected page target: {target:?}")
        };
        assert_eq!(p.path, ["foo"]);
    }

    #[test]
    fn unresolved_anchor_href_without_scope_stays_verbatim() {
        // No listed page in scope: the module var slot survives for the
        // render fallback instead of being dropped or half-substituted.
        let vars = Vars::none();
        let out = apply_vars(parse("[[a href=\"/tag/%%name%%\"]]x[[/a]]"), &vars);
        let Node::Link { target, .. } = &out[0] else {
            panic!("expected link: {out:?}")
        };
        let LinkTarget::Unresolved(objs) = target else {
            panic!("expected unresolved target: {target:?}")
        };
        assert!(matches!(
            objs.as_slice(),
            [TextObj::Plain(_), TextObj::ModuleVar { name, .. }] if name == "name"
        ));
    }

    #[test]
    fn reporting_vars_available_without_page() {
        let site = SafePathComponent::new("site".into()).unwrap();
        let vars = Vars::module(&site, None, None, 0, 7, Some(5));
        let out = apply_vars(parse("%%total%% of %%limit%%"), &vars);
        assert_eq!(plain(&out), "7 of 5");
    }

    #[test]
    fn content_var_embeds_listed_page_body() {
        let site = SafePathComponent::new("site".into()).unwrap();
        let page = listed("foo", Some("rumor-n"), "Foo", &["rumor"]);
        let body = parse("hello from foo");
        let vars = page_vars(&site, &page, Some(&body));
        let out = apply_vars(parse("wrap|%%content%%|end"), &vars);
        assert_eq!(plain(&out), "wrap|hello from foo|end");
    }

    #[test]
    fn content_var_absent_without_body() {
        let site = SafePathComponent::new("site".into()).unwrap();
        let page = listed("foo", None, "Foo", &[]);
        let vars = page_vars(&site, &page, None);
        let out = apply_vars(parse("%%content%%"), &vars);
        // No body fetched → passes through verbatim for the render fallback.
        assert!(
            matches!(&out[..], [Node::Text(TextObj::ModuleVar { name, .. })] if name == "content")
        );
    }
}
