//! Leptos web client for kolorinko: CSR + hydration of the SSR document.
//!
//! When the kolorinko server served the page, the HTML arrives already
//! containing the rendered layout plus an embedded [`SsrState`] script; the
//! client then *hydrates* — rendering the same [`layout`] from the same data
//! (so the trees match positionally) and attaching reactivity — and only
//! afterwards goes live: a WebTransport session ([`wt::connect_wt`])
//! re-subscribes the page and site shell, pushing updates into the very
//! signals the layout renders from. When no embedded state exists (a plain
//! `index.html` boot, e.g. Trunk dev), the same app client-side renders from
//! `None` and fills in as subscriptions arrive.
//!
//! The layout itself lives in [`kolorinko_render::layout`], shared with the
//! server's SSR response; this module only wires reactivity around it (router,
//! subscriptions, `<head>` management).

mod menu;
mod router;
mod wt;

use kolorinko_render::{THEME_LINK_ID, document_title, layout};
use kolorinko_rt::wire;
use kolorinko_rt::{SSR_STATE_ID, SsrState};
use leptos::prelude::*;
use std::{cell::Cell, rc::Rc};
use wt::WtClient;

/// The whole app: one layout driven by signals. `initial` is the embedded SSR
/// state (hydrate boot); without it everything starts `None` (CSR boot).
fn app(initial: Option<SsrState>) -> AnyView {
    let router = router::Router::bootstrap();
    router.install();
    menu::install();

    let site = router.site;
    let slug = router.slug;
    let (client, set_client) = signal_local(Option::<Rc<WtClient>>::None);
    let (page, set_page) = signal(initial.as_ref().map(|s| s.page.clone()));
    let (shell, set_shell) = signal(initial.map(|s| s.shell));

    Effect::new(move |_| {
        wasm_bindgen_futures::spawn_local(async move {
            match wt::connect_wt().await {
                Ok(c) => set_client.set(Some(c)),
                Err(e) => leptos::logging::warn!("wt error: {e:?}"),
            }
        });
    });

    follow(
        client,
        move || wire::article_latest(site.get(), slug.get()),
        set_page,
    );
    follow(client, move || wire::shell(site.get()), set_shell);

    // Keep the browser tab title in sync with the current page's title.
    Effect::new(move |_| {
        let Some(doc) = (|| web_sys::window()?.document())() else {
            return;
        };
        if let Some(title) = page.get().map(|a| a.meta.title) {
            doc.set_title(&document_title(
                site.get().as_ref(),
                &shell.get().and_then(|x| x.title),
                &title,
            ));
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
        let href = shell.get().and_then(|s| s.theme_root);
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

    layout(
        move || (*site.get()).clone(),
        move || shell.get(),
        move || page.get(),
    )
}

/// One subscription to a gear, keyed on `make_query`, feeding `set`.
///
/// The effect re-runs whenever `client` or `make_query` reads a changed signal
/// (WT session arrival, route site/slug), cancelling the previous handle and
/// subscribing to the new one; each server push lands in `set`. A re-run for a
/// *changed* query clears `set` (stale content shouldn't linger while the new
/// page loads), but the first subscription keeps whatever is already there —
/// the SSR state it hydrated from, until the first push confirms or replaces
/// it.
fn follow<Out: Send + Sync + Clone + 'static>(
    client: ReadSignal<Option<Rc<WtClient>>, LocalStorage>,
    make_query: impl Fn() -> wire::GearQuery<Out> + 'static,
    set: WriteSignal<Option<Out>>,
) {
    let prev = Cell::new(None::<u64>);
    Effect::new(move |_| {
        let Some(c) = client.get() else {
            return;
        };
        let stale = prev.take();
        if let Some(p) = stale {
            c.cancel(p);
            set.set(None);
        }
        let sub = c.subscribe(make_query(), move |v: Out| set.set(Some(v)));
        prev.set(Some(sub));
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
    match ssr_state() {
        Some(state) => leptos::mount::hydrate_body(move || app(Some(state))),
        None => leptos::mount::mount_to_body(|| app(None)),
    }
}
