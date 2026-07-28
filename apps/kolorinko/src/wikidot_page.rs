use crate::{
    safe_path::SafePathComponent,
    wikidot_parser::{parse, types::Content},
};
use dentrado::core::core_ctx::GearCtx;
use git2::{Oid, Repository};
use im::HashMap as ImHashMap;
use log::error;
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    fs,
    path::{Component, Path, PathBuf},
    rc::Rc,
    sync::Arc,
};

use crate::runtime::{GearId, GearOut, KolorinkoRT};

/// Configuration for the `repo` oracle gear: where to clone and how often to
/// re-pull. `path` is the on-disk clone location — a piece of *configuration*
/// (not a gear output), so holding a `&'static Path` here is fine; it never
/// appears in a `GearOut`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct RepoMeta {
    url: &'static str,
    path: &'static Path,
    interval: u32,
}

impl RepoMeta {
    /// Construct a new `RepoMeta`.
    ///
    /// `url` and `path` must live for `'static`; for a path read at runtime
    /// (e.g. from an env var), leak the backing storage with `Box::leak`.
    /// `interval` is the timer period in seconds (the gap between two
    /// `git pull`s of the oracle gear).
    #[must_use]
    pub(crate) const fn new(url: &'static str, path: &'static Path, interval: u32) -> Self {
        Self {
            url,
            path,
            interval,
        }
    }

    /// The timer period in seconds between two re-pulls.
    #[must_use]
    pub(crate) const fn interval(&self) -> u32 {
        self.interval
    }
}

/// Identity of a page: `(site, category, name)`. `category` is `None` for
/// uncategorized pages.
type PageKey = (
    SafePathComponent,
    Option<SafePathComponent>,
    SafePathComponent,
);

/// All data mirrored out of the repository at a given point in time: a map
/// from page identity `(site, category, name)` to the **raw body text** of its
/// highest-numbered revision.
///
/// This is the "huge in-memory data structure" the `repo` oracle holds. It is
/// returned as an immutable `Arc` so dependents (`load_page` gears) can read it
/// without touching the filesystem — the `repo` gear is the *only* gear that
/// touches the working tree. `repo` deliberately stores *text*, not parsed
/// trees: parsing is expensive (deep recursive combinator work) and most pages
/// are never viewed, so it is deferred to [`load_page`], which parses lazily
/// and caches the result per page.
///
/// The map is a *persistent* [`im::HashMap`]: cloning it is O(1) (structural
/// sharing) and an `insert`/`remove` is O(log n) and **non-destructive** — it
/// produces a new map that shares almost all of its structure with the old
/// one. That is what makes incremental updates cheap: on a tick the `repo`
/// gear patches only the pages the git diff touched, producing a fresh
/// `Arc<RepoData>` for almost the cost of the changed entries, while any
/// dependent still holding the previous `Arc` sees a stable, unchanging
/// snapshot. It also makes cache invalidation in [`load_page`] a pointer check:
/// unchanged pages keep their *original* `Arc<str>` allocation across snapshots
/// (structural sharing), so a page whose text didn't change is recognised in
/// O(1) and never re-parsed.
#[derive(Default, Clone, Debug)]
pub(crate) struct RepoData {
    pages: ImHashMap<PageKey, Arc<str>>,
}

impl RepoData {
    /// Number of pages held in the structure (diagnostic).
    #[must_use]
    pub(crate) fn page_count(&self) -> usize {
        self.pages.len()
    }

    /// Look up a page's raw body text by `(site, category, name)`.
    #[must_use]
    pub(crate) fn get(
        &self,
        site: &SafePathComponent,
        slug: &(Option<SafePathComponent>, SafePathComponent),
    ) -> Option<Arc<str>> {
        self.pages
            .get(&(site.clone(), slug.0.clone(), slug.1.clone()))
            .cloned()
    }
}

/// Per-instance cache for the `repo` gear: the opened `git2::Repository`
/// handle (kept across ticks so we don't re-open every period), the last-seen
/// commit tip (so a tick can diff old→new instead of rebuilding), and the
/// last-built [`RepoData`] (returned unchanged on a `tick = false` run).
///
/// `git2::Repository` is neither `Clone` nor `Debug`, but the gear cache must
/// be both (it is cloned out of the arena around each `run_step` await and
/// erased behind `Box<dyn Any>`). Wrapping the state in `Rc<RefCell<…>>` makes
/// the cache cheaply `Clone` (refcount bump), and a manual `Debug` impl avoids
/// requiring `Repository: Debug`. The `RefCell` guards only the *handle* (and
/// the `Arc<RepoData>` swap); the dataset itself is updated purely — each tick
/// produces a new `Arc<RepoData>` rather than mutating the old one in place.
#[derive(Default, Clone)]
pub(crate) struct RepoCache(Rc<RefCell<RepoInner>>);

#[derive(Default)]
struct RepoInner {
    /// `None` until the first successful `open_or_clone`.
    repo: Option<Repository>,
    /// The commit tip the current `data` was built from; `None` until the
    /// first build. Drives the incremental `old_tip → new_tip` diff.
    last_tip: Option<Oid>,
    /// The last fully-built dataset. `None` until the first tick / first build.
    data: Option<Arc<RepoData>>,
}

impl std::fmt::Debug for RepoCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("RepoCache").finish()
    }
}

/// Run the `repo` oracle. On a tick (`tick = true`):
/// - fetch + hard-reset to the new remote tip;
/// - if the tip *and* a prior dataset exist and differ, **incrementally**
///   patch the persistent map with only the pages the `git diff old→new`
///   touched (re-reading each affected page's latest revision from the working
///   tree), producing a new `Arc<RepoData>` that shares almost all its
///   structure with the old one;
/// - otherwise (first build, or tip unchanged) rebuild fully or reuse the
///   existing `Arc`.
///
/// The first ever run is always a tick (the gear is created with `next_due =
/// 0`), so the dataset is populated before any dependent reads it. A
/// fetch/parse failure leaves the last good dataset in place and is retried
/// next tick. On a non-tick run the previously-built dataset is returned
/// unchanged.
pub(crate) fn repo(meta: &RepoMeta, tick: bool, cache: &mut RepoCache) -> Arc<RepoData> {
    let mut inner = cache.0.borrow_mut();
    // Open (or clone) the repository lazily on first use.
    if inner.repo.is_none() {
        inner.repo = open_or_clone(meta.url, meta.path);
    }
    if tick {
        match inner.repo.as_ref().zip(pull_for_diff(inner.repo.as_ref())) {
            Some((_, PullOutcome::SameTip)) => {
                // Tip unchanged since last tick — nothing to do.
            }
            Some((r, PullOutcome::Updated { new_tip })) => {
                let new_data = match (inner.last_tip, inner.data.as_ref()) {
                    // Have a prior dataset built from a different tip: patch it
                    // incrementally. If the diff can't be computed (e.g. the old
                    // tip was garbage-collected away under a force-push), fall
                    // back to a full rebuild.
                    (Some(old_tip), Some(old_data)) if old_tip != new_tip => {
                        match incremental_update(r, old_data, old_tip, new_tip, meta.path) {
                            Some(new) => new,
                            None => Arc::new(build_all(meta.path)),
                        }
                    }
                    // No prior data (first build) — walk the whole tree.
                    _ => Arc::new(build_all(meta.path)),
                };
                inner.last_tip = Some(new_tip);
                inner.data = Some(new_data);
            }
            None => {
                // Fetch failed: keep serving the last good dataset; retry next tick.
            }
        }
    }
    // `tick = false`, or a tick whose fetch failed before any dataset existed
    // (first ever run): build from the current tree without pulling, so callers
    // always get *some* dataset.
    if inner.data.is_none() {
        inner.last_tip = current_tip(inner.repo.as_ref());
        inner.data = Some(Arc::new(build_all(meta.path)));
    }
    Arc::clone(inner.data.as_ref().expect("dataset populated above"))
}

/// The outcome of a `git fetch` + hard-reset: either the tip moved to a new
/// commit, or it stayed the same (no new commits since last pull).
enum PullOutcome {
    /// `new_tip == last_tip`: nothing changed upstream.
    SameTip,
    /// Upstream advanced (or force-pushed) to `new_tip`.
    Updated { new_tip: Oid },
}

/// Fetch from `origin` (force-updating local branches) and hard-reset the
/// working tree to the fetched tip. Returns the new tip, classified against
/// the previous tip so the caller can skip work when nothing changed. Returns
/// `None` if the fetch/reset failed (logged); the caller keeps serving the
/// last good dataset.
fn pull_for_diff(repo: Option<&Repository>) -> Option<PullOutcome> {
    let repo = repo?;
    let old_tip = current_tip(Some(repo));
    match try_pull(repo) {
        Ok(new_tip) => {
            if Some(new_tip) == old_tip {
                Some(PullOutcome::SameTip)
            } else {
                Some(PullOutcome::Updated { new_tip })
            }
        }
        Err(e) => {
            error!("Failed to pull the repository: {e}");
            None
        }
    }
}

/// The current `HEAD` commit id, or `None` if `HEAD` is unborn/missing.
fn current_tip(repo: Option<&Repository>) -> Option<Oid> {
    let repo = repo?;
    repo.head().ok().and_then(|r| r.target())
}

fn try_pull(repo: &Repository) -> Result<Oid, git2::Error> {
    let mut remote = repo.find_remote("origin")?;
    // Force-update all local branches from the remote and record the tip in FETCH_HEAD.
    remote.fetch(&["+refs/heads/*:refs/heads/*"], None, None)?;
    let fetched = repo.revparse_single("FETCH_HEAD")?;
    let new_tip = fetched.id();
    repo.reset(&fetched, git2::ResetType::Hard, None)?;
    Ok(new_tip)
}

/// Produce a new `Arc<RepoData>` from `old_data` by applying only the pages
/// the `old_tip → new_tip` diff touched. Each affected page is recomputed from
/// the (post-reset) working tree — re-reading its latest revision, or removed
/// if the page directory is gone. `old_data` is left untouched: the persistent
/// map is cloned O(1) and patched non-destructively, so any dependent still
/// holding the old `Arc` keeps a consistent snapshot.
fn incremental_update(
    repo: &Repository,
    old_data: &Arc<RepoData>,
    old_tip: Oid,
    new_tip: Oid,
    root: &Path,
) -> Option<Arc<RepoData>> {
    let affected = diff_affected_keys(repo, old_tip, new_tip)?;
    if affected.is_empty() {
        // No page revisions changed (e.g. only non-page files touched): the old
        // dataset is still current; just hand back another handle to it.
        return Some(Arc::clone(old_data));
    }
    let mut pages = old_data.pages.clone();
    for key in affected {
        let page_dir = page_dir_for_key(root, &key);
        match read_latest(&page_dir) {
            Some(content) => {
                pages.insert(key, content);
            }
            None => {
                pages.remove(&key);
            }
        }
    }
    Some(Arc::new(RepoData { pages }))
}

/// Collect the set of page keys touched by the `old_tip → new_tip` diff. Each
/// changed file path is mapped to its page key via [`path_to_page_key`];
/// multiple revisions of the same page collapse to one key. Paths outside the
/// `<site>/pages/…/r{N}.txt` layout are ignored (top-level files, READMEs,
/// etc. don't correspond to any page).
///
/// Returns `None` if either commit/tree or the diff can't be loaded (e.g. the
/// old tip was garbage-collected away under a force-push); the caller then
/// falls back to a full rebuild rather than serving a stale dataset.
fn diff_affected_keys(repo: &Repository, old_tip: Oid, new_tip: Oid) -> Option<HashSet<PageKey>> {
    let mut affected = HashSet::new();
    let old_tree = repo.find_commit(old_tip).ok()?.tree().ok()?;
    let new_tree = repo.find_commit(new_tip).ok()?.tree().ok()?;
    let diff = repo
        .diff_tree_to_tree(Some(&old_tree), Some(&new_tree), None)
        .ok()?;
    for delta in diff.deltas() {
        // A rename/copy has both; an add has only new; a delete has only old.
        // Recompute every affected page uniformly from the working tree, so we
        // just collect both sides' page keys.
        if let Some(p) = delta.old_file().path() {
            affected.extend(path_to_page_key(p));
        }
        if let Some(p) = delta.new_file().path() {
            affected.extend(path_to_page_key(p));
        }
    }
    Some(affected)
}

/// Map a repository-relative revision-file path to its page key, or `None` if
/// the path isn't a page revision. (`diff` delta paths are repo-relative, so no
/// repo-root prefix is needed.) The layout is:
///   `<site>/pages/<name>/r{N}.txt`        → `(site, None, name)`
///   `<site>/pages/<cat>/<name>/r{N}.txt`  → `(site, Some(cat), name)`
/// Depth disambiguates the two cases unambiguously: 4 components =
/// uncategorized page, 5 = categorized. This matches exactly what
/// [`build_all`] walks.
fn path_to_page_key(rel: &Path) -> Option<PageKey> {
    let comps: Vec<Component<'_>> = rel.components().collect();
    let rev = comps.last()?.as_os_str().to_str()?;
    rev.strip_prefix('r')?.strip_suffix(".txt")?;
    // Names must be valid safe path components.
    let as_safe =
        |c: &Component<'_>| SafePathComponent::new(c.as_os_str().to_string_lossy().into());
    match comps.as_slice() {
        // <site>/pages/<name>/r{N}.txt
        [site, pages, name, _rev] if pages.as_os_str() == "pages" => {
            Some((as_safe(site)?, None, as_safe(name)?))
        }
        // <site>/pages/<cat>/<name>/r{N}.txt
        [site, pages, cat, name, _rev] if pages.as_os_str() == "pages" => {
            Some((as_safe(site)?, Some(as_safe(cat)?), as_safe(name)?))
        }
        _ => None,
    }
}

/// The working-tree directory holding the revisions of a page, given its key
/// and the repo `root`. The inverse of [`path_to_page_key`]'s layout.
fn page_dir_for_key(root: &Path, key: &PageKey) -> PathBuf {
    let (site, category, name) = key;
    let mut dir = root.join(site).join("pages");
    if let Some(category) = category {
        dir = dir.join(category);
    }
    dir.join(name)
}

/// Open the repository at `path`, cloning it from `url` if it has not been cloned yet.
///
/// `Repository::clone` fails when the directory already exists, so on a restart we open
/// the existing clone instead of re-cloning.
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

/// Walk the whole working tree and build a [`RepoData`] holding the latest
/// revision of every page. Used for the initial build (and as a fallback); the
/// steady state uses [`incremental_update`] instead.
///
/// The on-disk layout is:
/// ```text
/// <root>/<site>/pages/<name>/r{N}.txt
/// <root>/<site>/pages/<category>/<name>/r{N}.txt
/// ```
/// A directory directly containing `r{N}.txt` files is a *page* (category
/// `None`); a directory whose entries are themselves page directories is a
/// *category*. The two cases are distinguished by whether `latest_revision`
/// finds revision files immediately inside.
fn build_all(root: &Path) -> RepoData {
    let mut pages = ImHashMap::new();
    let Ok(site_entries) = fs::read_dir(root) else {
        return RepoData::default();
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
        let pages_dir = site_path.join("pages");
        let Ok(cat_entries) = fs::read_dir(&pages_dir) else {
            continue;
        };
        for cat_entry in cat_entries.flatten() {
            let cat_path = cat_entry.path();
            let cat_name = cat_entry.file_name().to_string_lossy().into_owned();
            // Direct page (category = None): this entry holds r{N}.txt files.
            if latest_revision(&cat_path).is_some() {
                if let Some(name) = SafePathComponent::new(cat_name.clone())
                    && let Some(content) = read_latest(&cat_path)
                {
                    pages.insert((site.clone(), None, name), content);
                }
                continue;
            }
            // Otherwise a category: walk its page subdirectories.
            if !cat_path.is_dir() {
                continue;
            }
            let Some(category) = SafePathComponent::new(cat_name) else {
                continue;
            };
            let Ok(page_entries) = fs::read_dir(&cat_path) else {
                continue;
            };
            for page_entry in page_entries.flatten() {
                let page_path = page_entry.path();
                let Some(name) =
                    SafePathComponent::new(page_entry.file_name().to_string_lossy().into())
                else {
                    continue;
                };
                if let Some(content) = read_latest(&page_path) {
                    pages.insert((site.clone(), Some(category.clone()), name), content);
                }
            }
        }
    }
    RepoData { pages }
}

/// Read the body of the highest-numbered `r{N}.txt` revision in `dir` as raw
/// text (frontmatter header stripped), returning `None` (with a logged error)
/// if the directory holds no revision or the read fails. The text is returned
/// as `Arc<str>` so it can be stored directly in the persistent map and compared
/// by pointer identity for cache invalidation. Parsing is *not* done here — it
/// is deferred to [`load_page`], which only parses pages actually viewed.
fn read_latest(dir: &Path) -> Option<Arc<str>> {
    let latest = latest_revision(dir)?;
    match fs::read_to_string(&latest.1) {
        Ok(text) => {
            let (_header, body) = parse_revision(&text);
            Some(Arc::from(body))
        }
        Err(e) => {
            error!("Failed to read {}: {e}", latest.1.display());
            None
        }
    }
}

/// A key from a revision file's YAML-like frontmatter header.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum HeaderKey {
    Title,
    Tags,
    PageId,
    Site,
    Slug,
    Revision,
    RevisionId,
    Author,
    Timestamp,
}

impl HeaderKey {
    /// Map a raw header key string to a [`HeaderKey`], if recognised.
    fn from_key(key: &str) -> Option<Self> {
        Some(match key {
            "title" => Self::Title,
            "tags" => Self::Tags,
            "page_id" => Self::PageId,
            "site" => Self::Site,
            "slug" => Self::Slug,
            "revision" => Self::Revision,
            "revision_id" => Self::RevisionId,
            "author" => Self::Author,
            "timestamp" => Self::Timestamp,
            _ => return None,
        })
    }
}

/// Return the highest-numbered `r{N}.txt` revision in `dir` as `(N, path)`, or
/// `None` if the directory holds no revision file (used both to detect page
/// directories and to pick the latest revision).
fn latest_revision(dir: &Path) -> Option<(u64, PathBuf)> {
    fs::read_dir(dir)
        .ok()?
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let name = entry.file_name();
            let name = name.to_str()?;
            let number = name.strip_prefix('r')?.strip_suffix(".txt")?;
            let number: u64 = number.parse().ok()?;
            Some((number, entry.path()))
        })
        .max_by_key(|(number, _)| *number)
}

/// Split a revision file into its frontmatter header and body.
///
/// The header is a flat list of `key: value` lines wrapped between two `---`
/// delimiter lines. String values may be double-quoted; the surrounding quotes
/// are stripped. Unrecognised keys are ignored.
fn parse_revision(text: &str) -> (HashMap<HeaderKey, String>, &str) {
    let mut header = HashMap::new();
    let rest = match text.strip_prefix("---\n") {
        Some(rest) => rest,
        None => return (header, text),
    };
    let (header_text, body) = match rest.find("\n---\n") {
        Some(end) => (&rest[..end], &rest[end + "\n---\n".len()..]),
        None => return (header, text),
    };
    for line in header_text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        if let Some(key) = HeaderKey::from_key(key.trim()) {
            header.insert(key, strip_quotes(value.trim()).to_string());
        }
    }
    (header, body)
}

/// Strip a single layer of surrounding double quotes from `s`, if present.
fn strip_quotes(s: &str) -> &str {
    if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

/// `load_page` gear cache: the last `Arc<str>` we indexed out of `repo`'s
/// [`RepoData`] and the [`Content`] we parsed from it. Because `repo`'s map is
/// persistent ([`im::HashMap`]), unchanged pages keep their *original*
/// `Arc<str>` allocation across snapshots, so a page that didn't change since
/// last run is recognised by [`Arc::ptr_eq`] in O(1) and its cached parse is
/// reused — only a genuinely-changed page is re-parsed.
#[derive(Default, Clone, Debug)]
pub(crate) struct LoadCache {
    text: Option<Arc<str>>,
    content: Option<Arc<Content>>,
}

/// Load a single page: depend on the `repo` oracle, look the page's raw body
/// text up in the dataset it built, parse it into [`Content`], and cache the
/// result. No filesystem access — the working tree is touched only by the
/// `repo` gear. Parsing is deferred here rather than in `repo` so only
/// pages actually viewed are ever parsed, and a page whose text is unchanged
/// since last run is not re-parsed (pointer-identity check on the persistent
/// map's shared `Arc<str>`).
pub(crate) async fn load_page(
    meta: &RepoMeta,
    site: &SafePathComponent,
    slug: &(Option<SafePathComponent>, SafePathComponent),
    ctx: &mut GearCtx<KolorinkoRT>,
    cache: &mut LoadCache,
) -> Arc<Content> {
    let GearOut::RepoOut(data) = ctx.secondary_get(GearId::Repo(meta.clone())).await else {
        unreachable!("repo gear must produce RepoOut")
    };
    let Some(text) = data.get(site, slug) else {
        // Page no longer present in the dataset: drop any stale cached parse
        // and return empty content.
        *cache = LoadCache::default();
        return Arc::new(Content::new());
    };
    // Incremental invalidation by pointer identity. `repo`'s persistent map
    // shares unchanged `Arc<str>` entries across snapshots, so if the current
    // text is the *same allocation* we parsed last time, the cached parse is
    // still valid — reuse it. Only a genuinely-changed page re-parses.
    if let Some(prev) = &cache.text
        && Arc::ptr_eq(prev, &text)
    {
        return Arc::clone(
            cache
                .content
                .as_ref()
                .expect("content present whenever text is"),
        );
    }
    let content = Arc::new(parse(&text));
    *cache = LoadCache {
        text: Some(Arc::clone(&text)),
        content: Some(Arc::clone(&content)),
    };
    content
}
