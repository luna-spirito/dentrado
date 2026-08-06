//! Leptos CSR client for kolorinko.
//!
//! The page is served over HTTP/3 by the kolorinko server (same origin). On
//! load it opens a WebTransport session ([`wt::connect_wt`]) and then drives
//! everything reactively off the route ([`router::Router`]):
//! - the current page (`article_latest`),
//! - the site's `nav:top` and `nav:side` navigation pages.
//!
//! Each is a typed subscription that re-subscribes when the route (its `site`
//! and, for the page, its `slug`) changes. Navigation is client-side
//! (pushState) so the session and subscriptions stay live across page changes.

mod render;
mod router;
mod wt;

use kolorinko_rt::wire;
use kolorinko_wikitext::ArticleView;
use leptos::prelude::*;
use std::{cell::Cell, rc::Rc};
use wt::WtClient;

#[component]
fn App() -> impl IntoView {
    // `Rc<WtClient>` is `!Send`; store it in a local (thread-local) signal.
    let (client, set_client) = signal_local(Option::<Rc<WtClient>>::None);
    let (status, set_status) = signal(String::from("connecting…"));

    Effect::new(move |_| {
        wasm_bindgen_futures::spawn_local(async move {
            match wt::connect_wt(set_status).await {
                Ok(c) => set_client.set(Some(c)),
                Err(e) => set_status.set(format!("wt error: {e:?}")),
            }
        });
    });

    view! {
        {move || match client.get() {
            Some(c) => layout(c),
            None => view! { <p class="kolorinko-status">{move || status.get()}</p> }.into_any(),
        }}
    }
}

/// One subscription to an `article_latest` gear, keyed on `make_query`.
///
/// The effect re-runs whenever `make_query` reads a changed signal (route
/// site/slug), cancelling the previous handle and subscribing to the new one;
/// the result lands in the returned signal on every server push.
fn subscribe(
    client: Rc<WtClient>,
    make_query: impl Fn() -> wire::GearQuery<ArticleView> + 'static,
) -> ReadSignal<Option<ArticleView>> {
    let (rx, wx) = signal(None);
    let prev = Cell::new(None::<u64>);
    Effect::new(move |_| {
        if let Some(p) = prev.take() {
            client.cancel(p);
        }
        wx.set(None);
        let sub = client.subscribe(make_query(), move |v: ArticleView| wx.set(Some(v)));
        prev.set(Some(sub));
    });
    rx
}

fn layout(client: Rc<WtClient>) -> AnyView {
    let router = router::Router::bootstrap();
    router.install();
    let site = router.site;
    let slug = router.slug;

    let page = subscribe(client.clone(), move || wire::article_latest(site.get(), slug.get()));
    let nav_side = subscribe(client.clone(), move || {
        wire::article_latest(site.get(), router::nav_slug("side"))
    });
    let nav_top =
        subscribe(client, move || wire::article_latest(site.get(), router::nav_slug("top")));

    view! {
        <div id="container-wrap-wrap">
        <div id="container-wrap">
            <div id="container">
                <div id="header">
                    <h1><a href="/">{move || site.get().as_ref().to_string_lossy().to_string()}</a></h1>
                    <div id="top-bar">{move || view_nav(site.get(), nav_top.get())}</div>
                </div>
                <div id="content-wrap">
                    <div id="side-bar">{move || view_nav(site.get(), nav_side.get())}</div>
                    <div id="main-content">
                        <div id="page-title">
                            {move || page.get().map(|a| a.meta.title).unwrap_or_default()}
                        </div>
                        <div id="page-content">{move || match page.get() {
                            Some(a) => {
                                let s = site.get();
                                let blocks = render::render_block(
                                    &s.as_ref().to_string_lossy(),
                                    &a.content,
                                );
                                view! { <>{blocks}</> }.into_any()
                            }
                            None => view! { <p class="kolorinko-status">"loading…"</p> }.into_any(),
                        }}</div>
                    </div>
                </div>
            </div>
        </div>
        </div>
    }
    .into_any()
}

/// Render a navigation page (`nav:top` / `nav:side`) — or nothing before it
/// arrives. The same renderer as page bodies; nav pages are just pages.
fn view_nav(site: kolorinko_rt::SafePathComponent, nav: Option<ArticleView>) -> AnyView {
    match nav {
        Some(a) => {
            let s = site.as_ref().to_string_lossy();
            let blocks = render::render_block(&s, &a.content);
            view! { <>{blocks}</> }.into_any()
        }
        None => {
            let _: () = view! {};
            ().into_any()
        },
    }
}

fn main() {
    console_error_panic_hook::set_once();
    leptos::mount::mount_to_body(App);
}
