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
//! - [`repo_l_article_latest`] (`follow` lens over `repo`): projects one
//!   page — addressed canonically, `(space, local)` resolved to its current
//!   slug through the rename-stable page id — into an owned [`ArticleLatest`]
//!   (metadata + latest body + revision list), reading the latest body blob
//!   out of the worker's cache via the [`GitMailbox`]` carried in
//!   [`RepoData`]. Shippable (`Send`: owned `String`s). This is the bridge
//!   every off-`repo`-core gear reads the dataset through.
//! - [`repo_l_local_id`] (`follow` lens over `repo`): the slug-family →
//!   canonical bridge — the `(local id, title)` a legacy `(site, slug)`
//!   address names (HTTP slug redirects, the `/code/N` endpoint, the render
//!   CLI, and the include cone inside [`article_latest`]).
//! - [`repo_l_list_pages`] (`follow` lens over `repo`): projects a ListPages
//!   module's selection into [`ListPagesResult`] — the matching pages of one
//!   site, ordered and truncated per the module's parameters.
//! - [`article_latest_parsed`] (`event`, keyed canonically, living off the
//!   `repo` core): parses the latest body — pulled through the
//!   [`repo_l_article_latest`] lens via
//!   [`secondary_get`](dentrado::core::gear::GearQuery::secondary_get) — into
//!   [`ArticleView`] with `[[include]]` directives **left unresolved**. Kept
//!   separate from [`article_latest`] so the parse gears never depend on one
//!   another (which would let two pages that include each other form a gear
//!   cycle).
//! - [`article_latest`] (`follow` over [`article_latest_parsed`], co-located
//!   with it): runs the full resolution pipeline — `[[include]]` splicing
//!   and `[[module ListPages]]` instantiation via
//!   [`secondary_get`](dentrado::core::gear::GearQuery::secondary_get)-ing
//!   [`article_latest_parsed`] (includes bridged through [`repo_l_local_id`])
//!   / [`repo_l_list_pages`] (data-level cycles broken by a path-based
//!   guard) — producing the final [`ArticleView`] with the tree of every
//!   fetched page as its `deps`. Declaring each fetch as a dependency makes
//!   the result reactive: an edit to any page in the transitive
//!   include/transclusion cone re-runs this gear.
//! - [`shell`] (`follow` over `repo`): the whole site chrome in one shot — the
//!   resolved `nav:top` / `nav:side` pages (declared as [`article_latest`]
//!   [`secondary_get`](dentrado::core::gear::GearQuery::secondary_get) deps)
//!   plus the theme-root URLs — so the client fetches the site frame under a
//!   single `site`-keyed subscription.

use crate::wikidot_parser::{parse, parse_link_target};
use compio::fs;
use dentrado::core::{core_ctx::GearCtx, gear::GearResult, storage::Storage};
use git2::{ObjectType, Oid, Repository, Tree, TreeWalkMode};
use imbl::HashMap as ImHashMap;
use kolorinko_render::{http_refs, http_tail, rewrite_with};
use kolorinko_rt::{
    Body, CaRef, CodeBlock, ListPagesQuery, ListPagesResult, ListedPage, LocalId, RepoAssetPath,
    SafePathComponent, SiteShell, SpaceId,
};
use kolorinko_wikitext::{
    ArticleLatest, ArticleMeta, ArticleView, BlockCell, BlockRow, BlockTable, ContainerKind,
    Content, Include, LinkTarget, ListOrder, ListPages, ListPagesParams, Node, PageDep, PageRef,
    RevMeta, TextObj, TimeFilter,
};
use log::error;
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    fmt,
    path::{Path, PathBuf},
    rc::Rc,
};

use crate::runtime::{GearOutShared, KolorinkoRT};

// The body is split across the submodules below. Shared crate imports live
// here and reach each submodule via `use super::*`. Internal items are
// `pub(super)`; the private globs below share the ones used across siblings
// (and, under test, the include-resolution helpers), while only the genuine
// `crate::wikidot_page::…` API is re-exported `pub(crate)`.
mod article_latest;
mod assets_gear;
mod code_block;
mod config;
mod dataset;
mod git_worker;
mod iftags;
mod includes;
mod incremental;
mod lenses;
mod listpages;
mod repo_gear;
mod resources;
#[cfg(test)]
mod tests;
mod tree_walk;
mod vars;

use article_latest::*;
use dataset::*;
use git_worker::*;
use iftags::*;
use includes::*;
use incremental::*;
use listpages::*;
use repo_gear::*;
use resources::*;
use tree_walk::*;
use vars::*;

pub(crate) use article_latest::{LatestCache, ParsedCache, article_latest, article_latest_parsed};
pub(crate) use assets_gear::{AssetCache, RepoResourceCache, asset, ca_url, repo_resource};
pub(crate) use code_block::{CodeBlockCache, code_block};
pub(crate) use config::RepoMeta;
pub(crate) use dataset::{Article, RepoData, WDWebsite};
pub(crate) use git_worker::GitMailbox;
pub(crate) use lenses::{
    RepoLArticleCache, RepoLListPagesCache, RepoLLocalIdCache, ShellCache, repo_l_article_latest,
    repo_l_list_pages, repo_l_local_id, shell,
};
pub(crate) use repo_gear::{RepoCache, repo};

/// Resolve a canonical address `(space, local)` to the dataset location that
/// serves it: the registered site for the space, plus the page's current slug
/// from its (rename-stable) page id. `None` when the space is not registered
/// in the global config, or the site has no page with that id. This is one
/// of the two bridges between the URL layer (`space`/`local`) and the
/// slug-keyed dataset below (the other direction is [`repo_l_local_id`]).
pub(crate) fn page_slug(
    data: &RepoData,
    space: SpaceId,
    local: LocalId,
) -> Option<(
    SafePathComponent,
    (Option<SafePathComponent>, SafePathComponent),
)> {
    let site = crate::globals::site_of(&space)?;
    let slug = data
        .sites
        .get(site)?
        .by_page_id
        .get(&local.page_id())?
        .clone();
    Some((site.clone(), slug))
}
