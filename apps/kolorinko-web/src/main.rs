//! Leptos CSR client for kolorinko.
//!
//! The page itself is served over HTTP/3 by the kolorinko server (same
//! origin). On load it opens a WebTransport session and streams the page
//! content/edits. See [`wt`] for the wire protocol.

mod render;
mod wt;

use kolorinko_wikitext::Content;
use leptos::prelude::*;

#[component]
fn App() -> impl IntoView {
    let (page, set_page) = signal::<Option<Content>>(None);
    let (title, set_title) = signal(String::from("kolorinko"));
    let (status, set_status) = signal(String::from("connecting…"));

    Effect::new(move |_| {
        wasm_bindgen_futures::spawn_local(async move {
            if let Err(e) = wt::connect_wt(set_page, set_title, set_status).await {
                set_status.set(format!("wt error: {e:?}"));
            }
        });
    });

    view! {
        <div id="container">
            <div id="content-wrap">
                <div id="main-content">
                    <div id="page-title">{move || title.get()}</div>
                    <div id="page-content">
                        {move || match page.get() {
                            None => view! { <p>{move || status.get()}</p> }.into_any(),
                            Some(content) => {
                                let blocks = render::render_block(&content);
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
