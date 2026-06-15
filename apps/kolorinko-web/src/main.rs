//! Leptos CSR client for kolorinko.
//!
//! Connects to `/ws` on the page origin, requests a Wikidot page, and renders
//! its parsed AST into the `#page-content` shell. The wire protocol is the
//! JSON defined in the server's [`web::Reply`]/[`web::Request`].
//!
//! [`web::Reply`]: ../../../kolorinko/src/web.rs

mod render;

use kolorinko_wikitext::Content;
use leptos::prelude::*;
use serde::{Deserialize, Serialize};
use wasm_bindgen::{closure::Closure, JsCast};
use web_sys::{MessageEvent, WebSocket};

#[derive(Serialize)]
#[serde(tag = "t")]
enum Request {
    #[serde(rename = "load")]
    Load {
        site: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        category: Option<String>,
        page: String,
    },
}

#[derive(Deserialize)]
#[serde(tag = "t")]
enum Reply {
    #[serde(rename = "page")]
    Page { content: Content },
    #[serde(rename = "repo")]
    Repo { path: String },
    #[serde(rename = "error")]
    Error { error: String },
}

/// Default page to load on first connect: the Obscurative syntax lecture.
const DEFAULT_SITE: &str = "obscurative";
const DEFAULT_PAGE: &str = "syntax";

#[component]
fn App() -> impl IntoView {
    let (page, set_page) = signal::<Option<Content>>(None);
    let (title, set_title) = signal(String::from("kolorinko"));
    let (status, set_status) = signal(String::from("connecting…"));

    Effect::new(move |_| {
        let ws = connect(set_page, set_title, set_status.clone());
        if let Some(ws) = ws {
            // On open, request the default page.
            let onopen = Closure::<dyn FnMut(web_sys::Event)>::new({
                let ws = ws.clone();
                let set_status = set_status;
                move |_ev: web_sys::Event| {
                    let req = serde_json::to_string(&Request::Load {
                        site: DEFAULT_SITE.into(),
                        category: None,
                        page: DEFAULT_PAGE.into(),
                    })
                    .expect("serialize load");
                    let _ = ws.send_with_str(&req);
                    set_status.set("loading…".into());
                }
            });
            ws.set_onopen(Some(onopen.as_ref().unchecked_ref()));
            onopen.forget();
        }
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

/// Open `/ws` and route replies into the provided signals.
fn connect(
    set_page: WriteSignal<Option<Content>>,
    set_title: WriteSignal<String>,
    set_status: WriteSignal<String>,
) -> Option<WebSocket> {
    let Some(window) = web_sys::window() else {
        set_status.set("no window".into());
        return None;
    };
    let location = window.location();
    let proto = if location.protocol().unwrap_or_default() == "https:" {
        "wss:"
    } else {
        "ws:"
    };
    let host = location.host().unwrap_or_default();
    let url = format!("{proto}//{host}/ws");

    let ws = match WebSocket::new(&url) {
        Ok(ws) => ws,
        Err(e) => {
            set_status.set(format!("WebSocket error: {e:?}"));
            return None;
        }
    };

    let onmessage = Closure::<dyn FnMut(MessageEvent)>::new(move |ev: MessageEvent| {
        let data = match ev.data().as_string() {
            Some(s) => s,
            None => {
                set_status.set("received non-text frame".into());
                return;
            }
        };
        match serde_json::from_str::<Reply>(&data) {
            Ok(Reply::Page { content }) => {
                set_page.set(Some(content));
                set_status.set(String::new());
            }
            Ok(Reply::Repo { path }) => {
                set_status.set(format!("repo: {path}"));
            }
            Ok(Reply::Error { error }) => {
                set_status.set(format!("error: {error}"));
            }
            Err(e) => {
                set_status.set(format!("decode error: {e}"));
            }
        }
    });
    ws.set_onmessage(Some(onmessage.as_ref().unchecked_ref()));
    onmessage.forget();

    let onerror = Closure::<dyn FnMut(web_sys::Event)>::new(move |_ev: web_sys::Event| {
        set_status.set("WebSocket error".into());
    });
    ws.set_onerror(Some(onerror.as_ref().unchecked_ref()));
    onerror.forget();

    // Keep the socket alive for the page lifetime; the closures own clones.
    let leaked = ws.clone();
    std::mem::forget(leaked);
    set_title.set("Лекция Синтаксис".into());
    Some(ws)
}

fn main() {
    console_error_panic_hook::set_once();
    leptos::mount::mount_to_body(App);
}
