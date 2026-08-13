use super::*;

// =========================================================================
// Dataset
// =========================================================================

/// `(Option<category>, name)` — the per-site page address. `None` category =
/// a root page (slug has no `:`).
pub(super) type Slug = (Option<SafePathComponent>, SafePathComponent);

/// `(site, Option<category>, name)` — the full address of a page within the
/// dataset. Used both as the include-resolution visited key and as the
/// incremental-update reverse-index value (`_meta` path → its nested-map key).
pub(super) type Key = (
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
    pub(super) sites: ImHashMap<SafePathComponent, WDWebsite>,
    pub(super) mailbox: GitMailbox,
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
    pub(super) fn empty(mailbox: GitMailbox) -> Self {
        Self {
            sites: ImHashMap::new(),
            mailbox,
        }
    }

    /// Look up one page by `(site, slug)`.
    #[must_use]
    pub(super) fn article(&self, site: &SafePathComponent, slug: &Slug) -> Option<&Article> {
        find_article(&self.sites, site, slug)
    }
}

/// The nested-map lookup underlying [`RepoData::article`], factored out so the
/// build/incremental tests can resolve a page from a bare sites map.
pub(super) fn find_article<'a>(
    sites: &'a ImHashMap<SafePathComponent, WDWebsite>,
    site: &SafePathComponent,
    slug: &Slug,
) -> Option<&'a Article> {
    sites.get(site)?.articles.get(&slug.0)?.get(&slug.1)
}

/// One mirrored site: its pages nested by category; the site chrome from
/// `<site>/shell` (title, subtitle, and the theme-root path into `files/`);
/// and the content-addressed `files/` index — each mirrored attachment's
/// `<host>/<path>` tail (percent-decoded) mapped to its [`CaRef`] (read from
/// the `files/` symlink target, which points into the `_files/<xx>/<yy>/<hash>`
/// blob store). The index resolves in-article URLs ([`repo_resource`]) and the
/// theme root ([`shell`]).
#[derive(Default, Clone, Debug)]
pub(crate) struct WDWebsite {
    pub(super) articles:
        ImHashMap<Option<SafePathComponent>, ImHashMap<SafePathComponent, Article>>,
    pub(super) title: Option<String>,
    pub(super) subtitle: Option<String>,
    /// The theme stylesheet's `<host>/<path>` tail (`files/` prefix stripped);
    /// resolved against [`WDWebsite::files`] to a CA URL by the [`shell`] gear.
    pub(super) theme_root: Option<RepoAssetPath>,
    pub(super) files: ImHashMap<RepoAssetPath, CaRef>,
}

/// One page: metadata, the full revision-history summary, and blob Oids for the
/// latest body and **every** revision body. Bodies are never materialised here
/// — they live in the git object database, paged in lazily via
/// [`GitMailbox::blob`].
#[derive(Clone, Debug)]
pub(crate) struct Article {
    pub(super) meta: ArticleMeta,
    pub(super) latest_body: Oid,
    pub(super) revisions: Vec<RevMeta>,
    /// Every revision body's blob Oid (cheap; not text). Read on demand by the
    /// postponed `repo_l_article_revision` gear.
    #[allow(dead_code)]
    pub(super) bodies: ImHashMap<u64, Oid>,
}
