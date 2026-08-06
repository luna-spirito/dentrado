//! The Wikidot-export data layer and the four gears built on top of it.
//!
//! The export repository layout (one git clone mirroring many sites) is:
//! ```text
//! <site>/_meta/<p1>/<p2>/<pageid>                ← page metadata + revision table
//! <site>/_pages_by_id/<p1>/<p2>/<pageid>/r{N}.txt ← revision bodies (frontmatter + text)
//! <site>/pages/…                                  ← human-readable symlinks (ignored here)
//! <site>/files/…                                  ← attachments (not yet served)
//! ```
//! `<p1>/<p2>/<pageid>` is the page id split as 2/2/rest (e.g. id `1305054470`
//! → `13/05/054470`); the `_meta` and `_pages_by_id` subtrees share that exact
//! suffix, so a `_meta` path maps to its bodies directory by swapping the top
//! segment. The `_meta` file holds `slug`/`title`/`tags` header lines followed
//! by one TAB-separated `revision  revision_id  timestamp  author` row per
//! revision.
//!
//! # Gears
//! - [`repo`] (`local` oracle): polls the git remote on a timer and rebuilds
//!   the whole in-memory dataset as [`Rc`]`<`[`RepoData`]`>`. Pinned to one
//!   core; never crosses a thread, so it may (and does) hold `Rc`/`!Send` data.
//! - [`repo_l_article_latest`] (`follow` lens over `repo`): projects one page
//!   into an owned [`ArticleLatest`] (metadata + latest body + revision list,
//!   no bodies). Shippable, so `Send` (owned `String`s, no `Rc`/`Arc`).
//! - [`article_latest_parsed`] (`event`): parses the latest body into
//!   [`ArticleView`] with `[[include]]` directives **left unresolved**. Kept
//!   separate from [`article_latest`] so the parse gears never depend on one
//!   another (which would let two pages that include each other form a gear
//!   cycle).
//! - [`article_latest`] (`event`): resolves every `[[include]]` by
//!   [`secondary_get`](dentrado::core::gear::GearQuery::secondary_get)-ing
//!   [`article_latest_parsed`] of the included pages (data-level cycles broken
//!   by a visited-set), producing the final [`ArticleView`]. Declaring each
//!   include as a dependency makes the result reactive: an edit to any page in
//!   the transitive include cone re-runs this gear.

use crate::{
    safe_path::SafePathComponent,
    wikidot_parser::parse,
};
use dentrado::core::{core_ctx::GearCtx, storage::Storage};
use git2::{Oid, Repository};
use im::HashMap as ImHashMap;
use kolorinko_wikitext::{ArticleMeta, ArticleView, Content, Node, PageRef, RevMeta};
use log::error;
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    rc::Rc,
};

use crate::runtime::KolorinkoRT;

// =========================================================================
// Configuration
// =========================================================================

/// Configuration for the [`repo`] oracle gear: where to clone and how often to
/// re-pull. Holds `&'static` fields because it is part of a [`GearId`](crate::runtime::...)
/// identity (which is `'static`); a runtime path from a config file is leaked
/// once at startup with `Box::leak`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, dentrado::types::Localizable)]
pub(crate) struct RepoMeta {
    url: &'static str,
    path: &'static Path,
    interval: u32,
}

impl RepoMeta {
    #[must_use]
    pub(crate) const fn new(url: &'static str, path: &'static Path, interval: u32) -> Self {
        Self { url, path, interval }
    }

    #[must_use]
    pub(crate) const fn interval(&self) -> u32 {
        self.interval
    }
}

// =========================================================================
// Dataset
// =========================================================================

/// `(Option<category>, name)` — the per-site page address. `None` category =
/// a root page (slug has no `:`).
type Slug = (Option<SafePathComponent>, SafePathComponent);

/// `(site, Option<category>, name)` — the full address of a page within the
/// dataset. Used both as the include-resolution visited key and as the
/// incremental-update reverse-index value (`_meta` path → its nested-map key).
type Key = (SafePathComponent, Option<SafePathComponent>, SafePathComponent);

/// All sites mirrored out of the repository at one point in time. A persistent
/// [`im::HashMap`] so cloning the [`Rc`]`<RepoData>` is O(1) and an update is
/// non-destructive (dependents holding a prior snapshot see a stable view).
#[derive(Default, Clone, Debug)]
pub(crate) struct RepoData {
    sites: ImHashMap<SafePathComponent, WDWebsite>,
}

impl RepoData {
    /// Look up one page by `(site, slug)`.
    #[must_use]
    fn article(&self, site: &SafePathComponent, slug: &Slug) -> Option<&Article> {
        self.sites
            .get(site)?
            .articles
            .get(&slug.0)?
            .get(&slug.1)
    }
}

/// One mirrored site: its pages nested by category.
#[derive(Default, Clone, Debug)]
pub(crate) struct WDWebsite {
    articles: ImHashMap<Option<SafePathComponent>, ImHashMap<SafePathComponent, Article>>,
}

/// One page: metadata, the full revision-history summary, the latest revision's
/// body, and **every** revision body (loaded eagerly per the design so the
/// postponed revision gear can project any revision as a trivial lens).
#[derive(Clone, Debug)]
pub(crate) struct Article {
    meta: ArticleMeta,
    latest_body: Rc<str>,
    revisions: Vec<RevMeta>,
    /// Every revision body. Loaded eagerly (per design) but only read by the
    /// postponed `repo_l_article_revision` gear.
    #[allow(dead_code)]
    bodies: ImHashMap<u64, Rc<str>>,
}

/// Shippable projection of one page: metadata, the latest revision's raw body,
/// and the revision-history summary (no bodies). Owned `String`s — no `Rc`/`Arc`
/// — because it crosses cores.
#[derive(Clone, Debug, Default)]
pub(crate) struct ArticleLatest {
    pub(crate) meta: ArticleMeta,
    pub(crate) body: String,
    pub(crate) revisions: Vec<RevMeta>,
}

// =========================================================================
// `repo` oracle gear
// =========================================================================

/// Reverse index used by [`incremental_update`]: each `_meta` file path → its
/// nested-map [`Key`]. Kept across ticks so a moved tip can patch only the
/// pages the git diff touched, locating the old key to remove when a page's
/// slug changed or it was deleted.
type Index = HashMap<PathBuf, Key>;

/// Per-instance cache for [`repo`]: the opened `git2::Repository` (kept across
/// ticks), the last commit tip, the last-built dataset, and the reverse
/// [`Index`]. Wrapped in `Rc<RefCell<…>>` so the cache (which must be
/// `Clone + Debug`) is a cheap refcount bump and need not require
/// `Repository: Debug`.
#[derive(Default, Clone)]
pub(crate) struct RepoCache(Rc<RefCell<RepoInner>>);

#[derive(Default)]
struct RepoInner {
    repo: Option<Repository>,
    last_tip: Option<Oid>,
    data: Option<Rc<RepoData>>,
    index: Index,
}

impl std::fmt::Debug for RepoCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("RepoCache").finish()
    }
}

/// Run the [`repo`] oracle. On a tick: open/clone lazily, fetch + hard-reset,
/// and on a moved tip patch the dataset **incrementally** — only the pages the
/// `old_tip → new_tip` git diff touched are re-read, producing a new
/// [`Rc`]`<`[`RepoData`]`>` that structurally shares almost all of the old one
/// (and reuses the unaffected pages' `Rc<str>` bodies). Falls back to a full
/// [`build_all`] on the first build or when the diff can't be computed (e.g.
/// a force-push garbage-collected the old tip). A same-tip or non-tick run
/// returns the previously built dataset unchanged.
pub(crate) fn repo(meta: &RepoMeta, tick: bool, cache: &mut RepoCache) -> Rc<RepoData> {
    let mut inner = cache.0.borrow_mut();
    if inner.repo.is_none() {
        inner.repo = open_or_clone(meta.url, meta.path);
    }
    if tick
        && let Some((r, outcome)) = inner.repo.as_ref().zip(pull_for_diff(inner.repo.as_ref()))
        && let PullOutcome::Updated { new_tip } = outcome
    {
        let prev_tip = inner.last_tip;
        let prev_data = inner.data.clone();
        let rebuilt: Option<(RepoData, Index)> = match (prev_tip, prev_data.as_ref()) {
            (Some(old_tip), Some(old_data)) if old_tip != new_tip => {
                match diff_affected_meta_paths(r, old_tip, new_tip, meta.path) {
                    Some(affected) if affected.is_empty() => None,
                    Some(affected) => {
                        let mut index = inner.index.clone();
                        let data = incremental_update(old_data, &mut index, affected);
                        Some((data, index))
                    }
                    None => Some(build_all(meta.path)),
                }
            }
            _ => Some(build_all(meta.path)),
        };
        inner.last_tip = Some(new_tip);
        if let Some((data, index)) = rebuilt {
            inner.data = Some(Rc::new(data));
            inner.index = index;
        }
    }
    if inner.data.is_none() {
        let (data, index) = build_all(meta.path);
        inner.last_tip = current_tip(inner.repo.as_ref());
        inner.data = Some(Rc::new(data));
        inner.index = index;
    }
    Rc::clone(inner.data.as_ref().expect("dataset populated above"))
}

/// The outcome of a `git fetch` + hard-reset: the tip either moved or not.
enum PullOutcome {
    SameTip,
    Updated { new_tip: Oid },
}

/// Fetch from `origin` (force-updating local branches) and hard-reset the
/// working tree. Returns the new tip classified against the previous one so the
/// caller can skip a rebuild when nothing changed. `None` on fetch failure
/// (logged); the caller keeps serving the last good dataset.
fn pull_for_diff(repo: Option<&Repository>) -> Option<PullOutcome> {
    let repo = repo?;
    let old_tip = current_tip(Some(repo));
    match try_pull(repo) {
        Ok(new_tip) => Some(if Some(new_tip) == old_tip {
            PullOutcome::SameTip
        } else {
            PullOutcome::Updated { new_tip }
        }),
        Err(e) => {
            error!("Failed to pull the repository: {e}");
            None
        }
    }
}

fn current_tip(repo: Option<&Repository>) -> Option<Oid> {
    repo?.head().ok().and_then(|r| r.target())
}

fn try_pull(repo: &Repository) -> Result<Oid, git2::Error> {
    let mut remote = repo.find_remote("origin")?;
    remote.fetch(&["+refs/heads/*:refs/heads/*"], None, None)?;
    let fetched = repo.revparse_single("FETCH_HEAD")?;
    let new_tip = fetched.id();
    repo.reset(&fetched, git2::ResetType::Hard, None)?;
    Ok(new_tip)
}

fn open_or_clone(url: &str, path: &Path) -> Option<Repository> {
    match Repository::open(path) {
        Ok(r) => Some(r),
        Err(_) => match Repository::clone(url, path) {
            Ok(r) => Some(r),
            Err(e) => {
                error!("Failed to clone {url}: {e}");
                None
            }
        },
    }
}

// =========================================================================
// Repository walk → RepoData
// =========================================================================

/// Walk the whole working tree and build a [`RepoData`]: for each site, every
/// `_meta/<p1>/<p2>/<pageid>` file yields one [`Article`] (metadata from the
/// file, bodies from the sibling `_pages_by_id` directory).
fn build_all(root: &Path) -> (RepoData, Index) {
    let mut sites = ImHashMap::new();
    let mut index: Index = HashMap::new();
    let Ok(site_entries) = fs::read_dir(root) else {
        return (RepoData::default(), index);
    };
    for site_entry in site_entries.flatten() {
        let site_path = site_entry.path();
        if !site_path.is_dir() {
            continue;
        }
        let Some(site) = SafePathComponent::new(site_entry.file_name().to_string_lossy().into())
        else {
            continue;
        };
        let mut articles: ImHashMap<Option<SafePathComponent>, ImHashMap<SafePathComponent, Article>> =
            ImHashMap::new();
        for meta_file in walk_files(&site_path.join("_meta")) {
            let Some(article) = build_article(&meta_file, &site_path) else {
                continue;
            };
            let Some((category, name)) = slug_to_key(&article.meta.slug) else {
                continue;
            };
            index.insert(
                meta_file.clone(),
                (site.clone(), category.clone(), name.clone()),
            );
            articles
                .entry(category)
                .or_insert_with(ImHashMap::new)
                .insert(name, article);
        }
        if !articles.is_empty() {
            sites.insert(site, WDWebsite { articles });
        }
    }
    (RepoData { sites }, index)
}

/// Build one [`Article`] from its `_meta` file: parse the metadata, derive
/// `(category, name)` from the slug, and read every revision body from the
/// matching `_pages_by_id` directory.
fn build_article(meta_file: &Path, site_path: &Path) -> Option<Article> {
    let text = fs::read_to_string(meta_file).ok()?;
    let pm = parse_meta(&text);
    // `_meta/<p1>/<p2>/<id>` → `_pages_by_id/<p1>/<p2>/<id>` (same suffix).
    let rel = meta_file.strip_prefix(site_path).ok()?;
    let bodies_dir = site_path
        .join("_pages_by_id")
        .join(rel.strip_prefix("_meta").ok()?);
    let bodies = read_bodies(&bodies_dir);
    let latest = *bodies.keys().max()?;
    let latest_body = bodies.get(&latest)?.clone();
    // page id = the joined `<p1><p2><id>` tail of the `_meta` path.
    let page_id: String = rel
        .components()
        .skip(1)
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    let meta = ArticleMeta {
        title: pm.title,
        tags: pm.tags,
        slug: pm.slug,
        page_id,
    };
    Some(Article {
        meta,
        latest_body,
        revisions: pm.revisions,
        bodies,
    })
}

/// Every file reachable below `dir`, recursively (used to enumerate `_meta`
/// page-id files at arbitrary nesting).
fn walk_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    walk_into(dir, &mut out);
    out
}

fn walk_into(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk_into(&path, out);
        } else {
            out.push(path);
        }
    }
}

/// Read every `r{N}.txt` in `dir` into `{N → body}` (frontmatter stripped).
fn read_bodies(dir: &Path) -> ImHashMap<u64, Rc<str>> {
    let mut map = ImHashMap::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return map;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some(num) = name.strip_prefix('r').and_then(|s| s.strip_suffix(".txt")) else {
            continue;
        };
        let Ok(n) = num.parse::<u64>() else { continue };
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        map.insert(n, Rc::from(revision_body(&text)));
    }
    map
}

/// Strip a revision file's `---\n…\n---\n` frontmatter, returning the body.
/// If the frontmatter is absent the whole text is the body.
fn revision_body(text: &str) -> &str {
    let Some(rest) = text.strip_prefix("---\n") else {
        return text;
    };
    let Some(end) = rest.find("\n---\n") else {
        return text;
    };
    &rest[end + "\n---\n".len()..]
}

struct ParsedMeta {
    slug: String,
    title: String,
    tags: Vec<String>,
    revisions: Vec<RevMeta>,
}

/// Parse a `_meta` file: `slug`/`title`/`tags` header lines plus
/// TAB-separated `revision  revision_id  timestamp  author` rows. Header and
/// revision lines may appear in any order (a line with a TAB is a revision
/// row; a `key: value` line is a header).
fn parse_meta(text: &str) -> ParsedMeta {
    let mut slug = String::new();
    let mut title = String::new();
    let mut tags = Vec::new();
    let mut revisions = Vec::new();
    for line in text.lines() {
        if line.contains('\t') {
            let mut f = line.split('\t');
            if let (Some(r), Some(rid), Some(ts), Some(a)) =
                (f.next(), f.next(), f.next(), f.next())
            {
                if let (Ok(rev), Ok(timestamp)) =
                    (r.trim().parse::<u64>(), ts.trim().parse::<i64>())
                {
                    revisions.push(RevMeta {
                        revision: rev,
                        revision_id: rid.trim().to_string(),
                        timestamp,
                        author: a.trim().to_string(),
                    });
                }
            }
        } else if let Some((k, v)) = line.split_once(':') {
            match k.trim() {
                "slug" => slug = strip_quotes(v.trim()).to_string(),
                "title" => title = strip_quotes(v.trim()).to_string(),
                "tags" => tags = serde_json::from_str(v.trim()).unwrap_or_default(),
                _ => {}
            }
        }
    }
    ParsedMeta { slug, title, tags, revisions }
}

/// Split a canonical slug into `(Option<category>, name)`: `help:foo` →
/// `(Some("help"), "foo")`, `foo` → `(None, "foo")`.
fn slug_parts(slug: &str) -> (Option<String>, String) {
    match slug.split_once(':') {
        Some((cat, name)) => (Some(cat.to_string()), name.to_string()),
        None => (None, slug.to_string()),
    }
}

/// `(Option<category>, name)` as validated [`SafePathComponent`]s, or `None` if
/// either segment is unsafe (the page is dropped, as in [`build_all`]).
fn slug_to_key(slug: &str) -> Option<(Option<SafePathComponent>, SafePathComponent)> {
    let (cat, name) = slug_parts(slug);
    let name = SafePathComponent::new(name)?;
    let cat = match cat {
        None => None,
        Some(c) => Some(SafePathComponent::new(c)?),
    };
    Some((cat, name))
}

// =========================================================================
// Incremental update
// =========================================================================

/// Patch [`old`] for exactly the pages in [`affected`] (each an absolute
/// `_meta` path). For each: drop the old nested-map entry (if any, via the
/// [`Index`]), then re-read the page and re-insert under its current slug.
/// Unaffected pages are structurally shared from [`old`] (`im::HashMap`), so
/// only the touched pages' files are re-read.
fn incremental_update(old: &RepoData, index: &mut Index, affected: HashSet<PathBuf>) -> RepoData {
    let mut sites = old.sites.clone();
    for meta_path in affected {
        if let Some(old_key) = index.remove(&meta_path) {
            remove_page(&mut sites, &old_key);
        }
        let Some(site_dir) = meta_path_site_dir(&meta_path) else { continue };
        let Some(article) = build_article(&meta_path, &site_dir) else { continue };
        let Some(site) = meta_path_site(&meta_path) else { continue };
        let Some((cat, name)) = slug_to_key(&article.meta.slug) else { continue };
        index.insert(meta_path, (site.clone(), cat.clone(), name.clone()));
        insert_page(&mut sites, site, cat, name, article);
    }
    RepoData { sites }
}

/// The set of `_meta` paths changed between two tips. Each git-diff delta path
/// (old and new side) is normalized via [`normalize_meta_path`]; non-page paths
/// (e.g. `files/…`, top-level docs) are dropped. `None` if either tree is
/// unreachable (force-push GC of the old tip) — the caller falls back to
/// [`build_all`].
fn diff_affected_meta_paths(
    repo: &Repository,
    old_tip: Oid,
    new_tip: Oid,
    root: &Path,
) -> Option<HashSet<PathBuf>> {
    let old_tree = repo.find_commit(old_tip).ok()?.tree().ok()?;
    let new_tree = repo.find_commit(new_tip).ok()?.tree().ok()?;
    let diff = repo
        .diff_tree_to_tree(Some(&old_tree), Some(&new_tree), None)
        .ok()?;
    let mut affected = HashSet::new();
    for delta in diff.deltas() {
        if let Some(rel) = delta.old_file().path() {
            if let Some(mp) = normalize_meta_path(rel, root) {
                affected.insert(mp);
            }
        }
        if let Some(rel) = delta.new_file().path() {
            if let Some(mp) = normalize_meta_path(rel, root) {
                affected.insert(mp);
            }
        }
    }
    Some(affected)
}

/// Collapse a repo-relative delta path to its absolute `_meta` file path:
/// `<site>/_meta/<p1>/<p2>/<id>` stays as-is; `<site>/_pages_by_id/<p1>/<p2>/<id>/rN.txt`
/// swaps `_pages_by_id` for `_meta` and drops the `rN.txt` leaf. The first three
/// components after the kind are the `p1/p2/<id>` shard — `take(3)` handles
/// both shapes uniformly. `None` for anything that isn't a page file.
fn normalize_meta_path(rel: &Path, root: &Path) -> Option<PathBuf> {
    let mut comps = rel.components();
    let site = comps.next()?.as_os_str().to_str()?.to_string();
    let kind = comps.next()?.as_os_str().to_str()?;
    if kind != "_meta" && kind != "_pages_by_id" {
        return None;
    }
    let parts: Vec<std::ffi::OsString> = (0..3)
        .filter_map(|_| comps.next().map(|c| c.as_os_str().to_owned()))
        .collect();
    if parts.len() != 3 {
        return None;
    }
    let mut meta = root.join(site).join("_meta");
    for p in parts {
        meta.push(p);
    }
    Some(meta)
}

/// `<root>/<site>/_meta/<p1>/<p2>/<id>` → `<root>/<site>` (the site dir), via
/// the 4 ancestors above the file. Used as `build_article`'s site root.
fn meta_path_site_dir(meta_path: &Path) -> Option<PathBuf> {
    meta_path.ancestors().nth(4).map(Path::to_path_buf)
}

/// The site [`SafePathComponent`] of a `_meta` path (the dir name 4 levels up).
fn meta_path_site(meta_path: &Path) -> Option<SafePathComponent> {
    let site_dir = meta_path.ancestors().nth(4)?;
    SafePathComponent::new(site_dir.file_name()?.to_string_lossy().into())
}

/// Remove `(site, cat, name)` from the nested map, pruning a now-empty
/// category or site. Each level is cloned once (`im::HashMap` is O(1)), so this
/// is O(log n) and shares the rest of the structure.
fn remove_page(sites: &mut ImHashMap<SafePathComponent, WDWebsite>, (site, cat, name): &Key) {
    let Some(mut website) = sites.get(site).cloned() else {
        return;
    };
    if let Some(mut cat_map) = website.articles.get(cat).cloned() {
        cat_map.remove(name);
        if cat_map.is_empty() {
            website.articles.remove(cat);
        } else {
            website.articles.insert(cat.clone(), cat_map);
        }
    }
    if website.articles.is_empty() {
        sites.remove(site);
    } else {
        sites.insert(site.clone(), website);
    }
}

/// Insert an [`Article`] under `(site, cat, name)`, creating the site/category
/// levels as needed.
fn insert_page(
    sites: &mut ImHashMap<SafePathComponent, WDWebsite>,
    site: SafePathComponent,
    cat: Option<SafePathComponent>,
    name: SafePathComponent,
    article: Article,
) {
    let mut website = sites.get(&site).cloned().unwrap_or_default();
    let mut cat_map = website.articles.get(&cat).cloned().unwrap_or_default();
    cat_map.insert(name, article);
    website.articles.insert(cat, cat_map);
    sites.insert(site, website);
}

/// Strip one layer of surrounding double quotes, if present.
fn strip_quotes(s: &str) -> &str {
    if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

// =========================================================================
// `repo_l_article_latest` lens
// =========================================================================

/// Trivial lens cache: the lens is a pure projection of `repo`, recomputed on
/// every `follow` kick.
#[derive(Default, Clone, Debug)]
pub(crate) struct RepoLArticleCache;

/// Project one page out of `repo`'s dataset into a shippable [`ArticleLatest`].
/// A missing page yields an empty [`ArticleLatest`] (the page renders blank).
pub(crate) fn repo_l_article_latest(
    data: &RepoData,
    site: &SafePathComponent,
    slug: &Slug,
) -> ArticleLatest {
    data.article(site, slug)
        .map(|a| ArticleLatest {
            meta: a.meta.clone(),
            body: (*a.latest_body).to_string(),
            revisions: a.revisions.clone(),
        })
        .unwrap_or_default()
}

// =========================================================================
// `article_latest_parsed` gear
// =========================================================================

/// Cache for [`article_latest_parsed`]: the last body string and the
/// [`ArticleView`] parsed from it. Because the lens hands back a fresh `String`
/// each kick, an unchanged page is recognised by body equality and its cached
/// parse reused — only a genuinely-changed page is re-parsed.
#[derive(Default, Clone, Debug)]
pub(crate) struct ParsedCache {
    body: Option<String>,
    view: Option<ArticleView>,
}

/// Parse a page's latest body into an [`ArticleView`] **without** resolving
/// `[[include]]` directives. Depends only on the [`repo_l_article_latest`] lens
/// (never on another parse gear), so the parse layer is acyclic.
pub(crate) async fn article_latest_parsed<S: Storage<KolorinkoRT>>(
    meta: &RepoMeta,
    site: &SafePathComponent,
    slug: &Slug,
    ctx: &mut GearCtx<KolorinkoRT, S>,
    cache: &mut ParsedCache,
) -> ArticleView {
    let latest = crate::runtime::repo_l_article_latest(meta.clone(), site.clone(), slug.clone())
        .secondary_get(ctx)
        .await;
    if cache.body.as_deref() == Some(latest.body.as_str())
        && let Some(view) = &cache.view
    {
        return view.clone();
    }
    let view = ArticleView {
        meta: latest.meta.clone(),
        revisions: latest.revisions.clone(),
        content: parse(&latest.body),
    };
    *cache = ParsedCache {
        body: Some(latest.body),
        view: Some(view.clone()),
    };
    view
}

// =========================================================================
// `article_latest` gear — include resolution
// =========================================================================

/// No carry-over state: the result is fully re-derived each run from the parse
/// gears it depends on (which the framework re-runs on any change).
#[derive(Default, Clone, Debug)]
pub(crate) struct LatestCache;

/// Render a page's final [`ArticleView`] by resolving every `[[include]]`:
/// each included page's [`article_latest_parsed`] output is fetched and spliced
/// in place of the directive, recursively, with a visited-set to break
/// data-level cycles (A includes B includes A). Declaring each include as a
/// [`secondary_get`](dentrado::core::gear::GearQuery::secondary_get) dependency
/// makes the whole result reactive — an edit anywhere in the include cone
/// re-runs this gear.
pub(crate) async fn article_latest<S: Storage<KolorinkoRT>>(
    meta: &RepoMeta,
    site: SafePathComponent,
    slug: Slug,
    parsed: ArticleView,
    ctx: &mut GearCtx<KolorinkoRT, S>,
    _cache: &mut LatestCache,
) -> ArticleView {
    let ArticleView {
        meta: page_meta,
        revisions,
        content,
    } = parsed;
    let mut visited = HashSet::new();
    visited.insert((site.clone(), slug.0.clone(), slug.1.clone()));
    let content = resolve(content, &site, meta, &mut visited, ctx).await;
    ArticleView {
        meta: page_meta,
        revisions,
        content,
    }
}

/// Resolve every `[[include]]` directive anywhere inside `content`, splicing
/// the included pages' content in place of each directive. Works in passes:
/// each pass collects the include targets reachable from the current tree but
/// not yet fetched, declares each as a `secondary_get` dependency (so the whole
/// result is reactive to edits anywhere in the transitive include cone),
/// fetches them, and substitutes; then repeats until a pass finds nothing
/// new. A `visited` set breaks data-level cycles (A includes B includes A).
async fn resolve<S: Storage<KolorinkoRT>>(
    mut content: Content,
    site: &SafePathComponent,
    meta: &RepoMeta,
    visited: &mut HashSet<Key>,
    ctx: &mut GearCtx<KolorinkoRT, S>,
) -> Content {
    loop {
        let mut targets: Vec<(Key, SafePathComponent, Slug)> = Vec::new();
        collect_include_targets(&content, site, visited, &mut targets);
        if targets.is_empty() {
            break;
        }
        let mut fetched: HashMap<Key, Content> = HashMap::new();
        for (key, inc_site, inc_slug) in targets {
            visited.insert(key.clone());
            let parsed = crate::runtime::article_latest_parsed(
                meta.clone(),
                inc_site.clone(),
                inc_slug.clone(),
            )
            .secondary_get(ctx)
            .await;
            fetched.insert(key, parsed.content);
        }
        content = substitute_includes(content, site, &fetched);
    }
    content
}

/// Walk `content` and record every `[[include]]` target not already in
/// `visited` (and not already batched in `out`), so [`resolve`] can fetch the
/// whole pass at once. Sync recursion over the tree — no awaits.
fn collect_include_targets(
    content: &Content,
    current_site: &SafePathComponent,
    visited: &HashSet<Key>,
    out: &mut Vec<(Key, SafePathComponent, Slug)>,
) {
    for node in content {
        match node {
            Node::Include(inc) => {
                if let Some((inc_site, inc_slug)) = include_target(&inc.source, current_site)
                    && let key = (inc_site.clone(), inc_slug.0.clone(), inc_slug.1.clone())
                    && !visited.contains(&key)
                    && !out.iter().any(|(k, _, _)| *k == key)
                {
                    out.push((key, inc_site, inc_slug));
                }
            }
            Node::Container { content, .. } | Node::Heading { content, .. } => {
                collect_include_targets(content, current_site, visited, out);
            }
            Node::Table(rows) => {
                for row in rows {
                    for cell in row {
                        collect_include_targets(&cell.content, current_site, visited, out);
                    }
                }
            }
            Node::SupSubscript { sup, sub } => {
                collect_include_targets(sup, current_site, visited, out);
                collect_include_targets(sub, current_site, visited, out);
            }
            Node::Link { text, .. } | Node::Footnote(text) => {
                collect_include_targets(text, current_site, visited, out);
            }
            Node::Tabview(tabs) => {
                for tab in tabs {
                    collect_include_targets(&tab.name, current_site, visited, out);
                    collect_include_targets(&tab.content, current_site, visited, out);
                }
            }
            Node::ListPages(lp) => {
                collect_include_targets(&lp.prepend, current_site, visited, out);
                collect_include_targets(&lp.repeat, current_site, visited, out);
                collect_include_targets(&lp.append, current_site, visited, out);
            }
            _ => {}
        }
    }
}

/// Return `content` with every `[[include]]` whose target was fetched this
/// pass replaced by that target's content (spliced inline); directives that
/// couldn't be resolved (unknown target, or a data cycle) are left verbatim.
fn substitute_includes(
    content: Content,
    current_site: &SafePathComponent,
    fetched: &HashMap<Key, Content>,
) -> Content {
    let mut out: Content = Vec::with_capacity(content.len());
    for node in content {
        match node {
            Node::Include(inc) => {
                let resolved = include_target(&inc.source, current_site)
                    .and_then(|(s, slug)| {
                        fetched
                            .get(&(s, slug.0, slug.1))
                            .map(Content::as_slice)
                    });
                match resolved {
                    Some(nodes) => out.extend_from_slice(nodes),
                    None => out.push(Node::Include(inc)),
                }
            }
            Node::Container { kind, content } => out.push(Node::Container {
                kind,
                content: substitute_includes(content, current_site, fetched),
            }),
            Node::Heading { level, content } => out.push(Node::Heading {
                level,
                content: substitute_includes(content, current_site, fetched),
            }),
            Node::Table(rows) => out.push(Node::Table(
                rows.into_iter()
                    .map(|row| {
                        row.into_iter()
                            .map(|cell| kolorinko_wikitext::TableCell {
                                colspan: cell.colspan,
                                header: cell.header,
                                align: cell.align,
                                content: substitute_includes(cell.content, current_site, fetched),
                            })
                            .collect()
                    })
                    .collect(),
            )),
            Node::SupSubscript { sup, sub } => out.push(Node::SupSubscript {
                sup: substitute_includes(sup, current_site, fetched),
                sub: substitute_includes(sub, current_site, fetched),
            }),
            Node::Link { target, text } => out.push(Node::Link {
                target,
                text: substitute_includes(text, current_site, fetched),
            }),
            Node::Footnote(c) => out.push(Node::Footnote(substitute_includes(
                c,
                current_site,
                fetched,
            ))),
            Node::Tabview(tabs) => out.push(Node::Tabview(
                tabs.into_iter()
                    .map(|tab| kolorinko_wikitext::Tab {
                        name: substitute_includes(tab.name, current_site, fetched),
                        content: substitute_includes(tab.content, current_site, fetched),
                    })
                    .collect(),
            )),
            Node::ListPages(lp) => out.push(Node::ListPages(kolorinko_wikitext::ListPages {
                params: lp.params,
                prepend: substitute_includes(lp.prepend, current_site, fetched),
                repeat: substitute_includes(lp.repeat, current_site, fetched),
                append: substitute_includes(lp.append, current_site, fetched),
            })),
            leaf => out.push(leaf),
        }
    }
    out
}

/// Resolve an include's [`PageRef`] to `(site, slug)` on the current site.
/// The parser parks the first `:`-segment of the source in [`PageRef::space`];
/// for same-site page refs that segment is the category, so `space` → category
/// and the trailing path → name. Cross-site includes (`space` = another site)
/// are not yet supported. Unresolvable targets (bad path component) return
/// `None` and the directive is left in place.
fn include_target(src: &PageRef, current_site: &SafePathComponent) -> Option<(SafePathComponent, Slug)> {
    let name = SafePathComponent::new(src.path.last()?.clone())?;
    let category = match &src.space {
        Some(cat) => Some(SafePathComponent::new(cat.clone())?),
        None => None,
    };
    Some((current_site.clone(), (category, name)))
}
