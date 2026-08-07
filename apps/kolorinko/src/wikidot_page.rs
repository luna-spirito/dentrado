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
//! # Storage model
//! The dataset never materialises body text. It is built by walking the git
//! **object database** pinned to a commit tip, storing each body as its blob
//! `Oid` (cheap, content-addressed, immutable). Text is paged in lazily by the
//! [`repo_l_article_latest`] lens — the only stage co-located with the
//! `!Send` [`git2::Repository`] — through a shared bounded hot cache ([`Odb`]).
//! Memory thus tracks the rendered working set, not the repository size, and a
//! moving tip never tears a live snapshot: its Oids stay valid in the odb.
//!
//! # Gears
//! - [`repo`] (`local` oracle): polls the git remote on a timer and rebuilds
//!   the dataset as [`Rc`]`<`[`RepoData`]`>` of blob Oids. Pinned to one core;
//!   never crosses a thread, so it holds the `!Send` [`Odb`] via an [`Rc`].
//! - [`repo_l_article_latest`] (`follow` lens over `repo`): projects one page
//!   into an owned [`ArticleLatest`] (metadata + latest body + revision list,
//!   no bodies). Shippable, so `Send` (owned `String`s, no `Rc`/`Arc`); the one
//!   place a body blob is read out of the odb.
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

use crate::wikidot_parser::parse;
use dentrado::core::{core_ctx::GearCtx, storage::Storage};
use git2::{ObjectType, Oid, Repository, Tree, TreeWalkMode};
use im::HashMap as ImHashMap;
use kolorinko_rt::SafePathComponent;
use kolorinko_wikitext::{
    ArticleLatest, ArticleMeta, ArticleView, BlockCell, BlockRow, BlockTable, ContainerKind,
    Content, Include, ListPages, Node, PageRef, RevMeta, Tab, TableCell, TextObj,
};
use log::error;
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet, VecDeque},
    fmt,
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
        Self {
            url,
            path,
            interval,
        }
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
type Key = (
    SafePathComponent,
    Option<SafePathComponent>,
    SafePathComponent,
);

/// All sites mirrored out of the repository at one point in time. A persistent
/// [`im::HashMap`] so cloning the [`Rc`]`<RepoData>` is O(1) and an update is
/// non-destructive (dependents holding a prior snapshot see a stable view).
/// Carries the [`!Send`](Odb) odb handle so the [`repo_l_article_latest`] lens
/// — the only co-located consumer — can materialise bodies on demand.
#[derive(Default, Clone)]
pub(crate) struct RepoData {
    sites: ImHashMap<SafePathComponent, WDWebsite>,
    odb: Option<Odb>,
}

impl fmt::Debug for RepoData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RepoData")
            .field("sites", &self.sites)
            .finish_non_exhaustive()
    }
}

impl RepoData {
    /// Look up one page by `(site, slug)`.
    #[must_use]
    fn article(&self, site: &SafePathComponent, slug: &Slug) -> Option<&Article> {
        self.sites.get(site)?.articles.get(&slug.0)?.get(&slug.1)
    }
}

/// One mirrored site: its pages nested by category.
#[derive(Default, Clone, Debug)]
pub(crate) struct WDWebsite {
    articles: ImHashMap<Option<SafePathComponent>, ImHashMap<SafePathComponent, Article>>,
}

/// One page: metadata, the full revision-history summary, and blob Oids for the
/// latest body and **every** revision body. Bodies are never materialised here
/// — they live in the git object database, paged in lazily via [`Odb::blob`].
#[derive(Clone, Debug)]
pub(crate) struct Article {
    meta: ArticleMeta,
    latest_body: Oid,
    revisions: Vec<RevMeta>,
    /// Every revision body's blob Oid (cheap; not text). Read on demand by the
    /// postponed `repo_l_article_revision` gear.
    #[allow(dead_code)]
    bodies: ImHashMap<u64, Oid>,
}

/// Shared, lazily-materialised view over the git object database on the
/// [`repo`] oracle's core: one [`Repository`] handle plus a bounded hot cache of
/// already-read body blobs. `!Send` — it never leaves the oracle's core, so both
/// the oracle (build/diff) and the [`repo_l_article_latest`] lens (body
/// materialisation) reach it through the [`Rc`] cloned into every [`RepoData`].
pub(crate) type Odb = Rc<OdbInner>;

pub(crate) struct OdbInner {
    repo: Repository,
    blobs: RefCell<Blobs>,
}

struct Blobs {
    map: HashMap<Oid, Rc<str>>,
    order: VecDeque<Oid>,
}

impl OdbInner {
    /// Soft cap on the hot blob cache. Beyond it the oldest inserted blob is
    /// evicted (FIFO) — memory stays bounded to the rendered working set, not
    /// the whole repository.
    const CAP: usize = 8192;

    fn new(repo: Repository) -> Self {
        Self {
            repo,
            blobs: RefCell::new(Blobs {
                map: HashMap::new(),
                order: VecDeque::new(),
            }),
        }
    }

    /// Read body blob `oid` from the object database as a frontmatter-stripped
    /// `Rc<str>`, caching it. Content-addressed and immutable, so the cache is
    /// sound for the Oid's lifetime. `None` if the blob is missing or not valid
    /// UTF-8.
    fn blob(&self, oid: Oid) -> Option<Rc<str>> {
        {
            let b = self.blobs.borrow();
            if let Some(s) = b.map.get(&oid) {
                return Some(Rc::clone(s));
            }
        }
        let raw = blob_str(&self.repo, oid)?;
        let s: Rc<str> = Rc::from(revision_body(&raw));
        let mut b = self.blobs.borrow_mut();
        b.map.insert(oid, Rc::clone(&s));
        b.order.push_back(oid);
        if b.order.len() > Self::CAP
            && let Some(old) = b.order.pop_front()
        {
            b.map.remove(&old);
        }
        Some(s)
    }
}

// =========================================================================
// `repo` oracle gear
// =========================================================================

/// Reverse index used by [`incremental_update`]: each `_meta` file path → its
/// nested-map [`Key`]. Kept across ticks so a moved tip can patch only the
/// pages the git diff touched, locating the old key to remove when a page's
/// slug changed or it was deleted.
type Index = HashMap<PathBuf, Key>;

/// Per-instance cache for [`repo`]: the shared [`Odb`] (opened `Repository` +
/// blob cache, kept across ticks), the last commit tip, the last-built dataset,
/// and the reverse [`Index`]. Wrapped in `Rc<RefCell<…>>` so the cache (which
/// must be `Clone + Debug`) is a cheap refcount bump.
#[derive(Default, Clone)]
pub(crate) struct RepoCache(Rc<RefCell<RepoInner>>);

#[derive(Default)]
struct RepoInner {
    odb: Option<Odb>,
    last_tip: Option<Oid>,
    data: Option<Rc<RepoData>>,
    index: Index,
}

impl fmt::Debug for RepoCache {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("RepoCache").finish()
    }
}

/// Run the [`repo`] oracle. On a tick: open/clone lazily, fetch + hard-reset,
/// and on a moved tip patch the dataset **incrementally** — only the pages the
/// `old_tip → new_tip` git diff touched are re-read (as blob Oids) from the new
/// tip's tree, producing a new [`Rc`]`<`[`RepoData`]`>` that structurally shares
/// almost all of the old one. Falls back to a full [`build_from_tree`] on the
/// first build or when the diff can't be computed (e.g. a force-push
/// garbage-collected the old tip). A same-tip or non-tick run returns the
/// previously built dataset unchanged.
pub(crate) fn repo(meta: &RepoMeta, tick: bool, cache: &mut RepoCache) -> Rc<RepoData> {
    let mut inner = cache.0.borrow_mut();
    if inner.odb.is_none() {
        inner.odb = open_or_clone(meta.url, meta.path).map(|r| Rc::new(OdbInner::new(r)));
    }
    // Clone the odb handle out so git ops below never borrow `inner` (the pulls,
    // diffs, and tree reads are synchronous, but this keeps the borrows trivial).
    let odb = inner.odb.clone();
    if tick
        && let Some(odb) = odb.as_ref()
        && let Some(PullOutcome::Updated { new_tip }) = pull_for_diff(Some(&odb.repo))
    {
        let prev_tip = inner.last_tip;
        let prev_data = inner.data.clone();
        let rebuilt: Option<(RepoData, Index)> = match (prev_tip, prev_data.as_ref()) {
            (Some(old_tip), Some(old_data)) if old_tip != new_tip => {
                match diff_affected_meta_paths(&odb.repo, old_tip, new_tip, meta.path) {
                    Some(affected) if affected.is_empty() => None,
                    Some(affected) => {
                        let mut index = inner.index.clone();
                        let new_tree = odb
                            .repo
                            .find_commit(new_tip)
                            .ok()
                            .and_then(|c| odb.repo.find_tree(c.tree_id()).ok());
                        let data = match new_tree.as_ref() {
                            Some(t) => incremental_update(
                                &odb.repo, t, meta.path, old_data, &mut index, affected,
                            ),
                            None => build_from_tree(odb, new_tip, meta.path).0,
                        };
                        Some((data, index))
                    }
                    None => Some(build_from_tree(odb, new_tip, meta.path)),
                }
            }
            _ => Some(build_from_tree(odb, new_tip, meta.path)),
        };
        inner.last_tip = Some(new_tip);
        if let Some((data, index)) = rebuilt {
            inner.data = Some(Rc::new(data));
            inner.index = index;
        }
    }
    if inner.data.is_none() {
        let tip = current_tip(odb.as_ref().map(|o| &o.repo));
        let (data, index) = match (odb.as_ref(), tip) {
            (Some(o), Some(t)) => build_from_tree(o, t, meta.path),
            _ => (RepoData::default(), inner.index.clone()),
        };
        inner.last_tip = tip;
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
// Tree walk → RepoData (blob Oids only; no body text materialised)
// =========================================================================

/// Walk the commit's tree at `tip` and build a [`RepoData`]: for each site,
/// every `_meta/<p1>/<p2>/<pageid>` blob yields one [`Article`] (metadata parsed
/// from the blob, body blob Oids recorded from the sibling `_pages_by_id`
/// subtree). Bodies stay as Oids — never read into memory here.
fn build_from_tree(odb: &Odb, tip: Oid, root: &Path) -> (RepoData, Index) {
    let repo = &odb.repo;
    let mut sites: ImHashMap<SafePathComponent, WDWebsite> = ImHashMap::new();
    let mut index: Index = HashMap::new();
    let root_tree = match repo
        .find_commit(tip)
        .and_then(|c| repo.find_tree(c.tree_id()))
    {
        Ok(t) => t,
        Err(_) => {
            return (
                RepoData {
                    sites,
                    odb: Some(Rc::clone(odb)),
                },
                index,
            );
        }
    };
    // `(site, p1, p2, id)` keyed: the `_meta` blob Oid + per-revision body Oids.
    let mut metas: HashMap<(String, String, String, String), Oid> = HashMap::new();
    let mut bodies: HashMap<(String, String, String, String), ImHashMap<u64, Oid>> = HashMap::new();
    root_tree
        .walk(TreeWalkMode::PreOrder, |dir, entry| {
            if entry.kind() != Some(ObjectType::Blob) {
                return 0;
            }
            let Ok(name) = entry.name() else {
                return 0;
            };
            let path = format!("{dir}{name}");
            let comps: Vec<&str> = path.split('/').collect();
            match comps.as_slice() {
                [site, "_meta", p1, p2, id] => {
                    metas.insert(
                        ((*site).into(), (*p1).into(), (*p2).into(), (*id).into()),
                        entry.id(),
                    );
                }
                [site, "_pages_by_id", p1, p2, id, rfile] => {
                    if let Some(n) = rev_number(rfile) {
                        bodies
                            .entry(((*site).into(), (*p1).into(), (*p2).into(), (*id).into()))
                            .or_default()
                            .insert(n, entry.id());
                    }
                }
                _ => {}
            }
            0
        })
        .ok();
    for (key, meta_oid) in metas {
        let (site, p1, p2, id) = &key;
        let Some(site_c) = SafePathComponent::new(site.clone()) else {
            continue;
        };
        let Some(meta_text) = blob_str(repo, meta_oid) else {
            continue;
        };
        let pm = parse_meta(&meta_text);
        let body_map = bodies.remove(&key).unwrap_or_default();
        let Some(latest) = body_map.keys().max().copied() else {
            continue;
        };
        let Some(&latest_body) = body_map.get(&latest) else {
            continue;
        };
        let Some((cat, name)) = slug_to_key(&pm.slug) else {
            continue;
        };
        let article = Article {
            meta: ArticleMeta {
                title: pm.title,
                tags: pm.tags,
                slug: pm.slug,
                page_id: format!("{p1}{p2}{id}"),
            },
            latest_body,
            revisions: pm.revisions,
            bodies: body_map,
        };
        let meta_path = root.join(site).join("_meta").join(p1).join(p2).join(id);
        index.insert(meta_path, (site_c.clone(), cat.clone(), name.clone()));
        insert_page(&mut sites, site_c, cat, name, article);
    }
    (
        RepoData {
            sites,
            odb: Some(Rc::clone(odb)),
        },
        index,
    )
}

/// Read one [`Article`] out of `tree` at `(site, p1, p2, id)`: parse the `_meta`
/// blob and record every `r{N}.txt` body blob Oid from the matching
/// `_pages_by_id` subtree. Used by the incremental path to re-read only the
/// pages a git diff touched.
fn read_page(
    repo: &Repository,
    tree: &Tree,
    site: &str,
    p1: &str,
    p2: &str,
    id: &str,
) -> Option<Article> {
    let meta_rel = format!("{site}/_meta/{p1}/{p2}/{id}");
    let meta_oid = tree.get_path(Path::new(&meta_rel)).ok()?.id();
    let pm = parse_meta(&blob_str(repo, meta_oid)?);
    let body_map = enumerate_bodies(repo, tree, &format!("{site}/_pages_by_id/{p1}/{p2}/{id}"));
    let latest = body_map.keys().max().copied()?;
    let latest_body = *body_map.get(&latest)?;
    Some(Article {
        meta: ArticleMeta {
            title: pm.title,
            tags: pm.tags,
            slug: pm.slug,
            page_id: format!("{p1}{p2}{id}"),
        },
        latest_body,
        revisions: pm.revisions,
        bodies: body_map,
    })
}

/// Every `r{N}.txt` blob Oid directly under `dir_rel` in `tree`, as `{N → oid}`.
fn enumerate_bodies(repo: &Repository, tree: &Tree, dir_rel: &str) -> ImHashMap<u64, Oid> {
    let mut map = ImHashMap::new();
    let Ok(entry) = tree.get_path(Path::new(dir_rel)) else {
        return map;
    };
    let Ok(obj) = entry.to_object(repo) else {
        return map;
    };
    let Some(dir_tree) = obj.as_tree() else {
        return map;
    };
    for e in dir_tree.iter() {
        if e.kind() != Some(ObjectType::Blob) {
            continue;
        }
        let Ok(name) = e.name() else {
            continue;
        };
        if let Some(n) = rev_number(name) {
            map.insert(n, e.id());
        }
    }
    map
}

/// Parse the revision number out of a body file name: `r12.txt` → `12`.
fn rev_number(name: &str) -> Option<u64> {
    name.strip_prefix('r')
        .and_then(|s| s.strip_suffix(".txt"))
        .and_then(|s| s.parse().ok())
}

/// Read a blob by Oid straight from the odb as an owned `String` (uncached —
/// used for the small `_meta` blobs at build time).
fn blob_str(repo: &Repository, oid: Oid) -> Option<String> {
    let blob = repo.find_blob(oid).ok()?;
    std::str::from_utf8(blob.content()).ok().map(String::from)
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
    ParsedMeta {
        slug,
        title,
        tags,
        revisions,
    }
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
/// either segment is unsafe (the page is dropped, as in [`build_from_tree`]).
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
/// [`Index`]), then re-read the page (as blob Oids) from the new tip's `tree`
/// and re-insert under its current slug. Unaffected pages are structurally
/// shared from [`old`] (`im::HashMap`), so only the touched pages are re-read.
fn incremental_update(
    repo: &Repository,
    tree: &Tree,
    root: &Path,
    old: &RepoData,
    index: &mut Index,
    affected: HashSet<PathBuf>,
) -> RepoData {
    let mut sites = old.sites.clone();
    for meta_path in affected {
        if let Some(old_key) = index.remove(&meta_path) {
            remove_page(&mut sites, &old_key);
        }
        let Some((site, p1, p2, id)) = meta_page_parts(&meta_path, root) else {
            continue;
        };
        let Some(article) = read_page(repo, tree, &site, &p1, &p2, &id) else {
            continue;
        };
        let Some(site_c) = SafePathComponent::new(site) else {
            continue;
        };
        let Some((cat, name)) = slug_to_key(&article.meta.slug) else {
            continue;
        };
        index.insert(meta_path, (site_c.clone(), cat.clone(), name.clone()));
        insert_page(&mut sites, site_c, cat, name, article);
    }
    RepoData {
        sites,
        odb: old.odb.clone(),
    }
}

/// The set of `_meta` paths changed between two tips. Each git-diff delta path
/// (old and new side) is normalized via [`normalize_meta_path`]; non-page paths
/// (e.g. `files/…`, top-level docs) are dropped. `None` if either tree is
/// unreachable (force-push GC of the old tip) — the caller falls back to
/// [`build_from_tree`].
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

/// `<root>/<site>/_meta/<p1>/<p2>/<id>` → `(site, p1, p2, id)`, stripping the
/// root and the `_meta` kind segment. Used to re-read one page from the tree.
fn meta_page_parts(meta_path: &Path, root: &Path) -> Option<(String, String, String, String)> {
    let rel = meta_path.strip_prefix(root).ok()?;
    let mut c = rel.components();
    let site = c.next()?.as_os_str().to_str()?.to_string();
    let _kind = c.next()?.as_os_str().to_str()?; // "_meta"
    let p1 = c.next()?.as_os_str().to_str()?.to_string();
    let p2 = c.next()?.as_os_str().to_str()?.to_string();
    let id = c.next()?.as_os_str().to_str()?.to_string();
    Some((site, p1, p2, id))
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

/// Project one page out of `repo`'s dataset into a shippable [`ArticleLatest`],
/// materialising the latest body blob out of the odb. A missing page (or an
/// unopenable repository) yields an empty [`ArticleLatest`] (blank render).
pub(crate) fn repo_l_article_latest(
    data: &RepoData,
    site: &SafePathComponent,
    slug: &Slug,
) -> ArticleLatest {
    data.article(site, slug)
        .and_then(|a| {
            let body = data.odb.as_ref()?.blob(a.latest_body)?;
            Some(ArticleLatest {
                meta: a.meta.clone(),
                body: (*body).to_string(),
                revisions: a.revisions.clone(),
            })
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
    let content = apply_include_vars(content, &HashMap::new());
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
            Node::BlockTable(t) => {
                for row in &t.rows {
                    collect_include_targets(&row.content, current_site, visited, out);
                }
            }
            Node::BlockCell(c) => {
                collect_include_targets(&c.content, current_site, visited, out);
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
                    .and_then(|(s, slug)| fetched.get(&(s, slug.0, slug.1)).map(Content::as_slice));
                match resolved {
                    Some(nodes) => out.extend(apply_include_vars(nodes.to_vec(), &inc.vars)),
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
            Node::BlockTable(t) => out.push(Node::BlockTable(BlockTable {
                params: t.params,
                rows: t
                    .rows
                    .into_iter()
                    .map(|r| BlockRow {
                        params: r.params,
                        content: substitute_includes(r.content, current_site, fetched),
                    })
                    .collect(),
            })),
            Node::BlockCell(c) => out.push(Node::BlockCell(BlockCell {
                header: c.header,
                params: c.params,
                content: substitute_includes(c.content, current_site, fetched),
            })),
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

/// Replace every [`TextObj::IncludeVar`] in `content` using `vars`: a standalone
/// variable expands to its (recursively substituted) [`Content`]; inside an
/// attribute or image source it is flattened to plain text. An unresolved
/// variable falls back to its `//default`, or to nothing when it has none.
fn apply_include_vars(content: Content, vars: &HashMap<String, Content>) -> Content {
    content
        .into_iter()
        .flat_map(|n| subst_node(n, vars))
        .collect()
}

fn subst_node(node: Node, vars: &HashMap<String, Content>) -> Content {
    match node {
        Node::Text(TextObj::IncludeVar { name, default }) => match vars.get(&name) {
            Some(v) => apply_include_vars(v.clone(), vars),
            None => default
                .map(|d| apply_include_vars(d, vars))
                .unwrap_or_default(),
        },
        Node::Text(other) => vec![Node::Text(other)],
        Node::Container { kind, content } => vec![Node::Container {
            kind: subst_kind(kind, vars),
            content: apply_include_vars(content, vars),
        }],
        Node::Heading { level, content } => vec![Node::Heading {
            level,
            content: apply_include_vars(content, vars),
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
        Node::Table(rows) => vec![Node::Table(
            rows.into_iter()
                .map(|row| {
                    row.into_iter()
                        .map(|c| TableCell {
                            colspan: c.colspan,
                            header: c.header,
                            align: c.align,
                            content: apply_include_vars(c.content, vars),
                        })
                        .collect()
                })
                .collect(),
        )],
        Node::BlockTable(t) => vec![Node::BlockTable(BlockTable {
            params: subst_params(t.params, vars),
            rows: t
                .rows
                .into_iter()
                .map(|r| BlockRow {
                    params: subst_params(r.params, vars),
                    content: apply_include_vars(r.content, vars),
                })
                .collect(),
        })],
        Node::BlockCell(c) => vec![Node::BlockCell(BlockCell {
            header: c.header,
            params: subst_params(c.params, vars),
            content: apply_include_vars(c.content, vars),
        })],
        Node::SupSubscript { sup, sub } => vec![Node::SupSubscript {
            sup: apply_include_vars(sup, vars),
            sub: apply_include_vars(sub, vars),
        }],
        Node::Link { target, text } => vec![Node::Link {
            target,
            text: apply_include_vars(text, vars),
        }],
        Node::Include(inc) => vec![Node::Include(Include {
            source: inc.source,
            vars: inc
                .vars
                .into_iter()
                .map(|(k, v)| (k, apply_include_vars(v, vars)))
                .collect(),
        })],
        Node::ListPages(lp) => vec![Node::ListPages(ListPages {
            params: lp.params,
            prepend: apply_include_vars(lp.prepend, vars),
            repeat: apply_include_vars(lp.repeat, vars),
            append: apply_include_vars(lp.append, vars),
        })],
        Node::Footnote(c) => vec![Node::Footnote(apply_include_vars(c, vars))],
        Node::Tabview(tabs) => vec![Node::Tabview(
            tabs.into_iter()
                .map(|t| Tab {
                    name: apply_include_vars(t.name, vars),
                    content: apply_include_vars(t.content, vars),
                })
                .collect(),
        )],
        Node::Date { .. }
        | Node::HorizontalRule
        | Node::Raw(_)
        | Node::Stylesheet(_)
        | Node::Module(_)
        | Node::Code(_) => {
            vec![node]
        }
    }
}

fn subst_kind(kind: ContainerKind, vars: &HashMap<String, Content>) -> ContainerKind {
    match kind {
        ContainerKind::Div { inline, params } => ContainerKind::Div {
            inline,
            params: subst_params(params, vars),
        },
        other => other,
    }
}

fn subst_params(
    params: HashMap<String, Vec<TextObj>>,
    vars: &HashMap<String, Content>,
) -> HashMap<String, Vec<TextObj>> {
    params
        .into_iter()
        .map(|(k, v)| (k, subst_textobjs(v, vars)))
        .collect()
}

fn subst_textobjs(objs: Vec<TextObj>, vars: &HashMap<String, Content>) -> Vec<TextObj> {
    let mut out: Vec<TextObj> = Vec::new();
    for o in objs {
        let resolved: Vec<TextObj> = match o {
            TextObj::IncludeVar { name, default } => match vars.get(&name) {
                Some(v) => flatten_textobjs(&apply_include_vars(v.clone(), vars)),
                None => match default {
                    Some(d) => flatten_textobjs(&apply_include_vars(d, vars)),
                    None => Vec::new(),
                },
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

fn collect_plain(content: &Content, out: &mut String) {
    for n in content {
        match n {
            Node::Text(TextObj::Plain(s)) => out.push_str(s),
            Node::Text(TextObj::IncludeVar { default, .. }) => {
                if let Some(d) = default {
                    collect_plain(d, out);
                }
            }
            Node::Text(TextObj::ModuleVar { default, .. }) => {
                if let Some(d) = default {
                    out.push_str(d);
                }
            }
            Node::Container { content, .. }
            | Node::Heading { content, .. }
            | Node::Footnote(content) => collect_plain(content, out),
            Node::Link { text, .. } => collect_plain(text, out),
            Node::SupSubscript { sup, sub } => {
                collect_plain(sup, out);
                collect_plain(sub, out);
            }
            _ => {}
        }
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use git2::{IndexAddOption, Signature};
    use std::fs;

    /// Write one page (`_meta` + a single `r{rev}.txt` body) into the export
    /// layout under `root/<site>/…`, returning the relative paths committed.
    fn write_page(
        root: &Path,
        site: &str,
        p1: &str,
        p2: &str,
        id: &str,
        slug: &str,
        rev: u64,
        body: &str,
    ) {
        let base = root.join(site);
        let meta = format!("slug: \"{slug}\"\ntitle: \"T\"\ntags: []\n{rev}\trid\t1\ta\n");
        let meta_path = base.join("_meta").join(p1).join(p2).join(id);
        fs::create_dir_all(meta_path.parent().unwrap()).unwrap();
        fs::write(&meta_path, meta).unwrap();
        let body_dir = base.join("_pages_by_id").join(p1).join(p2).join(id);
        fs::create_dir_all(&body_dir).unwrap();
        fs::write(body_dir.join(format!("r{rev}.txt")), body).unwrap();
    }

    /// Stage everything under the worktree and commit (first commit if empty).
    fn commit(repo: &Repository, msg: &str, parents: &[&git2::Commit]) -> Oid {
        let sig = Signature::now("t", "t@t").unwrap();
        let mut idx = repo.index().unwrap();
        idx.add_all(["*"], IndexAddOption::DEFAULT, None).unwrap();
        idx.write().unwrap();
        let tree = repo.find_tree(idx.write_tree().unwrap()).unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, msg, &tree, parents)
            .unwrap()
    }

    fn site(s: &str) -> SafePathComponent {
        SafePathComponent::new(s.into()).unwrap()
    }

    fn root_slug(name: &str) -> Slug {
        (None, SafePathComponent::new(name.into()).unwrap())
    }

    #[test]
    fn build_and_materialise_from_odb() {
        let dir = std::env::temp_dir().join(format!("kolorinko_odb_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let repo = Repository::init(&dir).unwrap();
        write_page(
            &dir,
            "scp",
            "13",
            "05",
            "054470",
            "foo",
            1,
            "---\nx:1\n---\nFoo body",
        );
        write_page(
            &dir,
            "scp",
            "13",
            "05",
            "054471",
            "bar",
            1,
            "---\nx:1\n---\nBar body",
        );
        let tip = commit(&repo, "c1", &[]);

        let odb: Odb = Rc::new(OdbInner::new(repo));
        let (data, index) = build_from_tree(&odb, tip, &dir);

        // Two pages indexed; bodies stay as Oids until projected through the lens.
        assert_eq!(index.len(), 2);
        let foo = repo_l_article_latest(&data, &site("scp"), &root_slug("foo"));
        assert_eq!(foo.body, "Foo body");
        assert_eq!(foo.meta.slug, "foo");
        assert_eq!(foo.meta.page_id, "1305054470");
        assert_eq!(
            repo_l_article_latest(&data, &site("scp"), &root_slug("bar")).body,
            "Bar body"
        );
    }

    #[test]
    fn incremental_patch_on_tip_move() {
        let dir = std::env::temp_dir().join(format!("kolorinko_inc_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let repo = Repository::init(&dir).unwrap();
        write_page(
            &dir,
            "scp",
            "13",
            "05",
            "054470",
            "foo",
            1,
            "---\nx:1\n---\nFoo v1",
        );
        write_page(
            &dir,
            "scp",
            "13",
            "05",
            "054471",
            "bar",
            1,
            "---\nx:1\n---\nBar v1",
        );
        let tip1 = commit(&repo, "c1", &[]);

        let odb: Odb = Rc::new(OdbInner::new(repo));
        let (data, mut index) = build_from_tree(&odb, tip1, &dir);
        assert_eq!(
            repo_l_article_latest(&data, &site("scp"), &root_slug("foo")).body,
            "Foo v1"
        );

        // Edit only `foo` (new revision → moved blob Oid) and advance the tip.
        write_page(
            &dir,
            "scp",
            "13",
            "05",
            "054470",
            "foo",
            2,
            "---\nx:1\n---\nFoo v2",
        );
        let parent = odb.repo.find_commit(tip1).unwrap();
        let tip2 = commit(&odb.repo, "c2", &[&parent]);

        let affected = diff_affected_meta_paths(&odb.repo, tip1, tip2, &dir).unwrap();
        let tree2 = odb
            .repo
            .find_commit(tip2)
            .and_then(|c| odb.repo.find_tree(c.tree_id()))
            .unwrap();
        let next = incremental_update(&odb.repo, &tree2, &dir, &data, &mut index, affected);

        // `bar` is structurally shared from the old snapshot; `foo` re-read.
        assert_eq!(
            repo_l_article_latest(&next, &site("scp"), &root_slug("foo")).body,
            "Foo v2"
        );
        assert_eq!(
            repo_l_article_latest(&next, &site("scp"), &root_slug("bar")).body,
            "Bar v1"
        );
    }

    fn plain(s: &str) -> Content {
        vec![Node::Text(TextObj::Plain(s.to_string()))]
    }

    fn ivar(name: &str) -> Node {
        Node::Text(TextObj::IncludeVar {
            name: name.to_string(),
            default: None,
        })
    }

    #[test]
    fn include_var_resolves_to_value() {
        let mut vars = HashMap::new();
        vars.insert("align".to_string(), plain("right"));
        let out = apply_include_vars(vec![ivar("align")], &vars);
        assert_eq!(out, plain("right"));
    }

    #[test]
    fn unresolved_include_var_uses_default() {
        let node = Node::Text(TextObj::IncludeVar {
            name: "x".to_string(),
            default: Some(plain("fallback")),
        });
        let out = apply_include_vars(vec![node], &HashMap::new());
        assert_eq!(out, plain("fallback"));
    }

    #[test]
    fn unresolved_include_var_without_default_vanishes() {
        let out = apply_include_vars(vec![ivar("x")], &HashMap::new());
        assert!(out.is_empty());
    }

    #[test]
    fn include_var_in_div_param_flattens_to_text() {
        let div = Node::Container {
            kind: ContainerKind::Div {
                inline: false,
                params: HashMap::from([(
                    "style".to_string(),
                    vec![
                        TextObj::Plain("text-align: ".to_string()),
                        TextObj::IncludeVar {
                            name: "align".to_string(),
                            default: None,
                        },
                    ],
                )]),
            },
            content: vec![],
        };
        let mut vars = HashMap::new();
        vars.insert("align".to_string(), plain("right"));
        let out = apply_include_vars(vec![div], &vars);
        let Node::Container {
            kind: ContainerKind::Div { params, .. },
            ..
        } = &out[0]
        else {
            panic!("expected a div")
        };
        assert_eq!(
            params.get("style"),
            Some(&vec![TextObj::Plain("text-align: right".to_string())])
        );
    }
}
