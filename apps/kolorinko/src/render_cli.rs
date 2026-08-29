//! `kolorinko render [config] <target>` — one-shot SSR debug renderer.
//!
//! A `<site>/<page>` target renders one page to stdout; a bare `<site>`
//! target **mass renders** the entire site into a directory of standalone
//! `.html` files (default `.kolorinko/render/<site>`, `--out` to choose
//! another, `--force` to replace a non-empty one) — one file per page, laid
//! out like the site's routes (`<category>/<page>.html`, `<page>.html` for
//! root-category pages) and meant for LLM consumption, with an `index.html`
//! listing every page as the entry point.
//!
//! Both modes spin up the gear runtime (no network servers) and resolve pages
//! plus the site shell via `GearQuery::subscribe` (the same path the live
//! server's SSR response and [`crate::server::run_session`] use, so the whole
//! `repo → repo_l_article_latest → article_latest_parsed → article_latest`
//! cone runs — git clone, parse, `[[include]]` resolution — exactly as in
//! production), SSR-render each result through [`kolorinko_render`] into a
//! self-contained HTML document, and exit.
//!
//! Subscriptions stay live until every output is read, so the shared `repo`
//! oracle is computed once (one `git clone`) rather than re-cloned per page:
//! the single-page render holds page + shell; the mass render holds a
//! `repo_l_list_pages` selection of *every* page (hidden `_`-prefixed ones
//! included, `fullname` ascending, pagination disabled) plus the shell across
//! the whole run.
//!
//! The mass render is multi-core: parallelism rides the gear runtime's own
//! routing (each page's `article_latest` gear has a deterministic owner core
//! — group hash → jump-consistent hash), so every core's worker renders
//! exactly the pages it owns and the parse/`[[include]]` work spreads across
//! cores, while the `repo` oracle, the shell, and the page enumeration stay
//! single shared instances wherever routing puts them. Within one core, a
//! small bounded window of pages ([`IN_FLIGHT_PAGES`]) is kept in flight, so
//! each page's await phases (blob materialisation on the git worker thread,
//! cross-core include resolution) overlap with neighbouring pages' parse and
//! render CPU instead of idling the core.

use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc;
use std::time::Instant;

use dentrado::core::{core_ctx::Core, storage::InMemoryStorage};
use futures::{FutureExt, StreamExt, stream};
use kolorinko_render::render_page_document;
use kolorinko_rt::{
    ListPagesQuery, ListPagesResult, ListedPage, SafePathComponent, SiteShell, Slug, parse_route,
};
use kolorinko_wikitext::{ArticleView, ListOrder, ListPagesParams};
use log::info;

use crate::runtime::KolorinkoRT;
use crate::{Config, db_config, globals};

/// Entry point for `kolorinko render …` (the leading `render` already
/// consumed). A target with a `/` names one page (stdout render); a bare
/// `<site>` mass-renders the whole site.
pub(crate) fn run_cli(
    config_path: PathBuf,
    target: &str,
    inject: bool,
    out: Option<PathBuf>,
    force: bool,
) -> anyhow::Result<()> {
    let config: Config = toml::from_str(&std::fs::read_to_string(&config_path)?)
        .map_err(|e| anyhow::anyhow!("failed to parse {}: {e}", config_path.display()))?;
    let target = target.trim_end_matches('/');
    if target.contains('/') {
        if out.is_some() {
            anyhow::bail!("--out only applies to a whole-site render (bare <site> target)");
        }
        if force {
            anyhow::bail!("--force only applies to a whole-site render (bare <site> target)");
        }
        run(config, target, inject)
    } else {
        run_mass(config, target, inject, out, force)
    }
}

fn run(config: Config, page: &str, inject: bool) -> anyhow::Result<()> {
    let (site, slug) = parse_page(page)?;
    crate::init_globals(&config)?;
    let space = space_of_site(&site)?;

    // One core is enough for a single render; it keeps the `repo` oracle,
    // every lens, and the parse gears co-located, so the follow/secondary_get
    // cone never crosses a thread.
    let cores = std::num::NonZero::new(1).unwrap();

    let (tx, rx) = std::sync::mpsc::channel::<anyhow::Result<(ArticleView, SiteShell)>>();
    let worker = move |core: Rc<Core<KolorinkoRT, InMemoryStorage<KolorinkoRT>>>| {
        let tx = tx.clone();
        // Clone the captures into fresh locals here (not inside the async
        // block) so the worker closure itself stays `Fn`.
        let site = site.clone();
        let slug = slug.clone();
        async move {
            // The page is named by its slug on the CLI: bridge to the
            // canonical `(space, local)` identity the gears address, then
            // subscribe page + shell (holding both keeps the shared `repo`
            // oracle active across the queries — one clone total).
            let id_q = crate::runtime::repo_l_local_id(site.clone(), slug.clone());
            let Some((local, _title)) = (*id_q.subscribe(&core).await.current()).clone() else {
                let _ = tx.send(Err(anyhow::anyhow!("page {slug:?} not found in {site:?}")));
                return;
            };
            let page_q = crate::runtime::article_latest(space, local);
            let shell_q = crate::runtime::shell(space);
            let page_sub = page_q.subscribe(&core).await;
            let shell_sub = shell_q.subscribe(&core).await;
            // `current()` yields `SharedView<…>` (a `!Send` refcount handle);
            // clone the payload out so owned values cross the channel.
            let _ = tx.send(Ok((
                (*page_sub.current()).clone(),
                (*shell_sub.current()).clone(),
            )));
        }
    };

    let db = dentrado::core::db::Db::start_with_worker(db_config(cores), worker)?;
    let rendered = rx
        .recv()
        .map_err(|_| anyhow::anyhow!("gear worker died before producing output"))?;
    let (page_view, shell) = rendered?;
    drop(db); // shut the cores down cleanly

    let base_css = if inject {
        read_base_css(&config.server.web_dist)
    } else {
        None
    };
    let html = render_page_document(&shell, &page_view, base_css.as_deref());
    print!("{html}");
    Ok(())
}

// =========================================================================
// Whole-site (mass) render
// =========================================================================

/// Everything a mass-render worker task needs, cloned once per core.
#[derive(Clone)]
struct Shared {
    /// The site's registered canonical space (gear identity).
    space: kolorinko_rt::SpaceId,
    /// The dataset site (slug-keyed enumeration identity).
    site: SafePathComponent,
    /// `--inject`: the base theme stylesheet to inline into every page.
    base_css: Option<String>,
    tx: mpsc::Sender<Out>,
}

/// A mass-render worker → coordinator message.
enum Out {
    /// One fully resolved, fully rendered page.
    Page { page: ListedPage, html: String },
    /// This core finished its owned shard: pages rendered plus the site title
    /// (off the shared shell) for the index.
    PagesDone {
        rendered: usize,
        site_title: Option<String>,
    },
    /// A worker failed or panicked; the coordinator aborts the run.
    Fatal(anyhow::Error),
}

/// Pages kept in flight per core during a mass render. Parallelism *across*
/// cores already rides deterministic gear routing; the window only overlaps
/// each page's await phases (blob materialisation on the single git worker
/// thread, include/link resolution through the `repo` oracle's core) with the
/// parse and render CPU of neighbouring pages on this core — CPU work stays
/// serial on the core's single-threaded runtime either way, and the window
/// bounds how much per-page state (pinned gear outputs, views, rendered
/// documents) is alive at once.
const IN_FLIGHT_PAGES: usize = 4;

/// `kolorinko render <site>` — render every page of one site into a directory
/// of standalone `.html` files. One worker per core renders exactly the pages
/// its `article_latest` gears own ([`Core::owns`] — deterministic routing) and
/// streams finished documents to the coordinator on the main thread, which
/// writes them out (render work parallelises across cores; disk IO stays on
/// one thread) and finally seals the directory with an `index.html`.
fn run_mass(
    config: Config,
    site_arg: &str,
    inject: bool,
    out: Option<PathBuf>,
    force: bool,
) -> anyhow::Result<()> {
    let site = SafePathComponent::new(site_arg.to_string())
        .ok_or_else(|| anyhow::anyhow!("invalid site name {site_arg:?}"))?;
    let site_str = (*site).clone();
    let out = out.unwrap_or_else(|| Path::new(".kolorinko").join("render").join(&site_str));
    prepare_out_dir(&out, force)?;

    crate::init_globals(&config)?;
    let space = space_of_site(&site)?;
    let base_css = if inject {
        read_base_css(&config.server.web_dist)
    } else {
        None
    };
    let cores = crate::core_count();

    let (tx, rx) = mpsc::channel::<Out>();
    let shared = Shared {
        space,
        site,
        base_css,
        tx,
    };
    let worker = move |core: Rc<Core<KolorinkoRT, InMemoryStorage<KolorinkoRT>>>| {
        let shared = shared.clone();
        async move {
            // A panicking render (bad page data) must not hang the CLI waiting
            // for a worker that will never report back.
            let result = std::panic::AssertUnwindSafe(render_owned(&core, &shared))
                .catch_unwind()
                .await;
            match result {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    let _ = shared.tx.send(Out::Fatal(e));
                }
                Err(panic) => {
                    let _ = shared
                        .tx
                        .send(Out::Fatal(anyhow::anyhow!("worker panicked: {panic:?}")));
                }
            }
        }
    };

    let started = Instant::now();
    let db = dentrado::core::db::Db::start_with_worker(db_config(cores), worker)?;
    let (rendered, listing, site_title) = coordinate(&rx, &out, cores)?;
    drop(db); // shut the cores down cleanly

    if rendered == 0 {
        anyhow::bail!("site {site_str:?} has no pages in the repository (unknown site?)");
    }
    write_index(&out, &site_str, site_title.as_deref(), &listing)?;
    eprintln!(
        "rendered {rendered} pages into {} in {:.1}s",
        out.display(),
        started.elapsed().as_secs_f32()
    );
    Ok(())
}

/// Consume worker output until every core has finished its shard, writing each
/// page under `out` as it arrives. Returns `(pages written, index listing,
/// site title)`.
fn coordinate(
    rx: &mpsc::Receiver<Out>,
    out: &Path,
    cores: std::num::NonZero<u32>,
) -> anyhow::Result<(usize, Vec<(String, String, String)>, Option<String>)> {
    let mut rendered = 0usize;
    let mut done_workers = 0u32;
    let mut site_title: Option<String> = None;
    // `(rel path, fullname, title)` per rendered page, in arrival order, for
    // the index.
    let mut listing: Vec<(String, String, String)> = Vec::new();
    loop {
        match rx.recv().map_err(|_| anyhow::anyhow!("gear worker died"))? {
            Out::Fatal(e) => return Err(e),
            Out::Page { page, html } => {
                let rel = page_rel_path(&page);
                let path = out.join(&rel);
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&path, html)?;
                rendered += 1;
                listing.push((
                    rel.to_string_lossy().into_owned(),
                    page.fullname(),
                    page.title,
                ));
                if rendered.is_multiple_of(100) {
                    eprintln!("  {rendered} pages…");
                }
            }
            Out::PagesDone {
                rendered: n,
                site_title: title,
            } => {
                done_workers += 1;
                site_title = site_title.or(title);
                info!("core finished {n} pages");
            }
        }
        if done_workers == cores.get() {
            // Arrival order interleaves cores (and, within a core, the
            // in-flight window), so sort the index back to `fullname` order.
            listing.sort_unstable_by(|a, b| a.1.cmp(&b.1));
            return Ok((rendered, listing, site_title));
        }
    }
}

/// One core's worker: the two site-level shared gears (the shell and the
/// wide-open page enumeration — one instance wherever routing puts them,
/// however many workers subscribe), then exactly the pages whose
/// `article_latest` gear this core owns ([`Core::owns`] — the same
/// deterministic routing every core computes, so the shards partition the
/// site with no coordination). Each owned page is resolved, SSR-rendered
/// through [`render_page_document`], and streamed to the coordinator,
/// [`IN_FLIGHT_PAGES`] at a time; a page's subscription is dropped after
/// reading, so per-page state never outlives the window over a long run.
async fn render_owned(
    core: &Rc<Core<KolorinkoRT, InMemoryStorage<KolorinkoRT>>>,
    shared: &Shared,
) -> anyhow::Result<()> {
    let shell_q = crate::runtime::shell(shared.space);
    let shell: SiteShell = (*shell_q.subscribe(core).await.current()).clone();

    // The wide-open selection enumerating every page of the site (hidden
    // `_`-prefixed system pages included) in one shared `repo_l_list_pages`
    // query; holding both subscriptions for the whole run keeps the shared
    // `repo` oracle active (one git clone total).
    let list_q = crate::runtime::repo_l_list_pages(shared.site.clone(), enumerate_query());
    let listed: ListPagesResult = (*list_q.subscribe(core).await.current()).clone();

    // Read the site title out up front: the render window borrows `shell`
    // until it is dropped at the end of the function.
    let site_title = shell.title.clone().filter(|t| !t.is_empty());

    // Exactly this core's owned pages, each as an independent
    // resolve-render-send future, driven with a bounded window: while one
    // page's subscription awaits its gear (blob materialisation off-core,
    // include/link resolution), other pages' parse and render CPU keeps this
    // core busy instead of idling. A future dropped mid-await cancels
    // cleanly (subscription `Drop` releases interest), so erroring out of
    // the drain loop tears the window down safely.
    let owned = listed.pages.into_iter().filter_map(|page| {
        // The listed page id is the canonical local id — no slug round-trip
        // needed.
        let local = kolorinko_rt::LocalId::from_page_id(&page.page_id)?;
        let page_q = crate::runtime::article_latest(shared.space, local);
        core.owns(page_q.id()).then_some((page, page_q))
    });
    let mut rendering = stream::iter(owned)
        .map(|(page, page_q)| {
            // Borrow the shell instead of moving it — the closure must stay
            // `FnMut` across the window's futures (`core`/`shared` are
            // already shared references, so they copy).
            let shell = &shell;
            async move {
                let sub = page_q.subscribe(core).await;
                let view: ArticleView = (*sub.current()).clone();
                drop(sub);
                let html = render_page_document(shell, &view, shared.base_css.as_deref());
                shared
                    .tx
                    .send(Out::Page { page, html })
                    .map_err(|_| anyhow::anyhow!("coordinator gone"))?;
                anyhow::Ok(())
            }
        })
        .buffer_unordered(IN_FLIGHT_PAGES);
    let mut rendered = 0usize;
    while let Some(result) = rendering.next().await {
        result?;
        rendered += 1;
    }
    info!("core{} rendered {rendered} pages", core.core_id());
    shared
        .tx
        .send(Out::PagesDone {
            rendered,
            site_title,
        })
        .map_err(|_| anyhow::anyhow!("coordinator gone"))?;
    Ok(())
}

/// The wide-open selection enumerating every page of a site in one shared
/// `repo_l_list_pages` query: every category, hidden `_`-prefixed pages
/// included, `fullname` ascending, pagination disabled.
fn enumerate_query() -> ListPagesQuery {
    ListPagesQuery(ListPagesParams {
        category: Some("*".to_string()),
        tags: None,
        created_by: None,
        created_at: None,
        updated_at: None,
        fullname: None,
        name: None,
        pagetype: Some("*".to_string()),
        order: Some(ListOrder {
            by: "fullname".to_string(),
            ascending: true,
        }),
        offset: None,
        limit: Some(i64::MAX),
        per_page: Some(i64::MAX),
        separate: true,
        wrapper: true,
    })
}

/// `<site>/<page>` or `<site>/<category>/<page>` → `(site, slug)`. Shared
/// route parsing ([`parse_route`]); each segment is validated as a single safe
/// path component (rejects `..`, absolute, …).
fn parse_page(arg: &str) -> anyhow::Result<(SafePathComponent, Slug)> {
    parse_route(arg).ok_or_else(|| {
        anyhow::anyhow!("expected <site>/<page> or <site>/<category>/<page>, got {arg:?}")
    })
}

/// The registered space for a site named on the CLI — the render gears
/// address pages canonically, so the site must appear in
/// `ensure-evakuilo-sites`.
fn space_of_site(site: &SafePathComponent) -> anyhow::Result<kolorinko_rt::SpaceId> {
    globals::space_of(site).ok_or_else(|| {
        anyhow::anyhow!(
            "site {site:?} is not registered; add it to `ensure-evakuilo-sites` in the config"
        )
    })
}

/// The page's output path relative to the render root, laid out like the
/// site's routes: `<category>/<name>.html`, or `<name>.html` for a root
/// (`_default`-category) page. A root page `foo` (`foo.html`) and a category
/// `foo` (`foo/…`) never collide on disk — one is a file, the other a
/// directory.
fn page_rel_path(page: &ListedPage) -> PathBuf {
    let file = format!("{}.html", page.name);
    match &page.category {
        Some(cat) => Path::new(cat).join(file),
        None => PathBuf::from(file),
    }
}

/// The mass render's output directory: absent, empty, or (under `--force`)
/// replaced wholesale. Refusing a non-empty target keeps stale output — pages
/// deleted from the wiki, or an older export layout — from surviving next to
/// fresh files.
fn prepare_out_dir(out: &Path, force: bool) -> anyhow::Result<()> {
    if out.exists() {
        if force {
            if out.is_dir() {
                std::fs::remove_dir_all(out)?;
            } else {
                anyhow::bail!("--out exists and is not a directory: {}", out.display());
            }
        } else if out
            .read_dir()
            .map(|mut d| d.next().is_some())
            .unwrap_or(false)
        {
            anyhow::bail!(
                "refusing to render into non-empty {} (pass --force to replace it, \
                 or --out a fresh directory)",
                out.display()
            );
        }
    }
    std::fs::create_dir_all(out)?;
    Ok(())
}

/// Write the mass render's `index.html`: the site title plus one link per
/// page (sorted by `fullname`, mirroring the enumeration order — arrival
/// order interleaves cores and the per-core in-flight window), so a consumer
/// — LLM agent or human — can navigate the whole export from a single entry
/// point.
fn write_index(
    out: &Path,
    site: &str,
    site_title: Option<&str>,
    pages: &[(String, String, String)],
) -> anyhow::Result<()> {
    let site_title = site_title.filter(|t| !t.is_empty()).unwrap_or(site);
    let items = pages
        .iter()
        .map(|(rel, fullname, title)| {
            let text = if title.is_empty() || title == fullname {
                fullname.clone()
            } else {
                format!("{title} — {fullname}")
            };
            format!(
                r#"<li><a href="{}">{}</a></li>"#,
                html_escape(&rel.replace('\\', "/")),
                html_escape(&text)
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let doc_title = html_escape(&format!("{site} — all pages"));
    let html = format!(
        "<!doctype html>\n\
         <html lang=\"en\">\n\
         <head>\n<meta charset=\"utf-8\">\n<title>{doc_title}</title>\n</head>\n\
         <body>\n<h1>{}</h1>\n<p>{} pages</p>\n<ul>\n{items}\n</ul>\n</body>\n</html>\n",
        html_escape(site_title),
        pages.len(),
    );
    std::fs::write(out.join("index.html"), html)?;
    Ok(())
}

/// Escape the five HTML-significant characters — for the hand-built index,
/// whose content (page titles, paths) comes straight from wiki data.
fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// Read the Wikidot base theme stylesheet from the built frontend dist
/// (`<web_dist>/wikidot-base-theme/css/style.css`), if present, so it can be
/// inlined into the rendered document.
fn read_base_css(web_dist: &str) -> Option<String> {
    let path = Path::new(web_dist)
        .join("wikidot-base-theme")
        .join("css")
        .join("style.css");
    std::fs::read_to_string(path).ok()
}

#[cfg(test)]
mod tests {
    use super::{ListedPage, page_rel_path, prepare_out_dir};
    use std::path::{Path, PathBuf};

    fn listed(category: Option<&str>, name: &str) -> ListedPage {
        ListedPage {
            name: name.to_string(),
            category: category.map(str::to_string),
            page_id: "1".to_string(),
            title: String::new(),
            tags: Vec::new(),
            created_by: String::new(),
            created_at: 0,
            updated_by: String::new(),
            updated_at: 0,
            revisions: 1,
        }
    }

    #[test]
    fn page_paths_mirror_routes() {
        assert_eq!(
            page_rel_path(&listed(None, "start")),
            PathBuf::from("start.html")
        );
        assert_eq!(
            page_rel_path(&listed(Some("rpc"), "rpc-205")),
            Path::new("rpc").join("rpc-205.html")
        );
    }

    #[test]
    fn out_dir_must_be_absent_or_empty() {
        let tmp = std::env::temp_dir().join(format!("kolorinko-render-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);

        // Absent → created.
        prepare_out_dir(&tmp, false).expect("creates absent dir");
        // Empty → kept.
        prepare_out_dir(&tmp, false).expect("accepts empty dir");

        // Non-empty without --force → refused, contents untouched.
        std::fs::write(tmp.join("stale.html"), "x").unwrap();
        assert!(prepare_out_dir(&tmp, false).is_err());
        assert!(tmp.join("stale.html").exists());

        // Non-empty with --force → replaced empty.
        prepare_out_dir(&tmp, true).expect("force replaces dir");
        assert!(!tmp.join("stale.html").exists());

        // A file where the dir should be → refused even under --force.
        std::fs::remove_dir_all(&tmp).unwrap();
        std::fs::write(&tmp, "x").unwrap();
        assert!(prepare_out_dir(&tmp, true).is_err());
        std::fs::remove_file(&tmp).unwrap();
    }
}
