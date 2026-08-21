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
//! single shared instances wherever routing puts them.

use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc;
use std::time::Instant;

use dentrado::core::{core_ctx::Core, storage::InMemoryStorage};
use futures::FutureExt;
use kolorinko_render::render_page_document;
use kolorinko_rt::{
    ListPagesQuery, ListPagesResult, ListedPage, SafePathComponent, SiteShell, Slug, parse_route,
};
use kolorinko_wikitext::{ArticleView, ListOrder, ListPagesParams};
use log::info;

use crate::runtime::KolorinkoRT;
use crate::wikidot_page::RepoMeta;
use crate::{Config, db_config, make_repo_meta};

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
    // Compute the site string before `site` is moved into the worker closure.
    let site_str = (*site).clone();
    let repo_meta = make_repo_meta(&config.repo);

    // One core is enough for a single render; it keeps the `repo` oracle,
    // every lens, and the parse gears co-located, so the follow/secondary_get
    // cone never crosses a thread.
    let cores = std::num::NonZero::new(1).unwrap();

    let (tx, rx) = std::sync::mpsc::channel::<anyhow::Result<(ArticleView, SiteShell)>>();
    let worker = move |core: Rc<Core<KolorinkoRT, InMemoryStorage<KolorinkoRT>>>| {
        let page_q = crate::runtime::article_latest(repo_meta.clone(), site.clone(), slug.clone());
        let shell_q = crate::runtime::shell(repo_meta.clone(), site.clone());
        let tx = tx.clone();
        async move {
            // Subscribe before reading either: holding both keeps the shared
            // `repo` oracle active across the two queries (one clone total).
            let page_sub = page_q.subscribe(&core).await;
            let shell_sub = shell_q.subscribe(&core).await;
            let page = (page_q.getter)(page_sub.current());
            let shell = (shell_q.getter)(shell_sub.current());
            // The getters return `SharedView<…>` (a `!Send` refcount handle);
            // clone the payload out so owned values cross the channel.
            let _ = tx.send(Ok(((*page).clone(), (*shell).clone())));
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
    let html = render_page_document(&site_str, &shell, &page_view, base_css.as_deref());
    print!("{html}");
    Ok(())
}

// =========================================================================
// Whole-site (mass) render
// =========================================================================

/// Everything a mass-render worker task needs, cloned once per core.
#[derive(Clone)]
struct Shared {
    repo_meta: RepoMeta,
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

    let repo_meta = make_repo_meta(&config.repo);
    let base_css = if inject {
        read_base_css(&config.server.web_dist)
    } else {
        None
    };
    let cores = crate::core_count();

    let (tx, rx) = mpsc::channel::<Out>();
    let shared = Shared {
        repo_meta,
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
/// through [`render_page_document`], and streamed to the coordinator; its
/// subscription is dropped after reading so per-page state doesn't pile up
/// over a long run.
async fn render_owned(
    core: &Rc<Core<KolorinkoRT, InMemoryStorage<KolorinkoRT>>>,
    shared: &Shared,
) -> anyhow::Result<()> {
    let shell_q = crate::runtime::shell(shared.repo_meta.clone(), shared.site.clone());
    let shell_sub = shell_q.subscribe(core).await;
    let shell: SiteShell = (*(shell_q.getter)(shell_sub.current())).clone();

    // The wide-open selection enumerating every page of the site (hidden
    // `_`-prefixed system pages included) in one shared `repo_l_list_pages`
    // query; holding both subscriptions for the whole run keeps the shared
    // `repo` oracle active (one git clone total).
    let list_q = crate::runtime::repo_l_list_pages(
        shared.repo_meta.clone(),
        shared.site.clone(),
        enumerate_query(),
    );
    let list_sub = list_q.subscribe(core).await;
    let listed: ListPagesResult = (*(list_q.getter)(list_sub.current())).clone();

    let mut rendered = 0usize;
    for page in listed.pages {
        let Some(slug) = page_slug(&page) else {
            continue;
        };
        let page_q =
            crate::runtime::article_latest(shared.repo_meta.clone(), shared.site.clone(), slug);
        if !core.owns(&page_q.id) {
            continue; // another core owns this page
        }
        let sub = page_q.subscribe(core).await;
        let view: ArticleView = (*(page_q.getter)(sub.current())).clone();
        drop(sub);
        let html = render_page_document(&shared.site, &shell, &view, shared.base_css.as_deref());
        shared
            .tx
            .send(Out::Page { page, html })
            .map_err(|_| anyhow::anyhow!("coordinator gone"))?;
        rendered += 1;
    }
    info!("core{} rendered {rendered} pages", core.core_id());
    shared
        .tx
        .send(Out::PagesDone {
            rendered,
            site_title: shell.title.filter(|t| !t.is_empty()),
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

/// A listed page back into a `(category, name)` slug — the inverse of the
/// dataset projection (`slug_of` in `listpages.rs`): `None` only for
/// malformed names that cannot be safe path components (shouldn't happen for
/// pages that came out of the repo).
fn page_slug(page: &ListedPage) -> Option<Slug> {
    Some((
        page.category
            .as_ref()
            .and_then(|c| SafePathComponent::new(c.clone())),
        SafePathComponent::new(page.name.clone())?,
    ))
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
/// page (in the same `fullname`-ascending order the pages were rendered in),
/// so a consumer — LLM agent or human — can navigate the whole export from a
/// single entry point.
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
    use super::{ListedPage, page_rel_path, page_slug, prepare_out_dir};
    use std::path::{Path, PathBuf};

    fn listed(category: Option<&str>, name: &str) -> ListedPage {
        ListedPage {
            name: name.to_string(),
            category: category.map(str::to_string),
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
    fn page_slug_round_trips() {
        assert_eq!(
            page_slug(&listed(Some("rpc"), "rpc-205"))
                .map(|(c, n)| { (c.map(|c| (*c).clone()), (*n).clone()) }),
            Some((Some("rpc".to_string()), "rpc-205".to_string()))
        );
        assert_eq!(
            page_slug(&listed(None, "start")).map(|(c, n)| (c, (*n).clone())),
            Some((None, "start".to_string()))
        );
        // Unsafe names can't become slugs (defensive: repo data is validated
        // at the same boundary on the way in).
        assert!(page_slug(&listed(None, "../etc")).is_none());
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
