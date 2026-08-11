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
//! [`repo_l_article_latest`] lens, reading each blob straight from the odb on
//! demand (uncached at this layer — odb lookups are cheap, and the expensive
//! parse step is dedup'd downstream by [`ParsedCache`]).
//!
//! libgit2 is synchronous and [`git2::Repository`] is `!Send`, so calling it
//! straight from a gear would block the whole async core. Instead the
//! `!Send` `Repository`, the reverse [`Index`], and the current dataset
//! snapshot all live on one **dedicated worker thread** ([`GitWorker`]),
//! created and pinned there — the `Repository` is never moved across a thread.
//! The gears talk to it over a [`GitMailbox`] (a `flume` channel) and `.await`
//! each reply, so every libgit2 call happens off the core. Memory tracks the
//! rendered working set, not the repository size (body blobs are never retained
//! here), and a moving tip never tears a live snapshot: its Oids stay valid in
//! the odb.
//!
//! # Gears
//! - [`repo`] (`local` oracle): on each timer tick asks the worker to fetch +
//!   rebuild — incrementally, only the pages the `old_tip → new_tip` diff
//!   touched — and adopts the worker's new [`RepoData`] snapshot (wrapped in
//!   [`Rc`]). An unchanged tip yields `None`, so the prior `Rc` is kept and
//!   dependents aren't re-run for nothing.
//! - [`repo_l_article_latest`] (`follow` lens over `repo`): projects one page
//!   into an owned [`ArticleLatest`] (metadata + latest body + revision list),
//!   reading the latest body blob out of the worker's cache via the
//!   [`GitMailbox`] carried in [`RepoData`]. Shippable (`Send`: owned `String`s).
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
//! - [`shell`] (`follow` over `repo`): the whole site chrome in one shot — the
//!   resolved `nav:top` / `nav:side` pages (declared as [`article_latest`]
//!   [`secondary_get`](dentrado::core::gear::GearQuery::secondary_get) deps)
//!   plus the theme-root URLs — so the client fetches the site frame under a
//!   single `site`-keyed subscription.

use crate::wikidot_parser::parse;
use compio::fs;
use dentrado::core::{core_ctx::GearCtx, storage::Storage};
use git2::{ObjectType, Oid, Repository, Tree, TreeWalkMode};
use imbl::HashMap as ImHashMap;
use kolorinko_render::rewrite;
use kolorinko_rt::{AssetKind, RepoAssetOut, RepoAssetPath, SafePathComponent, SiteShell};
use kolorinko_wikitext::{
    ArticleLatest, ArticleMeta, ArticleView, BlockCell, BlockRow, BlockTable, ContainerKind,
    Content, Include, List, ListItem, ListPages, Node, PageRef, RevMeta, Tab, TableCell, TextObj,
};
use log::error;
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
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

    #[must_use]
    pub(crate) const fn path(&self) -> &'static Path {
        self.path
    }

    #[must_use]
    pub(crate) const fn url(&self) -> &'static str {
        self.url
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

/// All sites mirrored out of the repository at one point in time, plus the
/// [`GitMailbox`] back to the worker thread that owns their source
/// [`Repository`]. A persistent [`imbl::HashMap`] so cloning the
/// [`Rc`]`<RepoData>` is O(1) and an update is non-destructive (dependents
/// holding a prior snapshot see a stable view). `Send`: no `Repository`, no
/// `Rc` — it crosses the worker→core channel once, then lives behind an `Rc`
/// on the oracle's core.
pub(crate) struct RepoData {
    sites: ImHashMap<SafePathComponent, WDWebsite>,
    mailbox: GitMailbox,
}

impl fmt::Debug for RepoData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RepoData")
            .field("sites", &self.sites)
            .finish_non_exhaustive()
    }
}

impl RepoData {
    /// An empty snapshot that still carries `mailbox` (used when the worker has
    /// no repository yet, or has died — body reads then resolve to `None`).
    fn empty(mailbox: GitMailbox) -> Self {
        Self {
            sites: ImHashMap::new(),
            mailbox,
        }
    }

    /// Look up one page by `(site, slug)`.
    #[must_use]
    fn article(&self, site: &SafePathComponent, slug: &Slug) -> Option<&Article> {
        find_article(&self.sites, site, slug)
    }
}

/// The nested-map lookup underlying [`RepoData::article`], factored out so the
/// build/incremental tests can resolve a page from a bare sites map.
fn find_article<'a>(
    sites: &'a ImHashMap<SafePathComponent, WDWebsite>,
    site: &SafePathComponent,
    slug: &Slug,
) -> Option<&'a Article> {
    sites.get(site)?.articles.get(&slug.0)?.get(&slug.1)
}

/// One mirrored site: its pages nested by category, plus the site's theme roots
/// (from `<site>/_meta/theme_roots`, one URL per line) applied by the client.
#[derive(Default, Clone, Debug)]
pub(crate) struct WDWebsite {
    articles: ImHashMap<Option<SafePathComponent>, ImHashMap<SafePathComponent, Article>>,
    theme_roots: Vec<String>,
}

/// One page: metadata, the full revision-history summary, and blob Oids for the
/// latest body and **every** revision body. Bodies are never materialised here
/// — they live in the git object database, paged in lazily via
/// [`GitMailbox::blob`].
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

// =========================================================================
// Git worker thread (owns the !Send `Repository`; serves the async gears)
// =========================================================================

/// A request to the git worker thread. Each carries its own one-shot reply
/// channel; the oracle core sends one and `.await`s the reply, so it never
/// blocks while libgit2 works on the worker thread.
enum GitReq {
    /// Fetch + rebuild if the tip moved. The reply is `Some` only when the
    /// dataset actually changed; `None` lets the caller keep its cached `Rc`.
    Tick {
        reply: flume::Sender<Option<RepoData>>,
    },
    /// The current dataset without pulling (cold start / first non-tick run).
    Snapshot { reply: flume::Sender<RepoData> },
    /// Read one body blob out of the object database (frontmatter stripped).
    Blob {
        oid: Oid,
        reply: flume::Sender<Option<String>>,
    },
}

/// Cloneable, `Send` handle to the git worker thread, embedded in [`RepoData`]
/// so the [`repo_l_article_latest`] lens — co-located on the oracle's core —
/// can materialise body blobs off-core. Cheap to clone (one `Arc` bump inside
/// the `flume` sender).
#[derive(Clone)]
pub(crate) struct GitMailbox(flume::Sender<GitReq>);

impl GitMailbox {
    /// Fetch + rebuild if the tip moved; `Some` only when the dataset changed.
    async fn tick(&self) -> Option<RepoData> {
        let (tx, rx) = flume::bounded(1);
        if self.0.send_async(GitReq::Tick { reply: tx }).await.is_err() {
            return None;
        }
        rx.recv_async().await.unwrap_or(None)
    }

    /// Current dataset without pulling.
    async fn snapshot(&self) -> RepoData {
        let (tx, rx) = flume::bounded(1);
        if self
            .0
            .send_async(GitReq::Snapshot { reply: tx })
            .await
            .is_err()
        {
            return RepoData::empty(self.clone());
        }
        rx.recv_async()
            .await
            .unwrap_or_else(|_| RepoData::empty(self.clone()))
    }

    /// Read one body blob off the worker thread. `None` if missing/bad UTF-8.
    async fn blob(&self, oid: Oid) -> Option<String> {
        let (tx, rx) = flume::bounded(1);
        if self
            .0
            .send_async(GitReq::Blob { oid, reply: tx })
            .await
            .is_err()
        {
            return None;
        }
        rx.recv_async().await.unwrap_or(None)
    }
}

/// The git worker thread's owned state. Everything `!Send` or git-bound lives
/// here: the [`Repository`] (created *on* this thread and never moved), the
/// reverse [`Index`], the last tip, and the current sites snapshot. The thread
/// runs [`GitWorker::run`], servicing [`GitReq`]s until every [`GitMailbox`]
/// (and thus every [`RepoData`]) is gone. Body blobs are read **uncached** —
/// git odb lookups are cheap (content-addressed) and run only on the oracle's
/// timer tick; the expensive work (parsing) is dedup'd downstream by
/// [`ParsedCache`](crate::wikidot_page::ParsedCache), so a worker-side blob
/// cache would only duplicate that for no gain.
struct GitWorker {
    repo: Option<Repository>,
    url: &'static str,
    root: &'static Path,
    mailbox: GitMailbox,
    last_tip: Option<Oid>,
    sites: ImHashMap<SafePathComponent, WDWebsite>,
    index: Index,
}

impl GitWorker {
    fn new(url: &'static str, root: &'static Path, mailbox: GitMailbox) -> Self {
        Self {
            repo: None,
            url,
            root,
            mailbox,
            last_tip: None,
            sites: ImHashMap::new(),
            index: HashMap::new(),
        }
    }

    /// Service [`GitReq`]s until the mailbox closes. Runs on the worker thread;
    /// `rx.recv()` blocks *this* thread, never the async core.
    fn run(mut self, rx: flume::Receiver<GitReq>) {
        while let Ok(req) = rx.recv() {
            match req {
                GitReq::Tick { reply } => {
                    let _ = reply.send(self.tick());
                }
                GitReq::Snapshot { reply } => {
                    self.snapshot();
                    let _ = reply.send(self.data());
                }
                GitReq::Blob { oid, reply } => {
                    let _ = reply.send(self.blob(oid));
                }
            }
        }
    }

    /// Fetch + rebuild if the tip moved. `Some(data)` when the dataset changed
    /// (the caller adopts a fresh snapshot); `None` when nothing changed (the
    /// caller keeps its prior `Rc`). The first tick always pulls + builds.
    fn tick(&mut self) -> Option<RepoData> {
        self.ensure_repo();
        let repo = self.repo.as_ref()?;
        let outcome = pull_for_diff(repo);
        if self.last_tip.is_none() {
            let tip = current_tip(repo);
            if let Some(t) = tip {
                let (sites, index) = build_from_tree(repo, t, self.root);
                self.sites = sites;
                self.index = index;
            }
            self.last_tip = tip;
            return Some(self.data());
        }
        let new_tip = match outcome {
            Some(PullOutcome::SameTip) => return None,
            Some(PullOutcome::Updated { new_tip }) => new_tip,
            None => return None,
        };
        let old_tip = self.last_tip.expect("Some on the non-first path above");
        let rebuilt: Option<(ImHashMap<SafePathComponent, WDWebsite>, Index)> =
            match diff_affected_meta_paths(repo, old_tip, new_tip, self.root) {
                Some(affected) if affected.is_empty() => None,
                Some(affected) => {
                    let mut index = self.index.clone();
                    let new_tree = repo
                        .find_commit(new_tip)
                        .ok()
                        .and_then(|c| repo.find_tree(c.tree_id()).ok());
                    match new_tree.as_ref() {
                        Some(tree) => {
                            let sites = incremental_update(
                                repo,
                                tree,
                                self.root,
                                &self.sites,
                                &mut index,
                                affected,
                            );
                            Some((sites, index))
                        }
                        None => Some(build_from_tree(repo, new_tip, self.root)),
                    }
                }
                None => Some(build_from_tree(repo, new_tip, self.root)),
            };
        self.last_tip = Some(new_tip);
        if let Some((sites, index)) = rebuilt {
            self.sites = sites;
            self.index = index;
            Some(self.data())
        } else {
            None
        }
    }

    /// Build the dataset once (no pull) if it hasn't been built yet.
    fn snapshot(&mut self) {
        self.ensure_repo();
        if self.last_tip.is_none() {
            self.build();
        }
    }

    fn build(&mut self) {
        let Some(repo) = self.repo.as_ref() else {
            return;
        };
        let tip = current_tip(repo);
        if let Some(t) = tip {
            let (sites, index) = build_from_tree(repo, t, self.root);
            self.sites = sites;
            self.index = index;
        }
        self.last_tip = tip;
    }

    /// Lazily open/clone the `Repository` on this thread, retrying on each tick
    /// until it succeeds (a transient clone failure shouldn't doom the gear).
    fn ensure_repo(&mut self) {
        if self.repo.is_none() {
            self.repo = open_or_clone(self.url, self.root);
        }
    }

    /// A fresh [`RepoData`] snapshot (structurally shared `sites` + our mailbox).
    fn data(&self) -> RepoData {
        RepoData {
            sites: self.sites.clone(),
            mailbox: self.mailbox.clone(),
        }
    }

    /// Read body blob `oid` straight from the odb (frontmatter stripped).
    /// Uncached — see [`GitWorker`]: odb lookups are cheap and infrequent, and
    /// the parse layer dedup's the expensive work. `None` if missing/bad UTF-8.
    fn blob(&self, oid: Oid) -> Option<String> {
        read_body(self.repo.as_ref()?, oid)
    }
}

/// Spawn the dedicated git worker thread for `meta` and return its mailbox. The
/// worker opens/clones the [`Repository`] *on its own thread* (it is `!Send` and
/// must never cross a thread), then services [`GitReq`]s for the gear's life —
/// keeping all synchronous libgit2 work off the async core.
fn spawn_worker(meta: &RepoMeta) -> GitMailbox {
    let (tx, rx) = flume::unbounded::<GitReq>();
    let mailbox = GitMailbox(tx);
    let worker_mailbox = mailbox.clone();
    let url = meta.url();
    let root = meta.path();
    std::thread::Builder::new()
        .name("kolorinko-git".into())
        .spawn(move || GitWorker::new(url, root, worker_mailbox).run(rx))
        .expect("spawn git worker thread");
    mailbox
}

// =========================================================================
// `repo` oracle gear
// =========================================================================

/// Reverse index kept by [`GitWorker`]: each `_meta` file path → its nested-map
/// [`Key`]. The path encodes the *physical* location `(site, p1, p2, id)`, but
/// `Key = (site, cat, name)` holds the *logical* location whose `cat`/`name`
/// come from the **slug inside the `_meta` file** — not from the path — so the
/// key is genuinely non-derivable from the path. The index exists so an
/// incremental update can O(1)-remove a page's old entry when its slug changed
/// (rename) or it was deleted; without it you'd have to re-read the old tip's
/// blob or scan the dataset by page id.
type Index = HashMap<PathBuf, Key>;

/// Per-instance cache for [`repo`]: the [`GitMailbox`] (the worker is spawned
/// lazily on the first run) and the last adopted [`RepoData`] snapshot — the
/// `Rc` returned on a non-tick run, and the stable pointer kept when a tick
/// found the tip unchanged (so dependents aren't re-run for nothing). Wrapped
/// in `Rc<RefCell<…>>` so the cache (which must be `Clone + Debug`) is a cheap
/// refcount bump.
#[derive(Default, Clone)]
pub(crate) struct RepoCache(Rc<RefCell<RepoOracleState>>);

#[derive(Default)]
struct RepoOracleState {
    mailbox: Option<GitMailbox>,
    data: Option<Rc<RepoData>>,
}

impl fmt::Debug for RepoCache {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("RepoCache").finish()
    }
}

/// Run the [`repo`] oracle. All git work is off-loaded to the worker thread
/// (see [`spawn_worker`]): on a tick it fetches and incrementally rebuilds
/// only the pages the `old_tip → new_tip` diff touched (falling back to a full
/// [`build_from_tree`] on the first build or when the diff is uncomputable, e.g.
/// a force-push GC'd the old tip); a non-tick run just serves the cached
/// snapshot. The worker returns `None` when the tip didn't move, so the prior
/// `Rc` is handed back unchanged. Each `borrow()` is its own statement — never
/// a `match`/`if let` scrutinee — so no RefCell borrow is held across a
/// re-borrow (the classic scrutinee-temporary foot-gun).
pub(crate) async fn repo(meta: &RepoMeta, tick: bool, cache: &mut RepoCache) -> Rc<RepoData> {
    let mailbox = cache.0.borrow().mailbox.clone();
    let mailbox = match mailbox {
        Some(mb) => mb,
        None => {
            let mb = spawn_worker(meta);
            cache.0.borrow_mut().mailbox = Some(mb.clone());
            mb
        }
    };
    // Non-tick: serve the cached snapshot without pulling.
    if !tick {
        let cached = cache.0.borrow().data.clone();
        if let Some(data) = cached {
            return data;
        }
        // Cold start (first run, before any tick): build once off-core, no pull.
        let rc = Rc::new(mailbox.snapshot().await);
        cache.0.borrow_mut().data = Some(Rc::clone(&rc));
        return rc;
    }
    // Tick: pull + rebuild-if-changed off-core.
    if let Some(data) = mailbox.tick().await {
        let rc = Rc::new(data);
        cache.0.borrow_mut().data = Some(Rc::clone(&rc));
        return rc;
    }
    // Unchanged tip: keep the prior snapshot (empty fallback if the worker had
    // no repository to clone on its very first, failed tick).
    cache
        .0
        .borrow()
        .data
        .clone()
        .unwrap_or_else(|| Rc::new(RepoData::empty(mailbox.clone())))
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
fn pull_for_diff(repo: &Repository) -> Option<PullOutcome> {
    let old_tip = current_tip(repo);
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

fn current_tip(repo: &Repository) -> Option<Oid> {
    repo.head().ok().and_then(|r| r.target())
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

/// Walk the commit's tree at `tip` and build the sites map: for each site,
/// every `_meta/<p1>/<p2>/<pageid>` blob yields one [`Article`] (metadata parsed
/// from the blob, body blob Oids recorded from the sibling `_pages_by_id`
/// subtree). Bodies stay as Oids — never read into memory here. Returns the bare
/// sites map + reverse [`Index`]; the worker attaches its mailbox to form the
/// [`RepoData`] snapshot.
fn build_from_tree(
    repo: &Repository,
    tip: Oid,
    root: &Path,
) -> (ImHashMap<SafePathComponent, WDWebsite>, Index) {
    let mut sites: ImHashMap<SafePathComponent, WDWebsite> = ImHashMap::new();
    let mut index: Index = HashMap::new();
    let root_tree = match repo
        .find_commit(tip)
        .and_then(|c| repo.find_tree(c.tree_id()))
    {
        Ok(t) => t,
        Err(_) => return (sites, index),
    };
    // `(site, p1, p2, id)` keyed: the `_meta` blob Oid + per-revision body Oids.
    let mut metas: HashMap<(String, String, String, String), Oid> = HashMap::new();
    let mut bodies: HashMap<(String, String, String, String), ImHashMap<u64, Oid>> = HashMap::new();
    let mut theme_roots: HashMap<String, Oid> = HashMap::new();
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
                [site, "_meta", "theme_roots"] => {
                    theme_roots.insert((*site).into(), entry.id());
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
    for (site, oid) in theme_roots {
        let Some(site_c) = SafePathComponent::new(site.clone()) else {
            continue;
        };
        let Some(text) = blob_str(repo, oid) else {
            continue;
        };
        let roots: Vec<String> = text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect();
        if let Some(mut w) = sites.get(&site_c).cloned() {
            w.theme_roots = roots;
            sites.insert(site_c, w);
        }
    }
    (sites, index)
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

/// Read a body blob by Oid, stripping its frontmatter. Uncached (the worker
/// reads blobs straight from the odb on demand); used directly by tests
/// against a live `Repository`.
fn read_body(repo: &Repository, oid: Oid) -> Option<String> {
    let raw = blob_str(repo, oid)?;
    Some(revision_body(&raw).to_string())
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
/// shared from [`old`] (`imbl::HashMap`), so only the touched pages are re-read.
fn incremental_update(
    repo: &Repository,
    tree: &Tree,
    root: &Path,
    old: &ImHashMap<SafePathComponent, WDWebsite>,
    index: &mut Index,
    affected: HashSet<PathBuf>,
) -> ImHashMap<SafePathComponent, WDWebsite> {
    let mut sites = old.clone();
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
    sites
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
/// category or site. Each level is cloned once (`imbl::HashMap` is O(1)), so this
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

/// Per-instance cache for [`shell`]: a pure aggregation re-derived each run
/// from its `article_latest` dependencies and the live [`RepoData`], so it
/// carries no state between runs.
#[derive(Default, Clone, Debug)]
pub(crate) struct ShellCache;

/// Project one page out of `repo`'s dataset into a shippable [`ArticleLatest`],
/// materialising the latest body blob via the worker thread (off-core). A
/// missing page (or an unopenable repository) yields an empty [`ArticleLatest`]
/// (blank render).
pub(crate) async fn repo_l_article_latest(
    data: &RepoData,
    site: &SafePathComponent,
    slug: &Slug,
) -> ArticleLatest {
    let Some(a) = data.article(site, slug) else {
        return ArticleLatest::default();
    };
    let meta = a.meta.clone();
    let revisions = a.revisions.clone();
    match data.mailbox.blob(a.latest_body).await {
        Some(body) => ArticleLatest {
            meta,
            body,
            revisions,
        },
        None => ArticleLatest::default(),
    }
}

/// The site's whole chrome in one shot: the fully include-resolved `nav:top`
/// and `nav:side` pages plus the theme-root URLs. Each nav page is declared as
/// an [`article_latest`](crate::runtime::article_latest)
/// [`secondary_get`](dentrado::core::gear::GearQuery::secondary_get) dependency
/// (so an edit to either re-runs this gear); the theme roots are projected
/// straight out of the followed [`RepoData`]. Keyed on `site` alone, so the
/// client fetches the entire site frame under one subscription that survives
/// page navigation within the site.
pub(crate) async fn shell<S: Storage<KolorinkoRT>>(
    repo_meta: RepoMeta,
    data: &RepoData,
    site: SafePathComponent,
    ctx: &mut GearCtx<KolorinkoRT, S>,
) -> SiteShell {
    let nav_top = crate::runtime::article_latest(repo_meta.clone(), site.clone(), nav_slug("top"))
        .secondary_get(ctx)
        .await;
    let nav_side =
        crate::runtime::article_latest(repo_meta.clone(), site.clone(), nav_slug("side"))
            .secondary_get(ctx)
            .await;
    SiteShell {
        nav_top: (*nav_top).clone(),
        nav_side: (*nav_side).clone(),
        theme_roots: theme_roots_of(data, &site),
    }
}

/// `(nav, name)` slug for one of the per-site navigation pages (`nav:top`,
/// `nav:side`).
fn nav_slug(name: &str) -> Slug {
    let category = SafePathComponent::new("nav".to_string()).unwrap();
    let page = SafePathComponent::new(name.to_string()).unwrap();
    (Some(category), page)
}

/// Project the site's theme-root URLs out of the dataset.
fn theme_roots_of(data: &RepoData, site: &SafePathComponent) -> Vec<String> {
    data.sites
        .get(site)
        .map(|w| w.theme_roots.clone())
        .unwrap_or_default()
}

// =========================================================================
// `repo_asset` gear — mirrored site assets (theme/files)
// =========================================================================

/// No carry-over state: the gear reads the repo working tree fresh each run,
/// and its output is cached by the runtime (shared across cores until evicted
/// under interest pressure). A `()`-equivalent unit so the macro never emits
/// the invalid `()::default()` (types that don't start with an identifier need
/// `<()>` in qualified paths).
#[derive(Default, Clone, Debug)]
pub(crate) struct RepoAssetCache;

/// Read one mirrored site asset (`<site>/<kind>/<host>/<path…>`) out of the repo
/// working tree, rewriting CSS refs to local `/repo/…` URLs and zstd-compressing
/// the result when that shrinks it. A missing file yields a redirect back onto
/// the original host (`https://{host}/{path…}`). The [`RepoAssetPath`] is the
/// validated `<host>/<path…>` tail, so it doubles as the redirect target.
///
/// `shared` so the cached bytes are shared across cores by reference (one
/// allocation, refcounted) rather than cloned per subscriber core.
pub(crate) async fn repo_asset(
    meta: &RepoMeta,
    site: &SafePathComponent,
    kind: AssetKind,
    path: &RepoAssetPath,
) -> RepoAssetOut {
    let file = meta
        .path()
        .join(site.as_ref())
        .join(kind.as_str())
        .join(path.as_ref());
    match fs::read(&file).await {
        Ok(bytes) => {
            let mime = crate::assets::mime_for(path.as_str());
            let body = if mime == "text/css" {
                let base = format!("https://{}", path.as_str());
                let site_str = site.as_ref().to_string_lossy();
                let text = String::from_utf8_lossy(&bytes);
                crate::assets::compress(
                    rewrite(&text, Some(&base), &site_str, kind.as_str()).into_bytes(),
                )
            } else {
                crate::assets::compress(bytes)
            };
            RepoAssetOut::Ok(body)
        }
        Err(_) => RepoAssetOut::Redirect {
            location: format!("https://{}", path.as_str()),
        },
    }
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
        body: Some(latest.body.clone()),
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
    parsed: &ArticleView,
    ctx: &mut GearCtx<KolorinkoRT, S>,
    _cache: &mut LatestCache,
) -> ArticleView {
    let ArticleView {
        meta: page_meta,
        revisions,
        content,
    } = parsed.clone();
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
            fetched.insert(key, parsed.content.clone());
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
            Node::List(list) => for_each_content_in_list(list, &mut |c| {
                collect_include_targets(c, current_site, visited, out)
            }),
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
            Node::List(list) => out.push(Node::List(map_list(list, &|c| {
                substitute_includes(c, current_site, fetched)
            }))),
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
        Node::List(list) => vec![Node::List(map_list(list, &|c| apply_include_vars(c, vars)))],
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

/// Walk a [`List`], producing a new one whose every item body (and nested
/// sublist body) is transformed by `f`.
fn map_list<F: Fn(Content) -> Content>(list: List, f: &F) -> List {
    List {
        ordered: list.ordered,
        items: list
            .items
            .into_iter()
            .map(|item| ListItem {
                content: f(item.content),
                sublist: item.sublist.map(|b| Box::new(map_list(*b, f))),
            })
            .collect(),
    }
}

/// Borrow-walking twin of [`map_list`]: visit every item body in `list` (and
/// nested sublists) with `f`.
fn for_each_content_in_list<F: FnMut(&Content)>(list: &List, f: &mut F) {
    for item in &list.items {
        f(&item.content);
        if let Some(sub) = &item.sublist {
            for_each_content_in_list(sub, f);
        }
    }
}

fn subst_kind(kind: ContainerKind, vars: &HashMap<String, Content>) -> ContainerKind {
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

    /// Regression: `repo()`'s cold start spawns the worker and re-borrows the
    /// cache `RefCell` in its `None` arm. A `borrow()` left in the `match`
    /// scrutinee used to live through the arms and panic ("RefCell already
    /// borrowed"). Pointing at an unreachable source keeps the worker
    /// repository-less (empty dataset) while still exercising that path, and
    /// proves the unchanged-tip path keeps the same `Rc`.
    #[test]
    fn repo_cold_start_reborrows_without_panic() {
        use compio::runtime::Runtime;
        let dir =
            std::env::temp_dir().join(format!("kolorinko_repo_nopath_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path: &'static Path = Box::leak(dir.clone().into_boxed_path());
        let meta = RepoMeta::new("file:///nonexistent/kolorinko-repo", path, 900);
        let mut cache = RepoCache::default();
        let rt = Runtime::new().unwrap();
        // Cold start (non-tick): the `None` arm — the original panic site.
        let first = rt.block_on(repo(&meta, false, &mut cache));
        assert!(find_article(&first.sites, &site("nope"), &root_slug("nope")).is_none());
        // Nothing to pull → worker returns None → the prior `Rc` is kept.
        let second = rt.block_on(repo(&meta, true, &mut cache));
        assert!(Rc::ptr_eq(&first, &second));
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

        let (sites, index) = build_from_tree(&repo, tip, &dir);

        // Two pages indexed; bodies stay as Oids until read on demand.
        assert_eq!(index.len(), 2);
        let foo = find_article(&sites, &site("scp"), &root_slug("foo")).unwrap();
        assert_eq!(
            read_body(&repo, foo.latest_body),
            Some("Foo body".to_string())
        );
        assert_eq!(foo.meta.slug, "foo");
        assert_eq!(foo.meta.page_id, "1305054470");
        let bar = find_article(&sites, &site("scp"), &root_slug("bar")).unwrap();
        assert_eq!(
            read_body(&repo, bar.latest_body),
            Some("Bar body".to_string())
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

        let (sites, mut index) = build_from_tree(&repo, tip1, &dir);
        let foo = find_article(&sites, &site("scp"), &root_slug("foo")).unwrap();
        assert_eq!(
            read_body(&repo, foo.latest_body),
            Some("Foo v1".to_string())
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
        let parent = repo.find_commit(tip1).unwrap();
        let tip2 = commit(&repo, "c2", &[&parent]);

        let affected = diff_affected_meta_paths(&repo, tip1, tip2, &dir).unwrap();
        let tree2 = repo
            .find_commit(tip2)
            .and_then(|c| repo.find_tree(c.tree_id()))
            .unwrap();
        let next = incremental_update(&repo, &tree2, &dir, &sites, &mut index, affected);

        // `bar` is structurally shared from the old snapshot; `foo` re-read.
        let foo = find_article(&next, &site("scp"), &root_slug("foo")).unwrap();
        assert_eq!(
            read_body(&repo, foo.latest_body),
            Some("Foo v2".to_string())
        );
        let bar = find_article(&next, &site("scp"), &root_slug("bar")).unwrap();
        assert_eq!(
            read_body(&repo, bar.latest_body),
            Some("Bar v1".to_string())
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
                block: true,
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
