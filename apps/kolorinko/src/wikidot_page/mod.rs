//! The Wikidot-evacuation data layer and the four gears built on top of it.
//!
//! The data source is an `evakuilo` daemon's publication (its `out/`
//! directory — the configured `evakuilo.dir` — laid out one dir per site):
//! ```text
//! <out>/<site>/pages.json                    ← page manifest (id, slug,
//!                                                title, tags, revision
//!                                                counts, archive path)
//! <out>/<site>/pages_by_id/ab/cd/<id>.zst    ← one deterministic
//!                                                tar+zst per page, holding
//!                                                an rNNN.txt entry (v1
//!                                                frontmatter + text) per
//!                                                stored revision
//! <out>/<site>/files.json                    ← attachment manifest (URL,
//!                                                sha256, size, status)
//! <out>/<site>/files_ca/ab/cd/<sha[4..]>     ← content-addressed blobs
//! <out>/<site>/shell                         ← title/subtitle/theme_root
//! ```
//! The manifest is deterministic and write-if-changed, and every blob and
//! archive lands via tmp+rename — so a file's `(mtime, size)` drifting is
//! exactly "this file's content changed", and untouched files keep their
//! mtimes however often the daemon republishes.
//!
//! # Storage model
//! The dataset materialises **every latest body exactly once**: for each
//! manifest row the page's tar+zst is decompressed, every entry's
//! frontmatter joins the revision table, and the highest revision's body
//! (frontmatter stripped, NBSP-normalised) is kept as an `Arc<str>` keyed by
//! its SHA-256 — content addressing means a changed body is simply a new id,
//! so nothing is ever invalidated, only added (and pruned when no page
//! references it anymore). This whole-corpus snapshot ([`RepoSnapshot`]) is
//! what the gears distribute; RAM is the cheaper currency — the lazy
//! per-visit archive reads of the old lens layer measured as roughly half
//! the CPU of a full render. Attachments (`files_ca/`) stay lazy: `asset`
//! reads them straight from the publication's content-addressed store on
//! demand.
//!
//! Publication reading is synchronous filesystem work (manifest parsing,
//! zstd decompression), so it lives on one **dedicated worker thread**
//! ([`OutWorker`]); the oracle gear talks to it over an [`OutMailbox`] (a
//! `flume` channel) and `.await`s each reply, so every read happens off the
//! core. Memory tracks the corpus's live latest bodies, not its history.
//!
//! # Gears
//! - [`repo`] (`local` oracle): on each timer tick asks the worker to rescan
//!   + rebuild — the site-level gate is the `(mtime, size)` stamp triple of
//!   `pages.json`/`files.json`/`shell` (three `stat`s per site; an idle
//!   daemon costs nothing), and inside a drifted site only the pages whose
//!   manifest rows changed are re-read (row equality implies archive
//!   equality — the publisher only rewrites an archive when its row drifts).
//!   An unchanged publication yields `None`, so the prior `Rc` is kept and
//!   dependents aren't re-run for nothing. Batching falls out of the timer:
//!   a tick observes whatever landed since the last one.
//! - [`repo_snap`] (the one `follow` lens over `repo`): publishes the
//!   snapshot as a cross-core `shared` value — an O(1) structural clone, so
//!   every consumer core holds the whole corpus by reference. This single
//!   bridge replaced the whole `repo_l_*` lens family (one cross-core read
//!   per consuming gear *run*, none per hop): page projections, slug→id
//!   bridges, link-set queries, ListPages selections, and the `files/`
//!   index are all local lookups off it ([`latest`] et al.).
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
//!   [`local_id`], data-level cycles broken by a path-based
//!   guard), then parsed as one page and resolved: `[[module ListPages]]`
//!   instantiation, internal links, and mirrored resources — all local
//!   snapshot reads — producing the final [`ArticleView`] with the tree of
//!   every fetched page as its `deps`. Reactivity rides the single
//!   `repo_snap` dependency: any dataset change re-runs this gear, and the
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
mod iftags;
mod includes;
mod incremental;
mod lenses;
mod links;
mod listpages;
mod out_walk;
mod out_worker;
mod repo_gear;
mod resources;
#[cfg(test)]
mod tests;
mod vars;

use article_latest::*;
use dataset::*;
use iftags::*;
use includes::*;
use incremental::*;
use links::*;
use listpages::*;
use out_walk::*;
use out_worker::*;
use resources::*;
use vars::*;

pub(crate) use article_latest::{LatestCache, ParsedCache, article_latest, article_latest_parsed};
pub(crate) use assets_gear::{AssetCache, asset, ca_url};
pub(crate) use code_block::{CodeBlockCache, code_block};
pub(crate) use config::OutMeta;
pub(crate) use dataset::{article, latest, list_pages, local_id, query_pages, resource};
pub(crate) use lenses::{RepoSnapCache, ShellCache, repo_snap, shell};
pub(crate) use repo_gear::{RepoCache, repo};

/// Resolve a canonical address `(space, local)` to the dataset location that
/// serves it: the registered site for the space, plus the page's current slug
/// from its (rename-stable) page id. `None` when the space is not registered
/// in the global config, or the site has no page with that id. This is one
/// of the two bridges between the URL layer (`space`/`local`) and the
/// slug-keyed dataset below (the other direction is
/// [`local_id`]).
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
