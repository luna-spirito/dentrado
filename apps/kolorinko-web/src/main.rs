//! Leptos web client for kolorinko: CSR + hydration of the SSR document.
//!
//! When the kolorinko server served the page, the HTML arrives already
//! containing the rendered layout plus an embedded [`SsrState`] script; the
//! client then *hydrates* — rendering the same [`layout`] from the same data
//! (so the trees match positionally) and attaching reactivity — and only
//! afterwards goes live: a transport session ([`transport::connect`] —
//! WebTransport, falling back to fetch) re-subscribes the page and site
//! shell, feeding updates into the very signals the layout renders from.
//! When no embedded state exists (a plain `index.html` boot, e.g. Trunk
//! dev), the same app client-side renders from
//! `None` and fills in as subscriptions arrive.
//!
//! The layout itself lives in [`kolorinko_render::layout`], shared with the
//! server's SSR response; this module only wires reactivity around it (router,
//! subscriptions, `<head>` management).

mod menu;
mod router;
mod transport;

use kolorinko_render::{
    ABOUT_PATH, THEME_LINK_ID, Scope, about_page, document_title, layout,
};
use kolorinko_rt::wire;
use kolorinko_rt::{SSR_STATE_ID, SiteShell, SsrState};
use kolorinko_wikitext::ArticleView;
use leptos::prelude::*;
use std::{cell::Cell, rc::Rc};
use transport::Transport;

/// The whole app: one layout driven by signals. `initial` is the embedded SSR
/// state (hydrate boot); without it everything starts `None` (CSR boot).
fn app(initial: Option<SsrState>) -> AnyView {
    let router = router::Router::bootstrap(initial.as_ref());
    router.install();
    menu::install();

    let space = router.space;
    let local = router.local;
    let client = Rc::new(transport::connect());
    let page_hash = initial.as_ref().map(|s| s.page_hash.clone());
    let shell_hash = initial.as_ref().map(|s| s.shell_hash.clone());
    let (page, set_page) = signal(initial.as_ref().map(|s| s.page.clone()));
    let (shell, set_shell) = signal(initial.map(|s| s.shell));

    // The page and its shell are addressed by the canonical route itself —
    // the same identity the URL names — so no resolution round-trip exists:
    // the queries key straight on `space`/`local`.
    follow_opt(
        &client,
        page_hash,
        move || {
            space
                .get()
                .zip(local.get())
                .map(|(s, l)| wire::article_latest(s, l))
        },
        move |v: ArticleView| set_page.set(Some(v)),
        move || set_page.set(None),
    );

    follow_opt(
        &client,
        shell_hash,
        move || space.get().map(wire::shell),
        move |v: SiteShell| set_shell.set(Some(v)),
        move || set_shell.set(None),
    );

    // Field-level getters: each reactive node clones only the field it
    // renders, never the whole shell/page (see `layout`'s signature). The
    // display name falls back to the space id spelling until the shell
    // arrives with the site title.
    let space_str = move || space.get().map(|s| s.to_string()).unwrap_or_default();
    let site_name = move || {
        shell
            .with(|s| s.as_ref().and_then(|x| x.title.clone()))
            .unwrap_or_else(space_str)
    };
    let shell_title = move || shell.with(|s| s.as_ref().and_then(|x| x.title.clone()));
    let shell_subtitle = move || shell.with(|s| s.as_ref().and_then(|x| x.subtitle.clone()));
    let shell_site = move || shell.with(|s| s.as_ref().and_then(|x| x.site.clone()));
    let shell_root = move || shell.with(|s| s.as_ref().and_then(|x| x.root.clone()));
    let nav_top = move || shell.with(|s| s.as_ref().map(|x| x.nav_top.clone()));
    let nav_side = move || shell.with(|s| s.as_ref().map(|x| x.nav_side.clone()));
    let page_title = move || page.with(|p| p.as_ref().map(|a| a.meta.title.clone()));
    let page_body = move || page.get();

    // Keep the browser tab title in sync with the current page's title
    // (and with the about screen, which has a title of its own).
    let about = router.about;
    Effect::new(move |_| {
        let Some(doc) = (|| web_sys::window()?.document())() else {
            return;
        };
        if about.get() {
            doc.set_title("Dentrado");
            return;
        }
        if let Some(title) = page_title() {
            doc.set_title(&document_title(&site_name(), &shell_title(), &title));
        }
    });

    // Show the pretty titled form (`/SPACE/LOCAL/TITLE` — collapsed to
    // `/LOCAL/TITLE` when the page names this origin's default space) in the
    // address bar once the page's title is known — re-derived here from the
    // article itself, so renames propagate on the next render. Full-weight
    // addresses arrive from the server's 301s and SSR links; this is where
    // they simplify.
    let titled_router = router.clone();
    Effect::new(move |_| {
        let Some((space, local)) = space.get().zip(local.get()) else {
            return;
        };
        let Some(title) = page_title().filter(|t| !t.is_empty()) else {
            return;
        };
        let titled = titled_router.address(space, local, &title);
        let path = window_path();
        if titled_router.parse(&path) == Some((space, local)) && path != titled {
            if let Ok(h) = web_sys::window().expect("no window").history() {
                let _ = h.replace_state_with_url(&wasm_bindgen::JsValue::NULL, "", Some(&titled));
            }
        }
    });

    // Keep one theme `<link>` in `<head>` for the current site. Re-runs on site
    // change and on repo updates; the stylesheet is served (and its `@import`/
    // `url()` references rewritten) by the kolorinko origin. Reuses the
    // SSR-injected element instead of recreating it.
    Effect::new(move |_| {
        let Some(doc) = (|| web_sys::window()?.document())() else {
            return;
        };
        let href = shell.with(|s| s.as_ref().and_then(|x| x.theme_root.clone()));
        let link = match doc.get_element_by_id(THEME_LINK_ID) {
            Some(el) => el,
            None => match href {
                // No theme and no element: nothing to manage yet.
                None => return,
                Some(_) => {
                    let el = doc.create_element("link").expect("create link");
                    let _ = el.set_attribute("id", THEME_LINK_ID);
                    let _ = el.set_attribute("rel", "stylesheet");
                    let _ = doc.head().map(|h| h.append_child(&el));
                    el
                }
            },
        };
        match href {
            Some(href) if link.get_attribute("href").as_deref() != Some(href.as_str()) => {
                let _ = link.set_attribute("href", &href);
            }
            None => link.remove(),
            _ => {}
        }
    });

    // The two screens of the app: the platform's about page — switched to
    // without a reload ([`ABOUT_PATH`], subscriptions off, its own chrome) —
    // and the wiki layout. Both render from the same signals, so toggling
    // between them re-runs nothing but the view. The getters are cloned
    // *inside* the closure body: `layout` takes them by value, so a move out
    // of the capture would make this a one-shot `FnOnce` instead of the
    // re-runnable `Fn` a toggleable branch needs.
    //
    // The scope carries the origin's default space (`None` on the main
    // origin), so every href the client builds is short at construction;
    // SSR links arrive full and are simplified once at boot
    // ([`router::Router::install`]).
    let scope_default = router.default;
    let wiki = move || {
        layout(
            move || Scope { space: space.get(), default: scope_default },
            site_name.clone(),
            shell_title.clone(),
            shell_subtitle.clone(),
            shell_site.clone(),
            shell_root.clone(),
            nav_top.clone(),
            nav_side.clone(),
            page_title.clone(),
            page_body.clone(),
        )
    };
    view! {
        <>{move || if about.get() { about_page() } else { wiki() }}</>
    }
    .into_any()
}

/// The current pathname (best-effort; `""` outside a window).
fn window_path() -> String {
    web_sys::window()
        .and_then(|w| w.location().pathname().ok())
        .unwrap_or_default()
}

/// One subscription to a gear, keyed on `make_query`, feeding `set`.
///
/// The query is `Option`-valued: `None` (unknown address — a canonical route
/// still resolving, or the about screen) holds no subscription at all. The
/// effect re-runs whenever `make_query` reads a changed signal, cancelling
/// the previous handle and subscribing to the new one; each server push
/// lands in `set`, and `clear` runs when a *previously live* query goes
/// away (stale content shouldn't linger while the new page loads). The
/// hydration hash is claimed exactly once — by the boot subscription it
/// describes; every later subscription starts from scratch, because its
/// content was cleared on the way out (`clear`) or never arrived. Reconnects
/// don't re-run this: the client replays its registry (hash included) on
/// each fresh session.
fn follow_opt<Out: 'static>(
    client: &Rc<Transport>,
    initial_hash: Option<String>,
    make_query: impl Fn() -> Option<wire::GearQuery<Out>> + 'static,
    set: impl Fn(Out) + 'static,
    clear: impl Fn() + 'static,
) {
    let client = client.clone();
    let set = Rc::new(set);
    let clear = Rc::new(clear);
    let prev = Cell::new(None::<u64>);
    let boot = Cell::new(true);
    Effect::new(move |_| {
        let stale = prev.take();
        if let Some(p) = stale {
            client.cancel(p);
            clear();
        }
        if let Some(q) = make_query() {
            let known = boot.replace(false).then(|| initial_hash.clone()).flatten();
            let set = set.clone();
            let sub = client.subscribe(q, known.as_deref(), move |v: Out| set(v));
            prev.set(Some(sub));
        }
    });
}

/// Read the [`SsrState`] embedded by the server, if this page was SSR'd.
fn ssr_state() -> Option<SsrState> {
    let json = web_sys::window()?
        .document()?
        .get_element_by_id(SSR_STATE_ID)?
        .text_content()?;
    SsrState::from_embedded_json(&json)
}

fn main() {
    console_error_panic_hook::set_once();
    register_service_worker();
    match ssr_state() {
        Some(state) => leptos::mount::hydrate_body(move || app(Some(state))),
        None => {
            // A CSR boot on the about path may find the body already carrying
            // content: the SSR'd screen (a direct hit — bots and no-JS read
            // exactly that markup) or the bare app placeholder (a shell the
            // ServiceWorker served). `mount_to_body` appends, so the body is
            // cleared first — the client re-renders the identical static view
            // and owns routing from there.
            if window_path().trim_end_matches('/') == ABOUT_PATH
                && let Some(body) =
                    web_sys::window().and_then(|w| w.document()).and_then(|d| d.body())
            {
                while let Some(child) = body.first_child() {
                    let _ = body.remove_child(&child);
                }
            }
            leptos::mount::mount_to_body(|| app(None));
        }
    }
}

/// Register the app-shell ServiceWorker (release builds only). It intercepts
/// navigations and serves the cached CSR shell stale-while-revalidate, so real
/// browsers bypass SSR; bots / first load / no-JS fall through to SSR. Dev
/// builds skip registration so `trunk` edits aren't shadowed by a cached shell.
/// `/sw.js` is a permanent URL: the browser byte-checks it to update installed
/// workers, so moving it bricks every deployed client — contents change, the
/// address never does.
#[cfg(not(debug_assertions))]
fn register_service_worker() {
    let Some(window) = web_sys::window() else {
        return;
    };
    let nav = window.navigator();
    let sw = js_sys::Reflect::get(&nav, &"serviceWorker".into())
        .unwrap_or(wasm_bindgen::JsValue::UNDEFINED);
    if sw.is_undefined() {
        return;
    }
    let container: web_sys::ServiceWorkerContainer = sw.into();
    let promise = container.register("/sw.js");
    wasm_bindgen_futures::spawn_local(async move {
        if let Err(e) = wasm_bindgen_futures::JsFuture::from(promise).await {
            leptos::logging::warn!("sw register: {e:?}");
        }
    });
}

#[cfg(debug_assertions)]
fn register_service_worker() {}
