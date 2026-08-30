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
//! The dataset materialises **every latest body exactly once**: the tree walk
//! reads each page's blob out of the git object database, strips its
//! frontmatter, normalises NBSPs, and keeps the text as an `Arc<str>` —
//! content addressing means a changed body is simply a new oid, so nothing
//! is ever invalidated, only added (and pruned when no page references it
//! anymore). This whole-corpus snapshot ([`RepoSnapshot`]) is what the gears
//! distribute; RAM is the cheaper currency — the lazy per-visit odb reads of
//! the old lens layer measured as roughly half the CPU of a full render.
//! Attachments (`files/`) stay lazy: `asset` reads them straight from the
//! worktree's content-addressed `_files/` store on demand.
//!
//! libgit2 is synchronous and [`git2::Repository`] is `!Send`, so calling it
//! straight from a gear would block the whole async core. Instead the
//! `!Send` `Repository`, the reverse [`Index`], the current sites map, and
//! the materialised-body store all live on one **dedicated worker thread**
//! ([`GitWorker`]), created and pinned there — the `Repository` is never
//! moved across a thread. The oracle gear talks to it over a
//! [`GitMailbox`] (a `flume` channel) and `.await`s each reply, so every
//! libgit2 call happens off the core. Memory tracks the corpus's live
//! latest bodies, not the repository's history.
//!
//! # Gears
//! - [`repo`] (`local` oracle): on each timer tick asks the worker to fetch
//!   + rebuild — incrementally, only the pages the `old_tip → new_tip` diff
//!   touched (new bodies materialised into the persistent store, stale ones
//!   pruned) — and adopts the worker's new [`RepoSnapshot`] (wrapped in
//!   [`Rc`]). An unchanged tip yields `None`, so the prior `Rc` is kept and
//!   dependents aren't re-run for nothing.
//! - [`repo_snap`] (the one `follow` lens over `repo`): publishes the
//!   snapshot as a cross-core `shared` value — an O(1) structural clone, so
//!   every consumer core holds the whole corpus by reference. This single
//!   bridge replaced the whole `repo_l_*` lens family (one cross-core read
//!   per consuming gear *run*, none per hop): page projections, slug→id
//!   bridges, link-set queries, ListPages selections, and the `files/`
//!   index are all local lookups off it ([`RepoSnapshot::latest`] et al.).
//! - [`article_latest_parsed`] (`event`, keyed canonically, living off the
//!   `repo` core): parses the latest body — read out of the one
//!   [`repo_snap`] dependency per run — into [`ArticleView`] with
//!   `[[include]]` directives **left unresolved**. Kept separate from
//!   [`article_latest`] so the parse gears never depend on one another
//!   (which would let two pages that include each other form a gear cycle).
//! - [`article_latest`] (`follow` over [`article_latest_parsed`], co-located
//!   with it): runs the full resolution pipeline — the page's raw body read
//!   out of the same snapshot and assembled textually (`[[include]]` cones
//!   spliced into the raw text with their `{$vars}` substituted, Wikidot's
//!   own order of operations, includes bridged through
//!   [`RepoSnapshot::local_id`], data-level cycles broken by a path-based
//!   guard), then parsed as one page and resolved: `[[module ListPages]]`
//!   instantiation, internal links, and mirrored resources — all local
//!   snapshot reads — producing the final [`ArticleView`] with the tree of
//!   every fetched page as its `deps`. Reactivity rides the single
//!   `repo_snap` dependency: any repo change re-runs this gear, and the
//!   body/parse caches keep unchanged pages cheap.
//! - [`shell`] (`follow` over `repo`): the whole site chrome in one shot —
//!   the resolved `nav:top` / `nav:side` pages (declared as
//!   [`article_latest`]
//!   [`secondary_get`](dentrado::core::gear::GearQuery::secondary_get) deps)
//!   plus the theme-root URLs — so the client fetches the site frame under a
//!   single `site`-keyed subscription.

use crate::wikidot_parser::{parse, parse_link_target, wikidot_verbatim};
use compio::fs;
use dentrado::core::{core_ctx::GearCtx, gear::GearResult, storage::Storage};
use git2::{ObjectType, Oid, Repository, Tree, TreeWalkMode};
use imbl::HashMap as ImHashMap;
use kolorinko_render::{http_refs, http_tail, rewrite_with};
use kolorinko_rt::{
    Article, BlobId, Body, CaRef, CodeBlock, ListPagesQuery, ListPagesResult, ListedPage, LocalId,
    PageQuery, PageQueryResult, RepoAssetPath, RepoSnapshot, SafePathComponent, SiteShell, Slug,
    SpaceId, WDWebsite,
};
use kolorinko_wikitext::{
    ArticleMeta, ArticleView, BlockCell, BlockRow, BlockTable, ContainerKind, Content, Include,
    LinkTarget, ListOrder, ListPages, ListPagesParams, Node, PageDep, PageRef, RevMeta, TextObj,
    TimeFilter,
};
use log::error;
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    fmt,
    path::{Path, PathBuf},
    rc::Rc,
    sync::Arc,
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
mod links;
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
use links::*;
use listpages::*;
use repo_gear::*;
use resources::*;
use tree_walk::*;
use vars::*;

pub(crate) use article_latest::{LatestCache, ParsedCache, article_latest, article_latest_parsed};
pub(crate) use assets_gear::{AssetCache, asset, ca_url};
pub(crate) use code_block::{CodeBlockCache, code_block};
pub(crate) use config::RepoMeta;
pub(crate) use dataset::{article, latest, list_pages, local_id, query_pages, resource};
pub(crate) use lenses::{RepoSnapCache, ShellCache, repo_snap, shell};
pub(crate) use repo_gear::{RepoCache, repo};

/// Resolve a canonical address `(space, local)` to the dataset location that
/// serves it: the registered site for the space, plus the page's current slug
/// from its (rename-stable) page id. `None` when the space is not registered
/// in the global config, or the site has no page with that id. This is one
/// of the two bridges between the URL layer (`space`/`local`) and the
/// slug-keyed dataset below (the other direction is
/// [`RepoSnapshot::local_id`]).
pub(crate) fn page_slug(
    data: &RepoSnapshot,
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
