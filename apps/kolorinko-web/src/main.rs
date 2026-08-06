//! Leptos CSR client for kolorinko.
//!
//! The page itself is served over HTTP/3 by the kolorinko server (same
//! origin). On load it opens a WebTransport session and streams the page
//! content/edits. See [`wt`] for the wire protocol.

mod render;
mod wt;

use kolorinko_wikitext::ArticleView;
use leptos::prelude::*;

// TODO: CRITICAL: DEAL WITH LOCALIZABLE IN SERVER<->CLIENT INTERACTION!

#[component]
fn App() -> impl IntoView {
    let (article, set_article) = signal::<Option<ArticleView>>(None);
    let (status, set_status) = signal(String::from("connecting…"));

    Effect::new(move |_| {
        wasm_bindgen_futures::spawn_local(async move {
            if let Err(e) = wt::connect_wt(set_article, set_status).await {
                set_status.set(format!("wt error: {e:?}"));
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
