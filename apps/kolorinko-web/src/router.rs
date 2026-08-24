//! Path-based client router: the single source of truth for which page the
//! app shows. There is exactly one URL family — the canonical
//! `/{space}/{local}[/title]` (the 'S'/'L' marker char in both ids makes recognition
//! purely syntactic; the optional title segment is decorative). The server
//! SSRs canonical routes and 301s slug-form paths to them; the client never
//! resolves slugs, so a slug link is simply a full browser navigation.
//!
//! `space`/`local` live as reactive signals so subscriptions re-subscribe on
//! navigation. Two global listeners keep them in sync with the URL:
//! - a `popstate` listener (back / forward), and
//! - a delegated `click` listener that turns left-clicks on internal
//!   `<a href="/SPACE/LOCAL…">` links into client-side navigation
//!   (pushState) instead of a full reload — so the WebTransport session and
//!   its subscriptions stay live across page changes.

use kolorinko_rt::{LocalId, SpaceId, SsrState, parse_page_route};
use leptos::prelude::*;
use wasm_bindgen::{JsCast, JsValue, closure::Closure};
use web_sys::{Element, Event, MouseEvent};

/// The CSR-boot fallback (Trunk dev, or a stale ServiceWorker shell on an
/// unparseable path): the dev config's space (`obscurative`) landing page
/// (`main`, page id 986050317). The URL is rewritten to it, so a reload is
/// stable. Production boots always carry the SSR state and never land here.
const FALLBACK_PATH: &str = "/S70P6lbBZxbc-kcpGOCYmZA/LAAAAADrF7w0";

fn window() -> web_sys::Window {
    web_sys::window().expect("no window")
}

#[derive(Clone)]
pub(crate) struct Router {
    /// The canonical route: the `article_latest` / `shell` subscription keys.
    pub space: RwSignal<Option<SpaceId>>,
    pub local: RwSignal<Option<LocalId>>,
}

impl Router {
    /// Read the route from the current URL — or, on a hydration boot, from
    /// the embedded SSR state (which carries exactly the canonical address
    /// the server rendered with). An unparseable path is rewritten
    /// (replaceState) to the demo default so a refresh is stable.
    pub(crate) fn bootstrap(initial: Option<&SsrState>) -> Self {
        let route = initial
            .map(|s| (s.space, s.local))
            .or_else(|| parse_page_route(&window().location().pathname().unwrap_or_default()))
            .unwrap_or_else(|| {
                if let Ok(h) = window().history() {
                    let _ = h.replace_state_with_url(&JsValue::NULL, "", Some(FALLBACK_PATH));
                }
                parse_page_route(FALLBACK_PATH).expect("fallback parses")
            });
        Self {
            space: RwSignal::new(Some(route.0)),
            local: RwSignal::new(Some(route.1)),
        }
    }

    /// Install the `popstate` and delegated `click` listeners (fire-and-forget:
    /// they outlive the reactive scope, for the page lifetime).
    pub(crate) fn install(&self) {
        self.clone().install_popstate();
        self.clone().install_clicks();
    }

    /// Client-side navigate to an internal path: pushState + update signals,
    /// so subscriptions re-subscribe without a full reload.
    pub(crate) fn navigate(&self, path: String) {
        let Some((space, local)) = parse_page_route(&path) else {
            return;
        };
        if let Ok(h) = window().history() {
            let _ = h.push_state_with_url(&JsValue::NULL, "", Some(&path));
        }
        self.set_space(Some(space));
        self.set_local(Some(local));
    }

    fn sync_from_location(&self) {
        if let Some((space, local)) =
            parse_page_route(&window().location().pathname().unwrap_or_default())
        {
            self.set_space(Some(space));
            self.set_local(Some(local));
        }
    }

    /// Set a signal only when it actually changes (`RwSignal::set` notifies
    /// unconditionally — a plain `set` would tear down and re-subscribe every
    /// signal-keyed subscription on same-page navigations).
    fn set_space(&self, v: Option<SpaceId>) {
        if self.space.with_untracked(|s| *s != v) {
            self.space.set(v);
        }
    }

    fn set_local(&self, v: Option<LocalId>) {
        if self.local.with_untracked(|s| *s != v) {
            self.local.set(v);
        }
    }

    fn install_popstate(self) {
        let cb = Closure::<dyn Fn()>::new(move || self.sync_from_location());
        let _ = window().add_event_listener_with_callback(
            "popstate",
            cb.as_ref().unchecked_ref::<js_sys::Function>(),
        );
        cb.forget();
    }

    /// Intercept plain left-clicks on internal `<a href="/…">` links and turn
    /// them into client-side navigation when the href parses as a canonical
    /// route. Slug-form links (`/SPACE/cat:slug`) and asset-like hrefs (a `.`
    /// in the last segment) fall through to the browser — the server 301s the
    /// former to the canonical form, so a full navigation is correct, just
    /// slower.
    fn install_clicks(self) {
        let cb = Closure::<dyn Fn(Event)>::new(move |ev: Event| {
            if let Some(me) = ev.dyn_ref::<MouseEvent>()
                && (me.button() != 0
                    || me.meta_key()
                    || me.ctrl_key()
                    || me.shift_key()
                    || me.alt_key())
            {
                return;
            }
            let Some(el) = ev.target().and_then(|t| t.dyn_into::<Element>().ok()) else {
                return;
            };
            let Ok(Some(anchor)) = el.closest("a") else {
                return;
            };
            let Some(href) = anchor.get_attribute("href") else {
                return;
            };
            let path = href.split(['?', '#']).next().unwrap_or(&href);
            if !path.starts_with('/') || path.starts_with("//") {
                return;
            }
            if path.starts_with("/-/") {
                return; // system namespace: a real fetch
            }
            if path.rsplit('/').next().unwrap_or("").contains('.') {
                return;
            }
            if parse_page_route(path).is_some() {
                ev.prevent_default();
                self.navigate(path.to_string());
            }
        });
        let _ = window()
            .document()
            .expect("no document")
            .add_event_listener_with_callback(
                "click",
                cb.as_ref().unchecked_ref::<js_sys::Function>(),
            );
        cb.forget();
    }
}
