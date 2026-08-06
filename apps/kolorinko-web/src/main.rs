//! Leptos CSR client for kolorinko.
//!
//! The page itself is served over HTTP/3 by the kolorinko server (same
//! origin). On load it opens a WebTransport session ([`wt::connect_wt`]),
//! subscribes to the default page via a typed [`wire::GearQuery`], and renders
//! the streamed [`ArticleView`]. See [`wt`] for the wire protocol.

mod render;
mod wt;

use kolorinko_rt::wire;
use kolorinko_rt::SafePathComponent;
use kolorinko_wikitext::ArticleView;
use leptos::prelude::*;

/// Default page requested on connect: the Obscurative syntax lecture.
const DEFAULT_SITE: &str = "obscurative";
const DEFAULT_PAGE: &str = "syntax";

#[component]
fn App() -> impl IntoView {
    let (article, set_article) = signal::<Option<ArticleView>>(None);
    let (status, set_status) = signal(String::from("connecting…"));

    Effect::new(move |_| {
        wasm_bindgen_futures::spawn_local(async move {
            match wt::connect_wt(set_status).await {
                Ok(c) => {
                    let query = wire::article_latest(
                        SafePathComponent::new(DEFAULT_SITE.into()).unwrap(),
                        (None, SafePathComponent::new(DEFAULT_PAGE.into()).unwrap()),
                    );
                    c.subscribe(query, move |a: ArticleView| {
                        set_article.set(Some(a));
                        set_status.set(String::new());
                    });
                    // Keep the client alive for the page lifetime so future
                    // navigation can `subscribe`/`cancel` more pages.
                    std::mem::forget(c);
                }
                Err(e) => set_status.set(format!("wt error: {e:?}")),
            }
        });
    });

    view! {
        <div id="container">
            <div id="content-wrap">
                <div id="main-content">
                    <div id="page-title">
                        {move || article.get().map(|a| a.meta.title).unwrap_or_default()}
                    </div>
                    <div id="page-content">
                        {move || match article.get() {
                            None => view! { <p>{move || status.get()}</p> }.into_any(),
                            Some(a) => {
                                let blocks = render::render_block(&a.content);
                                view! { <>{blocks}</> }.into_any()
                            }
                        }}
                    </div>
                </div>
            </div>
        </div>
    }
}

fn main() {
    console_error_panic_hook::set_once();
    leptos::mount::mount_to_body(App);
}
