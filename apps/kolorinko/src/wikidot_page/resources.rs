use super::*;

// =========================================================================
// Mirrored-resource resolution (`/repo/…` content-addressed URLs)
// =========================================================================

/// Resolve every mirrored external resource — `[[image source]]`, `[[[url]]]`
/// link targets, and `url()`/`@import` references inside `[[module css]]` — to
/// its content-addressed `/repo/<site>/files/<xx>/<yy>/<hash>.<ext>` URL and
/// substitute. Each resource is declared as a [`repo_resource`]
/// `secondary_get` dependency, so the result is reactive to an attachment
/// being re-mirrored (new hash) anywhere in the (already include-resolved)
/// tree. URLs that aren't mirrored (hotlinks) are left untouched so the client
/// loads them straight from the origin.
pub(super) async fn resolve_resources<S: Storage<KolorinkoRT>>(
    content: Content,
    site: &SafePathComponent,
    meta: &RepoMeta,
    ctx: &mut GearCtx<KolorinkoRT, S>,
) -> Content {
    let mut tails: Vec<String> = Vec::new();
    collect_external_refs(&content, &mut tails);
    if tails.is_empty() {
        return content;
    }
    let mut resolved: HashMap<String, CaRef> = HashMap::new();
    for tail in &tails {
        let Some(path) = RepoAssetPath::new(percent_decode(tail)) else {
            continue;
        };
        let ca = crate::runtime::repo_resource(meta.clone(), site.clone(), path)
            .secondary_get(ctx)
            .await;
        if let Some(ca_ref) = &*ca {
            resolved.insert(tail.clone(), ca_ref.clone());
        }
    }
    substitute_resources(content, site, &resolved)
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
/// content-addressed URL from `resolved` (`host/path` tail → [`CaRef`]).
/// References absent from `resolved` (un-mirrored hotlinks) pass through
/// unchanged.
pub(super) fn substitute_resources(
    content: Content,
    site: &SafePathComponent,
    resolved: &HashMap<String, CaRef>,
) -> Content {
    let ca_for = |tail: &str| resolved.get(tail).map(|ca| ca_url(site, ca));
    let mut walk = |c: Content| substitute_resources(c, site, resolved);
    content
        .into_iter()
        .map(|node| match node {
            Node::Image {
                align,
                source,
                params,
            } => Node::Image {
                align,
                source: subst_source(source, ca_for),
                params,
            },
            Node::Link { target, text } => Node::Link {
                target: match target {
                    LinkTarget::Url(u) => match http_tail(&u, None).and_then(|t| ca_for(&t)) {
                        Some(ca) => LinkTarget::Url(ca),
                        None => LinkTarget::Url(u),
                    },
                    other => other,
                },
                text: walk(text),
            },
            Node::Stylesheet(css) => Node::Stylesheet(rewrite_with(&css, None, ca_for)),
            other => other.map_node(&mut walk),
        })
        .collect()
}

/// Rewrite a purely-literal image `source` (`[Plain(url)]`) to its CA URL when
/// `ca_for` resolves it; leave sources with variables or non-http URLs as-is.
fn subst_source<F: Fn(&str) -> Option<String>>(source: Vec<TextObj>, ca_for: F) -> Vec<TextObj> {
    if let Some(url) = TextObj::plain_concat(&source)
        && let Some(tail) = http_tail(&url, None)
        && let Some(ca) = ca_for(&tail)
    {
        return vec![TextObj::Plain(ca)];
    }
    source
}
