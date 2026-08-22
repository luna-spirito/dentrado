use super::*;
use std::cmp::Ordering;
use std::time::{SystemTime, UNIX_EPOCH};

// =========================================================================
// ListPages assembly (the template-instantiation half of `article_latest`)
// =========================================================================

/// The rendering page's own context: what context-sensitive ListPages
/// selectors (`category="."`, `range="."`) and `[[iftags]]` conditionals
/// resolve against.
pub(super) struct HostCtx {
    pub(super) fullname: String,
    pub(super) category: Option<String>,
    pub(super) tags: Vec<String>,
}

/// Resolve every `[[module ListPages]]` in `content`: each distinct module
/// selection (context selectors resolved against the rendering page) is
/// queried from the [`repo_l_list_pages`] lens — declared as a
/// [`secondary_get`](dentrado::core::gear::GearQuery::secondary_get)
/// dependency, so an edit that changes any selection re-runs this gear — and
/// every module node is replaced by its instantiated template: prepend, one
/// repeat per matching page with that page's `%%vars%%` bound, append,
/// wrapped in Wikidot's `list-pages-box` / `list-pages-item` containers.
pub(super) async fn resolve_listpages<S: Storage<KolorinkoRT>>(
    content: Content,
    state: &mut ResolveState,
    host: &HostCtx,
    ctx: &mut GearCtx<KolorinkoRT, S>,
) -> (Content, Vec<PageDep>) {
    let mut queries: Vec<ListPagesQuery> = Vec::new();
    collect_listpages_queries(&content, host, &mut queries);
    if queries.is_empty() {
        return (content, Vec::new());
    }
    let mut results: HashMap<ListPagesQuery, ListPagesResult> = HashMap::new();
    for query in &queries {
        let result =
            crate::runtime::repo_l_list_pages(state.site.clone(), query.clone())
                .secondary_get(ctx)
                .await;
        results.insert(query.clone(), (*result).clone());
    }
    // A template referencing `%%content%%` embeds each listed page's rendered
    // body, fetched and resolved below.
    let listed = resolve_content_bodies(&content, host, &results, state, ctx).await;
    (
        substitute_listpages(content, &results, &state.bodies, &state.site, host),
        listed,
    )
}

/// Fetch and fully resolve the body of every page a `%%content%%` template in
/// `content` needs, cached in `state.bodies` by fullname (so a page reached
/// from several modules or transclusions is resolved once per run), together
/// with each resolved page as a [`PageDep`] (its own resolution deps nested
/// under it). Each page is declared as an [`article_latest_parsed`]
/// `secondary_get` dependency (reactive to its edits) and run through
/// [`resolve_full`] in its own context; `state.resolved` — extended before
/// each resolution — is what stops a transclusion cycle (a listed page
/// embedding, transitively, a page already being resolved).
async fn resolve_content_bodies<S: Storage<KolorinkoRT>>(
    content: &Content,
    host: &HostCtx,
    results: &HashMap<ListPagesQuery, ListPagesResult>,
    state: &mut ResolveState,
    ctx: &mut GearCtx<KolorinkoRT, S>,
) -> Vec<PageDep> {
    let pages = content_needing_bodies(content, &state.site, host, results);
    let mut deps = Vec::new();
    for (key, slug, page) in pages {
        if state.bodies.contains_key(&page.fullname()) || !state.resolved.insert(key.clone()) {
            continue;
        }
        let parsed =
            crate::runtime::article_latest_parsed(state.site.clone(), slug.clone())
                .secondary_get(ctx)
                .await;
        let host = HostCtx {
            fullname: page.fullname(),
            category: page.category.clone(),
            tags: page.tags.clone(),
        };
        let (content, page_deps) =
            resolve_full(parsed.content.clone(), slug, host, state, ctx).await;
        state.bodies.insert(page.fullname(), content);
        deps.push(page_dep(&key, page_deps));
    }
    deps
}

/// Collect every listed page — as a resolution `(Key, Slug, ListedPage)` —
/// whose body a `%%content%%`-referencing template in `content` will need.
fn content_needing_bodies(
    content: &Content,
    site: &SafePathComponent,
    host: &HostCtx,
    results: &HashMap<ListPagesQuery, ListPagesResult>,
) -> Vec<(Key, Slug, ListedPage)> {
    let mut out = Vec::new();
    collect_content_bodies(content, site, host, results, &mut out);
    out
}

/// A template is checked at its own level only: a `%%content%%` inside a
/// nested `[[module ListPages]]` belongs to that nested module (resolved when
/// its template is spliced in), so the walker skips nested module templates.
fn collect_content_bodies(
    content: &Content,
    site: &SafePathComponent,
    host: &HostCtx,
    results: &HashMap<ListPagesQuery, ListPagesResult>,
    out: &mut Vec<(Key, Slug, ListedPage)>,
) {
    for node in content {
        match node {
            Node::ListPages(lp) => {
                if uses_content_var(&lp.repeat)
                    && let Some(result) = results.get(&resolve_query(&lp.params, host))
                {
                    for page in &result.pages {
                        if let Some(slug) = slug_of(page)
                            && let key = (site.clone(), slug.0.clone(), slug.1.clone())
                            && !out.iter().any(|(k, _, _)| *k == key)
                        {
                            out.push((key, slug, page.clone()));
                        }
                    }
                }
            }
            other => other.visit_node(&mut |c| collect_content_bodies(c, site, host, results, out)),
        }
    }
}

/// Does a template body reference `%%content%%` at its own level?
fn uses_content_var(content: &Content) -> bool {
    content.iter().any(|node| match node {
        Node::Text(TextObj::ModuleVar { name, .. }) => name == "content",
        // A nested module's `%%content%%` belongs to that module.
        Node::ListPages(_) => false,
        other => {
            let mut hit = false;
            other.visit_node(&mut |c| hit |= uses_content_var(c));
            hit
        }
    })
}

/// The `(category, name)` slug of a listed page, or `None` if either part is
/// not a valid path component (shouldn't happen for pages that came out of the
/// repo, but guards against malformed data).
fn slug_of(page: &ListedPage) -> Option<Slug> {
    Some((
        page.category
            .as_ref()
            .and_then(|c| SafePathComponent::new(c.clone())),
        SafePathComponent::new(page.name.clone())?,
    ))
}

/// Walk `content` and record every distinct ListPages query it contains —
/// including modules nested inside another module's template body, which the
/// substitution pass resolves when it splices that template in.
fn collect_listpages_queries(content: &Content, host: &HostCtx, out: &mut Vec<ListPagesQuery>) {
    for node in content {
        match node {
            Node::ListPages(lp) => {
                let query = resolve_query(&lp.params, host);
                if !out.contains(&query) {
                    out.push(query);
                }
                collect_listpages_queries(&lp.prepend, host, out);
                collect_listpages_queries(&lp.repeat, host, out);
                collect_listpages_queries(&lp.append, host, out);
            }
            other => other.visit_node(&mut |c| collect_listpages_queries(c, host, out)),
        }
    }
}

/// Replace every ListPages module in `content` by its instantiation from
/// `results`, recursing into the spliced-in template so a module nested in
/// another module's template body is resolved too.
fn substitute_listpages(
    content: Content,
    results: &HashMap<ListPagesQuery, ListPagesResult>,
    bodies: &HashMap<String, Content>,
    site: &SafePathComponent,
    host: &HostCtx,
) -> Content {
    let mut walk = |c: Content| substitute_listpages(c, results, bodies, site, host);
    content
        .into_iter()
        .flat_map(|node| match node {
            Node::ListPages(lp) => match results.get(&resolve_query(&lp.params, host)) {
                Some(result) => walk(instantiate(&lp, result, bodies, site, host)),
                None => vec![Node::ListPages(lp)],
            },
            other => vec![other.map_node(&mut walk)],
        })
        .collect()
}

/// Finalize the query for one module against the rendering page: `category`
/// `.` (and the default) is the current category, `name` `.` (or its
/// documented twin `=`) the current page's name, and `range="."` (parsed as
/// `fullname="="`) selects the current page.
fn resolve_query(params: &ListPagesParams, host: &HostCtx) -> ListPagesQuery {
    let mut p = params.clone();
    p.category = Some(match p.category.as_deref() {
        None | Some(".") => host
            .category
            .clone()
            .unwrap_or_else(|| "_default".to_string()),
        Some(other) => other.to_string(),
    });
    if p.fullname.as_deref() == Some("=") {
        p.fullname = Some(host.fullname.clone());
    }
    if p.name.as_deref().is_some_and(|n| n == "." || n == "=") {
        p.name = Some(
            host.fullname
                .rsplit_once(':')
                .map_or(host.fullname.clone(), |(_, n)| n.to_string()),
        );
    }
    ListPagesQuery(p)
}

/// Instantiate one resolved module: prepend and append render once (only in
/// the `separate="no"` mode Wikidot restricts them to), the repeat body once
/// per matching page with that page's `%%vars%%` bound and its `[[iftags]]`
/// evaluated against its own tags (`separate="no"` compiles all items
/// together with the rendering page's tags instead).
fn instantiate(
    lp: &ListPages,
    result: &ListPagesResult,
    bodies: &HashMap<String, Content>,
    site: &SafePathComponent,
    host: &HostCtx,
) -> Content {
    let ListPagesParams {
        separate,
        wrapper,
        offset,
        limit,
        ..
    } = &lp.params;
    let edge_vars = Vars::module(site, None, None, 0, result.total, *limit);
    let mut out = Content::new();
    if !separate {
        out.extend(apply_vars(lp.prepend.clone(), &edge_vars));
    }
    for (i, page) in result.pages.iter().enumerate() {
        let vars = Vars::module(
            site,
            Some(page),
            bodies.get(&page.fullname()),
            i as i64 + offset.unwrap_or(0) + 1,
            result.total,
            *limit,
        );
        let item = apply_vars(lp.repeat.clone(), &vars);
        let tags = if *separate {
            page.tags.as_slice()
        } else {
            host.tags.as_slice()
        };
        let item = evaluate_iftags(item, tags);
        if *separate {
            out.push(class_div("list-pages-item", item));
        } else {
            out.extend(item);
        }
    }
    if !separate {
        out.extend(apply_vars(lp.append.clone(), &edge_vars));
    }
    if *wrapper {
        vec![class_div("list-pages-box", out)]
    } else {
        out
    }
}

/// A bare `<div class="…">` container.
fn class_div(class: &str, content: Content) -> Node {
    Node::Container {
        kind: ContainerKind::Div {
            inline: false,
            block: true,
            params: [("class".to_string(), vec![TextObj::Plain(class.to_string())])].into(),
        },
        content,
    }
}

// =========================================================================
// ListPages selection (the `repo_l_list_pages` lens core)
// =========================================================================

/// Project a resolved [`ListPagesQuery`] over one site: filter the site's pages
/// by the selection parameters, order them, and truncate to the first
/// pagination page. `%%total%%` counts the matches before truncation.
pub(super) fn select(w: &WDWebsite, params: &ListPagesParams) -> ListPagesResult {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    let mut pages: Vec<ListedPage> = w
        .articles
        .iter()
        .flat_map(|(cat, by_name)| by_name.iter().map(move |(name, a)| (cat, name, a)))
        .filter(|&(_, _, a)| matches_page(a, params, now))
        .map(|(cat, name, a)| listed_page(cat, name, a))
        .collect();
    pages.sort_by(|a, b| cmp_order(&params.order, a, b));
    let total = pages.len() as i64;
    let start = params.offset.unwrap_or(0).clamp(0, total) as usize;
    pages.drain(..start);
    let cap = params
        .limit
        .unwrap_or(i64::MAX)
        .min(params.per_page.unwrap_or(20))
        .clamp(0, i64::MAX) as usize;
    pages.truncate(cap);
    ListPagesResult { pages, total }
}

/// One matching page as the template-visible [`ListedPage`]: creation/update
/// are the first/last revision by number.
fn listed_page(
    cat: &Option<SafePathComponent>,
    name: &SafePathComponent,
    a: &Article,
) -> ListedPage {
    let (created, updated) = first_last_revisions(a);
    ListedPage {
        name: (**name).clone(),
        category: cat.as_ref().map(|c| (**c).clone()),
        title: a.meta.title.clone(),
        tags: a.meta.tags.clone(),
        created_by: created.map_or_else(String::new, |r| r.author.clone()),
        created_at: created.map_or(0, |r| r.timestamp),
        updated_by: updated.map_or_else(String::new, |r| r.author.clone()),
        updated_at: updated.map_or(0, |r| r.timestamp),
        revisions: a.revisions.len() as u64,
    }
}

/// The first and last revision of a page (by revision number, in case the
/// `_meta` rows are unordered).
fn first_last_revisions(a: &Article) -> (Option<&RevMeta>, Option<&RevMeta>) {
    let mut first: Option<&RevMeta> = None;
    let mut last: Option<&RevMeta> = None;
    for r in &a.revisions {
        if first.is_none_or(|f| r.revision < f.revision) {
            first = Some(r);
        }
        if last.is_none_or(|l| r.revision >= l.revision) {
            last = Some(r);
        }
    }
    (first, last)
}

/// Does one page pass every supported selector? Unsupported selectors with no
/// data behind them (`rating`, `votes`, `parent`, `link_to`, …) are not
/// filtered at all.
fn matches_page(a: &Article, params: &ListPagesParams, now: i64) -> bool {
    let name = a.meta.slug.rsplit(':').next().unwrap_or(&a.meta.slug);
    match params.pagetype.as_deref() {
        Some("hidden") if !name.starts_with('_') => return false,
        Some("*") => {}
        _ if name.starts_with('_') => return false,
        _ => {}
    }
    if !matches_category(a, params.category.as_deref()) {
        return false;
    }
    if let Some(fullname) = &params.fullname
        && !fullname.eq_ignore_ascii_case(&a.meta.slug)
    {
        return false;
    }
    if let Some(sel) = &params.name
        && !sel.eq_ignore_ascii_case(name)
    {
        return false;
    }
    if let Some(by) = &params.created_by
        && !by.eq_ignore_ascii_case(author_of(a, End::First))
    {
        return false;
    }
    if let Some(filter) = &params.created_at
        && !time_matches(filter, timestamp_of(a, End::First), now)
    {
        return false;
    }
    if let Some(filter) = &params.updated_at
        && !time_matches(filter, timestamp_of(a, End::Last), now)
    {
        return false;
    }
    let Some(tags) = &params.tags else {
        return true;
    };
    if tags.no_tags {
        return a.meta.tags.is_empty();
    }
    let has = |t: &str| a.meta.tags.iter().any(|pt| pt.eq_ignore_ascii_case(t));
    tags.all.iter().all(|t| has(t))
        && (tags.any.is_empty() || tags.any.iter().any(|t| has(t)))
        && tags.none.iter().all(|t| !has(t))
}

#[derive(Copy, Clone)]
enum End {
    First,
    Last,
}

fn author_of(a: &Article, end: End) -> &str {
    let (f, l) = first_last_revisions(a);
    match end {
        End::First => f.map_or("", |r| &r.author),
        End::Last => l.map_or("", |r| &r.author),
    }
}

fn timestamp_of(a: &Article, end: End) -> i64 {
    let (f, l) = first_last_revisions(a);
    match end {
        End::First => f.map_or(0, |r| r.timestamp),
        End::Last => l.map_or(0, |r| r.timestamp),
    }
}

/// A `category="…"` selector: `"*"` (or `None`, once resolved) keeps every
/// category; otherwise a token list where `-cat` excludes and plain cats are
/// additive (OR). `_default` is the Wikidot name of the root (no-category)
/// pages.
fn matches_category(a: &Article, selector: Option<&str>) -> bool {
    let Some(selector) = selector.filter(|s| !s.is_empty()) else {
        return true;
    };
    if selector == "*" {
        return true;
    }
    let slug_cat = a.meta.slug.split_once(':').map_or("_default", |(c, _)| c);
    let mut included = false;
    for token in selector.split([',', ' ']).filter(|t| !t.is_empty()) {
        match token.strip_prefix('-') {
            Some(cat) if cat.eq_ignore_ascii_case(slug_cat) => return false,
            Some(_) => {}
            None if token.eq_ignore_ascii_case(slug_cat) => included = true,
            None => {}
        }
    }
    included
}

/// A relative (`last n unit` / `older than n unit`) or absolute time filter
/// against a page timestamp.
fn time_matches(filter: &TimeFilter, ts: i64, now: i64) -> bool {
    match *filter {
        TimeFilter::Last(secs) => ts >= now - secs,
        TimeFilter::OlderThan(secs) => ts < now - secs,
        TimeFilter::Before(t) => ts < t,
        TimeFilter::After(t) => ts > t,
        TimeFilter::Between(a, b) => ts >= a && ts < b,
    }
}

/// Order two matched pages. Unsupported keys (`rating`, `votes`, `size`,
/// `random`, data-form fields) and an absent `order` fall back to Wikidot's
/// default: `created_at desc`.
fn cmp_order(order: &Option<ListOrder>, a: &ListedPage, b: &ListedPage) -> Ordering {
    let Some(ListOrder { by, ascending }) = order else {
        return b.created_at.cmp(&a.created_at);
    };
    let cmp = match by.as_str() {
        "name" => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
        "fullname" => {
            (a.category.as_deref(), a.name.as_str()).cmp(&(b.category.as_deref(), b.name.as_str()))
        }
        "title" => a.title.to_lowercase().cmp(&b.title.to_lowercase()),
        "created_by" => a.created_by.cmp(&b.created_by),
        "created_at" => a.created_at.cmp(&b.created_at),
        "updated_at" => a.updated_at.cmp(&b.updated_at),
        "revisions" => a.revisions.cmp(&b.revisions),
        _ => return b.created_at.cmp(&a.created_at),
    };
    if *ascending { cmp } else { cmp.reverse() }
}
