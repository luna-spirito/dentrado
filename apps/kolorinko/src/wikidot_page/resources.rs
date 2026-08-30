use super::*;

// =========================================================================
// Mirrored-resource resolution (`/repo/…` content-addressed URLs)
// =========================================================================

/// Resolve every mirrored external resource — `[[image source]]`, `[[[url]]]`
/// link targets, and `url()`/`@import` references inside `[[module css]]` — to
/// its content-addressed `/-/repo/<site>/files/<xx>/<yy>/<hash>.<ext>` URL and
/// substitute, each through a local snapshot lookup
/// ([`RepoSnapshot::resource`]). A URL that isn't mirrored is retried as
/// Wikidot's `/code/N` endpoint ([`code_url_for_tail`]) and pointed at the
/// local slug-family code route; anything else is left as its original
/// absolute URL (a hotlink the client loads straight from the origin).
pub(super) fn resolve_resources(
    content: Content,
    site: &SafePathComponent,
    snap: &RepoSnapshot,
) -> Content {
    let mut tails: Vec<String> = Vec::new();
    collect_external_refs(&content, &mut tails);
    if tails.is_empty() {
        return content;
    }
    let mut resolved: HashMap<String, CaRef> = HashMap::new();
    let mut code: HashMap<String, String> = HashMap::new();
    for tail in &tails {
        let Some(path) = RepoAssetPath::new(percent_decode(tail)) else {
            continue;
        };
        match resource(snap, site, &path) {
            Some(ca_ref) => {
                resolved.insert(tail.clone(), ca_ref);
            }
            None => {
                if let Some(url) = code_url_for_tail(tail) {
                    code.insert(tail.clone(), url);
                }
            }
        }
    }
    substitute_resources(content, site, &resolved, &code)
}

/// Walk `content` and collect every mirrored-attachment `host/path` tail
/// reachable from an image source, a URL link target, or a stylesheet
/// reference — deduplicated, in first-appearance order.
pub(super) fn collect_external_refs(content: &Content, out: &mut Vec<String>) {
    let push = |t: String, out: &mut Vec<String>| {
        if !out.iter().any(|x| x == &t) {
            out.push(t);
        }
    };
    for node in content {
        match node {
            Node::Image { source, .. } => {
                if let Some(t) = ref_tail_of(source) {
                    push(t, out);
                }
            }
            Node::Link {
                target: LinkTarget::Url(u),
                ..
            } => {
                if let Some(t) = http_tail(u, None) {
                    push(t, out);
                }
            }
            Node::Stylesheet(css) => {
                for t in http_refs(css) {
                    push(t, out);
                }
            }
            other => other.visit_node(&mut |c| collect_external_refs(c, out)),
        }
    }
}

/// The http `host/path` tail of an image `source`, but only when it is purely
/// literal text (no module/include variables) — a variable URL can't be
/// content-addressed statically and is left for the client to resolve at render.
fn ref_tail_of(source: &[TextObj]) -> Option<String> {
    TextObj::plain_concat(source).and_then(|url| http_tail(&url, None))
}

/// Replace every mirrored-attachment reference in `content` with its
/// content-addressed URL from `resolved` (`host/path` tail → [`CaRef`]), or
/// its local code route from `code` (`host/path` tail → `/S…/<slug>/code/N`,
/// the `/code/N` fallback of [`resolve_resources`]). References absent from
/// both (un-mirrored hotlinks) pass through unchanged.
pub(super) fn substitute_resources(
    content: Content,
    site: &SafePathComponent,
    resolved: &HashMap<String, CaRef>,
    code: &HashMap<String, String>,
) -> Content {
    let url_for = |tail: &str| {
        resolved
            .get(tail)
            .map(|ca| ca_url(site, ca))
            .or_else(|| code.get(tail).cloned())
    };
    let mut walk = |c: Content| substitute_resources(c, site, resolved, code);
    content
        .into_iter()
        .map(|node| match node {
            Node::Image {
                align,
                source,
                params,
            } => Node::Image {
                align,
                source: subst_source(source, url_for),
                params,
            },
            Node::Link {
                target,
                text,
                class,
                new_tab,
            } => Node::Link {
                new_tab,
                target: match target {
                    LinkTarget::Url(u) => match http_tail(&u, None).and_then(|t| url_for(&t)) {
                        Some(url) => LinkTarget::Url(url),
                        None => LinkTarget::Url(u),
                    },
                    other => other,
                },
                text: walk(text),
                class,
            },
            Node::Stylesheet(css) => Node::Stylesheet(rewrite_with(&css, None, url_for)),
            other => other.map_node(&mut walk),
        })
        .collect()
}

/// Rewrite a purely-literal image `source` (`[Plain(url)]`) to its resolved
/// URL when `url_for` matches it; leave sources with variables or non-http
/// URLs as-is.
fn subst_source<F: Fn(&str) -> Option<String>>(source: Vec<TextObj>, url_for: F) -> Vec<TextObj> {
    if let Some(url) = TextObj::plain_concat(&source)
        && let Some(tail) = http_tail(&url, None)
        && let Some(resolved) = url_for(&tail)
    {
        return vec![TextObj::Plain(resolved)];
    }
    source
}

// =========================================================================
// Wikidot `/code/N` endpoints → local slug-family code routes
// =========================================================================

/// Rewrite an external reference of Wikidot's `/code/N` endpoint shape to
/// the local route that serves it — `/<space>/<slug>/code/N`, the slug
/// family's code tail (see [`crate::respond`]) — so theme-component
/// `@import`s load from the mirror instead of the origin (which, beyond
/// archive purity, also stops them being blocked as mixed content:
/// `http://` subresources of the HTTPS mirror). Recognized tails, covering
/// every form in the corpus:
/// - `<host>/<page>/code/<N>` — `<sub>.wikidot.com` hosts (± `www.`) and
///   configured alias domains (± `www.`);
/// - `<host>/local--code/<page>/<N>` — the `.wdfiles.com` alias the wikidot
///   form 302s to (page percent-encoded).
///
/// `None` for anything else — including sites no registered space serves
/// (an un-mirrored wiki keeps its hotlink rather than a guaranteed-404
/// local route) and multi-segment pages (`forum/t-123/code/1`, which no
/// code endpoint ever answered).
pub(super) fn code_url_for_tail(tail: &str) -> Option<String> {
    let (host, rest) = tail.split_once('/')?;
    let space = code_space(host)?;
    let segs: Vec<&str> = rest.split('/').collect();
    let (page_raw, n) = match segs.as_slice() {
        [page, "code", n] | ["local--code", page, n] => (*page, *n),
        _ => return None,
    };
    let n: u32 = n.parse().ok()?;
    let page = percent_decode(page_raw);
    if page.contains('/') {
        return None; // a decoded `%2F` never makes a second segment
    }
    let slug = match page.split_once(':') {
        Some((cat, name)) => (
            Some(SafePathComponent::new(cat.to_owned())?),
            SafePathComponent::new(name.to_owned())?,
        ),
        None => (None, SafePathComponent::new(page)?),
    };
    let cat = slug
        .0
        .as_ref()
        .map_or(String::new(), |c| format!("{}:", **c));
    Some(format!("/{space}/{cat}{}/code/{n}", *slug.1))
}

/// The registered space a code-endpoint host belongs to: a
/// `<sub>.wikidot.com` / `<sub>.wdfiles.com` host (± `www.`) names `sub` —
/// either a registered site directly, or (the `.wdfiles.com` form of) a
/// configured alias domain; any other host must be a configured alias
/// domain itself (± `www.` — the corpus references `www.rpc-wiki.net`
/// while configs list the bare form). `None` for hosts no registered space
/// claims (the reference stays a hotlink).
fn code_space(host: &str) -> Option<SpaceId> {
    let base = host
        .strip_suffix(".wikidot.com")
        .or_else(|| host.strip_suffix(".wdfiles.com"))
        .map_or(host, |sub| sub.strip_prefix("www.").unwrap_or(sub));
    let by_name =
        SafePathComponent::new(base.to_owned()).and_then(|s| crate::globals::space_of(&s));
    by_name.or_else(|| {
        crate::globals::space_of_domain(base)
            .or_else(|| {
                base.strip_prefix("www.")
                    .and_then(crate::globals::space_of_domain)
            })
            .map(|(space, _)| space)
    })
}
