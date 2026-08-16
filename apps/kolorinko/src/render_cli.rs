//! `kolorinko render [config] <site>/<page>` — one-shot SSR debug renderer.
//!
//! Spins up the gear runtime (no network servers), resolves one page plus its
//! site shell via `GearQuery::subscribe` (the same path the live server's SSR
//! response and [`crate::server::run_session`] use, so the whole
//! `repo → repo_l_article_latest → article_latest_parsed → article_latest`
//! cone runs — git clone, parse, `[[include]]` resolution — exactly as in
//! production), SSR-renders the result through [`kolorinko_render`] into a
//! self-contained HTML document, prints it, and exits.
//!
//! Both subscriptions stay live until every output is read, so the shared
//! `repo` oracle is computed once (one `git clone`) rather than re-cloned per
//! page.

use std::path::{Path, PathBuf};
use std::rc::Rc;

use dentrado::core::{core_ctx::Core, storage::InMemoryStorage};
use kolorinko_render::render_page_document;
use kolorinko_rt::{SafePathComponent, SiteShell, Slug, parse_route};
use kolorinko_wikitext::ArticleView;

use crate::runtime::KolorinkoRT;
use crate::{Config, db_config, make_repo_meta};

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

/// `<site>/<page>` or `<site>/<category>/<page>` → `(site, slug)`. Shared
/// route parsing ([`parse_route`]); each segment is validated as a single safe
/// path component (rejects `..`, absolute, …).
fn parse_page(arg: &str) -> anyhow::Result<(SafePathComponent, Slug)> {
    parse_route(arg).ok_or_else(|| {
        anyhow::anyhow!("expected <site>/<page> or <site>/<category>/<page>, got {arg:?}")
    })
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
