use super::*;
use crate::wikidot_parser::ClosedTag;
use crate::wikidot_parser::lexer::{OpenTag, Tok, lex_bracket, lex_link1, lex_link3};
use crate::wikidot_parser::split_include_args;
use std::ops::Range;

// =========================================================================
// `[[include]]` resolution — textual splicing, variable substitution,
// dependency tree
// =========================================================================

/// Resolve an include's [`PageRef`] to `(site, slug)` on the current site.
/// The parser parks the first `:`-segment of the source in [`PageRef::space`];
/// for same-site page refs that segment is the category, so `space` → category
/// and the trailing path → name. Cross-site includes (`space` = another site)
/// are not yet supported. Unresolvable targets (bad path component) return
/// `None` and the directive is left in place.
fn include_target(
    src: &PageRef,
    current_site: &SafePathComponent,
) -> Option<(SafePathComponent, Slug)> {
    let name = SafePathComponent::new(src.path.last()?.clone())?;
    let category = match &src.space {
        Some(cat) => Some(SafePathComponent::new(cat.clone())?),
        None => None,
    };
    Some((current_site.clone(), (category, name)))
}

/// One `[[include …]]` directive of a raw page body that the parser will
/// honour: the directive's byte span, its source page, and its raw
/// `key=value` bindings (values literal text, quotes unwrapped).
pub(super) struct LiveDirective {
    span: Range<usize>,
    source: PageRef,
    vars: Vec<(String, String)>,
}

impl LiveDirective {
    /// The `(site, slug)` this directive names on `current_site`.
    pub(super) fn target(
        &self,
        current_site: &SafePathComponent,
    ) -> Option<(SafePathComponent, Slug)> {
        include_target(&self.source, current_site)
    }
}

/// The `[[include]]` directives of a raw body that survive to the parse.
/// A hand-rolled scan over the raw bytes that reuses the lexer's own
/// recognisers (`lex_bracket`, `lex_link3`, `lex_link1`), so it sees exactly
/// the constructs the lexer would — the whole point being that it never
/// lexes or pairs: this runs once per splice level and the full parse is
/// exactly once, at the end.
///
/// The scan tracks the constructs that can hide a directive: the line
/// escape `@@…@@`, the inline `{{…}}`/`%%…%%`/`{$…}` slots, links — and
/// above all the verbatim regions (`[!--…--]`, `[[code]]`, `[[module
/// css]]`), pairing their openers and closers the way the pairer does:
/// `[[/module]]` claims the topmost module entry (a verbatim css region or
/// a plain module body), a verbatim close drops every region opened inside
/// it, and an opener that never closes keeps the rest of the page live
/// (no rollback). A directive inside a *closed* verbatim region stays
/// literal; that is exactly the set the parser builds [`Node::Include`]s
/// from — and so exactly the set Wikidot splices; an include "documented"
/// inside a code block stays literal.
///
/// The splice set is line-anchored like the live rule itself
/// (`/^\[\[include …\]\]$/ims`): a directive must sit at the very start
/// of its line or the raw rule never fires and the text stays literal
/// (quoted samples, attribute values); only the tail stays loose — the
/// live rule's lazy args run to the next `]]` before a newline, loose
/// enough that a stray bracket after our balanced close still splices.
pub(super) fn live_directives(text: &str) -> Vec<LiveDirective> {
    let b = text.as_bytes();
    // (start, key, verbatim) open module/comment/code regions; module
    // bodies are plain (non-verbatim) but still claim a `[[/module]]`.
    #[derive(PartialEq, Clone, Copy)]
    enum Key {
        Comment,
        Code,
        Module,
    }
    let mut stack: Vec<(usize, Key, bool)> = Vec::new();
    let mut closed: Vec<Range<usize>> = Vec::new();
    let mut directives = Vec::new();
    let close = |stack: &mut Vec<(usize, Key, bool)>,
                 closed: &mut Vec<Range<usize>>,
                 key: Key,
                 end: usize| {
        if let Some(j) = stack.iter().rposition(|&(_, k, _)| k == key) {
            let (start, _, verbatim) = stack[j];
            stack.truncate(j);
            if verbatim {
                closed.push(start..end);
            }
        }
    };
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'@' if b[i..].starts_with(b"@@") => {
                // `@@…@@` escape: the body hides constructs; it ends at the
                // closing `@@`, a newline, or end of input.
                let mut j = i + 2;
                while j < b.len() && b[j] != b'\n' && !b[j..].starts_with(b"@@") {
                    j += 1;
                }
                i = if b[j..].starts_with(b"@@") { j + 2 } else { j };
            }
            b'{' if b[i..].starts_with(b"{{") || b[i..].starts_with(b"{$") => {
                // `{{…}}` / `{$…}` slots: hidden when they close on the line.
                let (delim, dl) = if b[i + 1] == b'{' {
                    (b"}}" as &[u8], 2)
                } else {
                    (b"}" as &[u8], 1)
                };
                let mut j = i + 2;
                while j < b.len() && b[j] != b'\n' && !b[j..].starts_with(delim) {
                    j += 1;
                }
                i = if b[j..].starts_with(delim) {
                    j + dl
                } else {
                    i + 1
                };
            }
            b'%' if b[i..].starts_with(b"%%") => {
                let mut j = i + 2;
                while j < b.len() && b[j] != b'\n' && !b[j..].starts_with(b"%%") {
                    j += 1;
                }
                i = if b[j..].starts_with(b"%%") {
                    j + 2
                } else {
                    i + 1
                };
            }
            b'-' if b[i..].starts_with(b"--]") => {
                close(&mut stack, &mut closed, Key::Comment, i + 3);
                i += 3;
            }
            b'[' => {
                if b[i..].starts_with(b"[!--") {
                    stack.push((i, Key::Comment, true));
                    i += 4;
                } else if b[i..].starts_with(b"[[[")
                    && let Some((end, _, _)) = lex_link3(b, i)
                {
                    i = end;
                } else if b[i..].starts_with(b"[[")
                    && let Some((end, tok)) = lex_bracket(b, i)
                {
                    match &tok {
                        Tok::Open(OpenTag::Include { raw }) if i == 0 || b[i - 1] == b'\n' => {
                            let (source, vars) = split_include_args(raw);
                            directives.push(LiveDirective {
                                span: i..end,
                                source,
                                vars,
                            });
                        }
                        Tok::Open(OpenTag::Code { .. }) => {
                            stack.push((i, Key::Code, true));
                        }
                        Tok::Open(OpenTag::Css) => {
                            stack.push((i, Key::Module, true));
                        }
                        Tok::Open(OpenTag::ModuleBlock { .. })
                        | Tok::Open(OpenTag::ListPages { .. }) => {
                            stack.push((i, Key::Module, false));
                        }
                        Tok::Close(ClosedTag::Code) => {
                            close(&mut stack, &mut closed, Key::Code, end);
                        }
                        Tok::Close(ClosedTag::Module) => {
                            close(&mut stack, &mut closed, Key::Module, end);
                        }
                        _ => {}
                    }
                    i = end;
                } else if let Some((end, _, _)) = lex_link1(b, i) {
                    i = end;
                } else {
                    i += 1;
                }
            }
            _ => i += 1,
        }
    }
    directives.retain(|d| {
        !closed
            .iter()
            .any(|r| r.start < d.span.start && d.span.end <= r.end)
    });
    directives
}

/// Assemble the body of the page at the head of `path` in one recursive
/// pass: every live `[[include]]` whose target's body is in `raws` and is
/// not already on the recursion `path` (a data-level cycle) is replaced by
/// that body with the directive's bindings substituted into it — which
/// resolves the values of directives nested inside it too, so vars cascade
/// top-down through the chain — and the spliced body is assembled in turn
/// with the target pushed onto the path. Directives that cannot be spliced
/// — a cycle, a target with no fetched body, a cross-site include — stay
/// verbatim, degrading to [`Node::Include`] in the final parse (the render
/// fallback).
///
/// Wikidot splices includes into the raw text and parses the assembled
/// whole, which is what makes a component's half-open `[[div]]` or
/// `[[cell]]` pair with the includer's closer — this pass keeps that
/// property by construction: it never parses, it only replaces text.
pub(super) fn splice_includes(
    text: &str,
    current_site: &SafePathComponent,
    raws: &HashMap<Key, Arc<str>>,
    path: &[Key],
) -> String {
    let directives = live_directives(text);
    let mut out = String::with_capacity(text.len());
    let mut pos = 0;
    for d in directives {
        out.push_str(&text[pos..d.span.start]);
        pos = d.span.end;
        let target = d
            .target(current_site)
            .map(|(site, slug)| (site, slug.0, slug.1));
        if let Some(key) = target.as_ref().filter(|k| !path.contains(k))
            && let Some(raw) = raws.get(key)
        {
            let inner = subst_vars(raw, &d.vars);
            let mut deeper = path.to_vec();
            deeper.push(key.clone());
            out.push_str(&splice_includes(&inner, current_site, raws, &deeper));
        } else {
            out.push_str(&text[d.span]);
        }
    }
    out.push_str(&text[pos..]);
    out
}

/// Substitute the `{$var}` slots of `text` — the textual half of Wikidot's
/// include assembly: values are pasted into the raw body, so a value's
/// markup re-parses in place (bold stays bold in body text and attribute
/// values alike). The lookup mirrors the tree walk this replaces: bindings
/// stay in source order and the first non-empty value wins — which is what
/// makes the `key={$key}|key=default` fallback idiom work, since an unset
/// `{$key}` has already substituted to empty here — and an unresolved name
/// falls back to its `//default`, or to nothing. The scan is the lexer's own
/// `{$…}` grammar: a slot runs to the closing `}`, and a newline or end of
/// input before one leaves the `{$` literal.
pub(super) fn subst_vars(text: &str, vars: &[(String, String)]) -> String {
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while let Some(rel) = text[i..].find("{$") {
        let at = i + rel;
        out.push_str(&text[i..at]);
        let rest = &text[at + 2..];
        match rest.find(['}', '\n']) {
            Some(j) if rest.as_bytes()[j] == b'}' => {
                let raw = &rest[..j];
                let (name, default) = raw.split_once("//").unwrap_or((raw, ""));
                let value = vars.iter().find(|(k, v)| k == name && !v.trim().is_empty());
                out.push_str(value.map_or(default, |(_, v)| v.as_str()));
                i = at + 2 + j + 1;
            }
            _ => {
                out.push_str("{$");
                i = at + 2;
            }
        }
    }
    out.push_str(&text[i..]);
    out
}

/// Fold the discovery-order `(includer, included)` edges into the page's
/// dependency tree: one [`PageDep`] per fetched page, nested under the page
/// whose body first included it; `root`'s direct includes form the top level.
pub(super) fn dep_tree(root: &Key, edges: Vec<(Key, Key)>) -> Vec<PageDep> {
    let mut children: HashMap<Key, Vec<Key>> = HashMap::new();
    for (includer, included) in edges {
        children.entry(includer).or_default().push(included);
    }
    dep_children(&children, children.get(root).map_or(&[], Vec::as_slice))
}

fn dep_children(children: &HashMap<Key, Vec<Key>>, keys: &[Key]) -> Vec<PageDep> {
    keys.iter()
        .map(|key| {
            page_dep(
                key,
                dep_children(children, children.get(key).map_or(&[], Vec::as_slice)),
            )
        })
        .collect()
}

/// One fetched page as a [`PageDep`]: its `(site, category, page)` address,
/// with its nested deps.
pub(super) fn page_dep(key: &Key, deps: Vec<PageDep>) -> PageDep {
    PageDep {
        site: (*key.0).clone(),
        category: key.1.as_ref().map(|c| (**c).clone()),
        page: (*key.2).clone(),
        deps,
    }
}
