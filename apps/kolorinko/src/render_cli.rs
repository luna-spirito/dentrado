//! `kolorinko render [config] <site>/<page>` — one-shot SSR debug renderer.
//!
//! Spins up the gear runtime (no network servers), resolves one page plus its
//! site's `nav:top` / `nav:side` pages via `GearQuery::subscribe` (the same path
//! the live server's [`crate::server::run_session`] uses, so the whole
//! `repo → repo_l_article_latest → article_latest_parsed → article_latest`
//! cone runs — git clone, parse, `[[include]]` resolution — exactly as in
//! production), SSR-renders the result through [`kolorinko_render`] into a
//! self-contained HTML document, prints it, and exits.
//!
//! All three `article_latest` subscriptions stay live until every output is
//! read, so the shared `repo` oracle is computed once (one `git clone`) rather
//! than re-cloned per page.

use std::path::{Path, PathBuf};
use std::rc::Rc;

use dentrado::core::{core_ctx::Core, storage::InMemoryStorage};
use kolorinko_render::render_page_document;
use kolorinko_rt::SafePathComponent;
use kolorinko_wikitext::ArticleView;

use crate::runtime::KolorinkoRT;
use crate::{Config, db_config, make_repo_meta};

type Slug = (Option<SafePathComponent>, SafePathComponent);

/// Entry point for `kolorinko render …` (the leading `render` already consumed).
pub(crate) fn run_cli(config_path: PathBuf, page: String, inject: bool) -> anyhow::Result<()> {
    let config: Config = toml::from_str(&std::fs::read_to_string(&config_path)?)
        .map_err(|e| anyhow::anyhow!("failed to parse {}: {e}", config_path.display()))?;
    run(config, &page, inject)
}

fn run(config: Config, page: &str, inject: bool) -> anyhow::Result<()> {
    let (site, slug) = parse_page(page)?;
    // Compute the site string before `site` is moved into the worker closure.
    let site_str = site.as_ref().to_string_lossy().to_string();
    let repo_meta = make_repo_meta(&config.repo);
    let nav_top_slug = nav_slug("top")?;
    let nav_side_slug = nav_slug("side")?;

    // One core is enough for a single render; it keeps the `repo` oracle,
    // every lens, and the parse gears co-located, so the follow/secondary_get
    // cone never crosses a thread.
    let cores = std::num::NonZero::new(1).unwrap();

    let (tx, rx) =
        std::sync::mpsc::channel::<anyhow::Result<(ArticleView, ArticleView, ArticleView)>>();
    let worker = move |core: Rc<Core<KolorinkoRT, InMemoryStorage<KolorinkoRT>>>| {
        let page_q = crate::runtime::article_latest(repo_meta.clone(), site.clone(), slug.clone());
        let nav_top_q =
            crate::runtime::article_latest(repo_meta.clone(), site.clone(), nav_top_slug.clone());
        let nav_side_q =
            crate::runtime::article_latest(repo_meta.clone(), site.clone(), nav_side_slug.clone());
        let tx = tx.clone();
        async move {
            // Subscribe before reading any: holding all three keeps the shared
            // `repo` oracle active across the three queries (one clone total).
            let page_sub = page_q.subscribe(&core).await;
            let nav_top_sub = nav_top_q.subscribe(&core).await;
            let nav_side_sub = nav_side_q.subscribe(&core).await;
            let res = (|| -> anyhow::Result<(ArticleView, ArticleView, ArticleView)> {
                let ship = |out: dentrado::core::gear::GearResult<KolorinkoRT>| {
                    out.into_ship().ok_or_else(|| {
                        anyhow::anyhow!("article_latest produced a local (non-shippable) output")
                    })
                };
                let page = (page_q.getter)(ship(page_sub.current())?);
                let nav_top = (nav_top_q.getter)(ship(nav_top_sub.current())?);
                let nav_side = (nav_side_q.getter)(ship(nav_side_sub.current())?);
                Ok((page, nav_top, nav_side))
            })();
            let _ = tx.send(res);
        }
    };

    let db = dentrado::core::db::Db::start_with_worker(db_config(cores), worker)?;
    let rendered = rx
        .recv()
        .map_err(|_| anyhow::anyhow!("gear worker died before producing output"))?;
    let (page_view, nav_top, nav_side) = rendered?;
    drop(db); // shut the cores down cleanly

    let base_css = if inject {
        read_base_css(&config.server.web_dist)
    } else {
        None
    };
    let html = render_page_document(
        &site_str,
        &page_view,
        Some(&nav_top),
        Some(&nav_side),
        base_css.as_deref(),
    );
    print!("{html}");
    Ok(())
}

/// `<site>/<page>` or `<site>/<category>/<page>` → `(site, slug)`. Each segment
/// is validated as a single safe path component (rejects `..`, absolute, …).
fn parse_page(arg: &str) -> anyhow::Result<(SafePathComponent, Slug)> {
    let segs: Vec<&str> = arg.split('/').filter(|s| !s.is_empty()).collect();
    let mk = |s: &str| {
        SafePathComponent::new(s.to_string())
            .ok_or_else(|| anyhow::anyhow!("invalid path segment: {s:?}"))
    };
    match segs.as_slice() {
        [s, p] => Ok((mk(s)?, (None, mk(p)?))),
        [s, c, p] => Ok((mk(s)?, (Some(mk(c)?), mk(p)?))),
        _ => anyhow::bail!("expected <site>/<page> or <site>/<category>/<page>, got {arg:?}"),
    }
}

/// Slug for one of the per-site navigation pages (`nav:top`, `nav:side`).
fn nav_slug(name: &str) -> anyhow::Result<Slug> {
    let category = SafePathComponent::new("nav".into())
        .ok_or_else(|| anyhow::anyhow!("invalid nav category"))?;
    let page = SafePathComponent::new(name.into())
        .ok_or_else(|| anyhow::anyhow!("invalid nav page: {name:?}"))?;
    Ok((Some(category), page))
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
