//! Path-based client router: the single source of truth for which page the
//! app shows. There is exactly one URL family — the canonical
//! `/{space}/{local}[/title]` (the 'S'/'L' marker char in both ids makes recognition
//! purely syntactic; the optional title segment is decorative) — plus its
//! space-less sibling `/{local}[/title]` on a wiki's own domain, where the
//! `Host` already names the space: the server injects that space as
//! `window.__DEFAULT_SPACE_ID__` ([`DEFAULT_SPACE_GLOBAL`]) into every HTML
//! document it serves there, and this router reads it and addresses `/L…`
//! paths to it.
//!
//! The division of labor is absolute: the server always outputs
//! full-weight links (every href and redirect carries its space id); the
//! client always simplifies them — [`simplify`] against the origin's
//! default space, applied at href construction (the render `Scope` carries
//! the default), to the address bar ([`address`]), to pushed navigations
//! ([`navigate`]), and once over the hydrated SSR markup
//! ([`collapse_document`] — hydration reuses the server's attributes
//! verbatim). The space segment appears anywhere only when it differs from
//! the host's own space.
//!
//! The server SSRs canonical routes and 301s slug-form paths to them; the
//! client never resolves slugs, so a slug link is simply a full browser
//! navigation.
//!
//! `space`/`local` live as reactive signals so subscriptions re-subscribe on
//! navigation. Two global listeners keep them in sync with the URL:
//! - a `popstate` listener (back / forward), and
//! - a delegated `click` listener that turns left-clicks on internal
//!   `<a href="/SPACE/LOCAL…">` links into client-side navigation
//!   (pushState) instead of a full reload — so the WebTransport session and
//!   its subscriptions stay live across page changes.

use kolorinko_render::ABOUT_PATH;
use kolorinko_rt::{
    DEFAULT_SPACE_GLOBAL, LocalId, SpaceId, SsrState, format_page_route, parse_local_route,
    parse_page_route, simplify,
};
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
    /// `None`s while the about screen is shown — no page to subscribe to
    /// (subscriptions tear down and content signals clear, so returning to
    /// a page refetches fresh).
    pub space: RwSignal<Option<SpaceId>>,
    pub local: RwSignal<Option<LocalId>>,
    /// Whether the current route is the platform's about screen
    /// ([`ABOUT_PATH`]) — a client-side screen of the app, switched to
    /// without a reload so the WebTransport session stays live.
    pub about: RwSignal<bool>,
    /// The space this origin already names (`window.__DEFAULT_SPACE_ID__`,
    /// injected by the server on a wiki's own domain; `None` on the main
    /// origin). `/L…` paths address it ([`parse_route`]), and its own
    /// canonical hrefs simplify to the space-less form ([`simplify`]).
    pub(crate) default: Option<SpaceId>,
}
/// `window.__DEFAULT_SPACE_ID__`, the space the origin itself names — the
/// script the server injects into every HTML document of a wiki's own
/// configured domain (absent on the main origin, where every URL carries
/// its own space).
fn default_space(window: &web_sys::Window) -> Option<SpaceId> {
    js_sys::Reflect::get(window.as_ref(), &JsValue::from_str(DEFAULT_SPACE_GLOBAL))
        .ok()?
        .as_string()
        .and_then(|s| SpaceId::parse(&s))
}

/// The route a path names when `default` is the space the origin already
/// names: the canonical `/{space}/{local}[/title]`, or — on a wiki's own
/// domain — the space-less `/{local}[/title]` addressed to the default
/// space ([`parse_local_route`]).
fn parse_route(default: Option<SpaceId>, path: &str) -> Option<(SpaceId, LocalId)> {
    parse_page_route(path).or_else(|| Some((default?, parse_local_route(path)?)))
}

impl Router {
    /// Read the route from the current URL — or, on a hydration boot, from
    /// the embedded SSR state (which carries exactly the canonical address
    /// the server rendered with; the about path is never SSR'd through the
    /// app shell, so it can't carry state). An unparseable path is rewritten
    /// (replaceState) to the demo default so a refresh is stable.
    pub(crate) fn bootstrap(initial: Option<&SsrState>) -> Self {
        let default = default_space(&window());
        let path = window().location().pathname().unwrap_or_default();
        // The about screen: no page behind it. Only reachable on a CSR boot
        // — a direct hit is SSR'd into the shell without embedded state, and
        // the ServiceWorker serves the cached shell — so `initial` is always
        // `None` there; the embedded state of a served page is never about.
        // Trailing slashes are insignificant.
        let about = initial.is_none() && path.trim_end_matches('/') == ABOUT_PATH;
        // The about route carries no space/local — and, unlike an
        // unparseable path, must NOT fall back (the URL stays on
        // ABOUT_PATH; no subscriptions either: `None`s tear them down).
        let (space, local) = if about {
            (None, None)
        } else {
            initial
                .map(|s| (s.space, s.local))
                .or_else(|| parse_route(default, &path))
                .map(|(s, l)| (Some(s), Some(l)))
                .unwrap_or_else(|| {
                    if let Ok(h) = window().history() {
                        let _ = h.replace_state_with_url(&JsValue::NULL, "", Some(FALLBACK_PATH));
                    }
                    let (s, l) = parse_page_route(FALLBACK_PATH).expect("fallback parses");
                    (Some(s), Some(l))
                })
        };
        Self {
            space: RwSignal::new(space),
            local: RwSignal::new(local),
            about: RwSignal::new(about),
            default,
        }
    }

    /// Install the `popstate` and delegated `click` listeners, and simplify
    /// the hydrated document's links once (fire-and-forget: the listeners
    /// outlive the reactive scope, for the page lifetime).
    pub(crate) fn install(&self) {
        self.clone().install_popstate();
        self.clone().install_clicks();
        self.collapse_document();
    }

    /// Client-side navigate to an internal path: pushState + update signals,
    /// so subscriptions re-subscribe without a full reload. The pushed
    /// address is the path's simplified form ([`simplify`]). The about path
    /// switches the app to the about screen (page subscriptions off) the
    /// same way.
    pub(crate) fn navigate(&self, path: String) {
        if path == ABOUT_PATH {
            if let Ok(h) = window().history() {
                let _ = h.push_state_with_url(&JsValue::NULL, "", Some(ABOUT_PATH));
            }
            self.set_about(true);
            self.set_space(None);
            self.set_local(None);
            return;
        }
        let Some((space, local)) = self.parse(&path) else {
            return;
        };
        if let Ok(h) = window().history() {
            let _ = h.push_state_with_url(&JsValue::NULL, "", Some(&simplify(self.default, &path)));
        }
        self.set_about(false);
        self.set_space(Some(space));
        self.set_local(Some(local));
    }

    /// The route a path names on this origin — [`parse_route`] against the
    /// origin's default space.
    pub(crate) fn parse(&self, path: &str) -> Option<(SpaceId, LocalId)> {
        parse_route(self.default, path)
    }

    /// The address-bar form of `(space, local)` titled `title` on this
    /// origin: the canonical full route — [`format_page_route`] always
    /// carries the space — simplified against the origin's default space.
    pub(crate) fn address(&self, space: SpaceId, local: LocalId, title: &str) -> String {
        simplify(self.default, &format_page_route(Some(space), local, title))
    }

    fn sync_from_location(&self) {
        if window().location().pathname().unwrap_or_default() == ABOUT_PATH {
            self.set_about(true);
            self.set_space(None);
            self.set_local(None);
            return;
        }
        self.set_about(false);
        if let Some((space, local)) =
            self.parse(&window().location().pathname().unwrap_or_default())
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

    fn set_about(&self, v: bool) {
        if self.about.with_untracked(|s| *s != v) {
            self.about.set(v);
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
    /// route (the space-less `/L…` family included, on a wiki's own domain)
    /// or names the about screen. Slug-form links (`/SPACE/cat:slug`,
    /// `/cat:slug`) and asset-like hrefs (a `.` in the last segment) fall
    /// through to the browser — the server 301s the former to the canonical
    /// form, so a full navigation is correct, just slower.
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
            if path.rsplit('/').next().unwrap_or("").contains('.') {
                return;
            }
            if self.parse(path).is_some() || path == ABOUT_PATH {
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

    /// Simplify the links of the initial, hydrated document: the server
    /// always emits full-weight hrefs and hydration reuses them verbatim
    /// (a hydrated client never re-sets static attributes), so the SSR tree
    /// needs one explicit pass — the only DOM rewrite in the system.
    /// Everything the client builds afterwards is short at construction
    /// (the render `Scope` carries the default space), so nothing further
    /// is ever needed.
    fn collapse_document(&self) {
        let Some(doc) = window().document() else {
            return;
        };
        let Ok(anchors) = doc.query_selector_all("a[href]") else {
            return;
        };
        for i in 0..anchors.length() {
            let Some(el) = anchors.item(i).and_then(|n| n.dyn_into::<Element>().ok()) else {
                continue;
            };
            let Some(href) = el.get_attribute("href") else {
                continue;
            };
            if href.starts_with('/') {
                let short = simplify(self.default, &href);
                if short != href {
                    let _ = el.set_attribute("href", &short);
                }
            }
        }
    }
}
