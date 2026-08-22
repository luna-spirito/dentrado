//! Path-based client router: the single source of truth for which page the
//! app shows. Two URL families address a page:
//!
//! - **canonical** `/<space>/<local>` (a 22-char space id, an 11-char local
//!   id) — the route the server SSRs; the client resolves it to the serving
//!   `site`/`slug` through the `page_addr` gear over WebTransport, so a
//!   hydration boot seeds the resolution from the embedded SSR state (no
//!   round-trip), while a CSR boot (ServiceWorker shell) resolves live;
//! - **legacy** `/<site>/<category?>/<page>` (Wikidot's `category:name`
//!   flattened to `/`) — what in-content links still carry; rendered in
//!   place, no canonical identity needed client-side.
//!
//! `space`/`local` and `site`/`slug` live as reactive signals so
//! subscriptions re-subscribe on navigation. Two global listeners keep them in
//! sync with the URL:
//! - a `popstate` listener (back / forward), and
//! - a delegated `click` listener that turns left-clicks on internal `<a>`
//!   into client-side navigation (pushState) instead of a full reload — so the
//!   WebTransport session and its subscriptions stay live across page changes.

use kolorinko_rt::{LocalId, SafePathComponent, SpaceId, SsrState, parse_route};
use leptos::prelude::*;
use wasm_bindgen::{JsCast, JsValue, closure::Closure};
use web_sys::{Element, Event, MouseEvent};

pub(crate) type Slug = kolorinko_rt::Slug;

const FALLBACK_SITE: &str = "obscurative";
const FALLBACK_PAGE: &str = "syntax";

/// The demo default route for a bare or unparseable path: rewritten
/// (replaceState) so a refresh is stable.
fn window() -> web_sys::Window {
    web_sys::window().expect("no window")
}

fn fallback() -> ClientRoute {
    let href = format!("/{FALLBACK_SITE}/{FALLBACK_PAGE}");
    if let Ok(h) = window().history() {
        let _ = h.replace_state_with_url(&JsValue::NULL, "", Some(&href));
    }
    ClientRoute::Legacy {
        site: SafePathComponent::new(FALLBACK_SITE.into()).unwrap(),
        slug: (None, SafePathComponent::new(FALLBACK_PAGE.into()).unwrap()),
    }
}

/// What a path names. Canonical routes with a decorative third segment
/// (`/space/local/slug`) normalize to the two-segment form (the server 301s
/// them; the client just never pushes the slug).
#[derive(Clone, PartialEq)]
enum ClientRoute {
    Canonical { space: SpaceId, local: LocalId },
    Legacy { site: SafePathComponent, slug: Slug },
}

/// Parse an internal path into a route: canonical first (a 22-char
/// base64url first segment is syntactically distinct from any legacy site
/// name), then the legacy `/site[/cat/page]` form.
fn route_for(path: &str) -> Option<ClientRoute> {
    let mut segs = path.trim_start_matches('/').split('/');
    let first = segs.next()?;
    if let Some(space) = SpaceId::parse(first)
        && let Some(local) = segs.next().and_then(LocalId::parse)
    {
        return Some(ClientRoute::Canonical { space, local });
    }
    parse_route(path).map(|(site, slug)| ClientRoute::Legacy { site, slug })
}

#[derive(Clone)]
pub(crate) struct Router {
    /// The canonical route, when the URL is `/SPACE/LOCAL` (`None` on legacy
    /// paths): the `page_addr` subscription's keys.
    pub space: RwSignal<Option<SpaceId>>,
    pub local: RwSignal<Option<LocalId>>,
    /// The resolved dataset address, once known (`None` while a canonical
    /// route is still resolving). Legacy routes set it directly.
    pub site: RwSignal<Option<SafePathComponent>>,
    pub slug: RwSignal<Option<Slug>>,
}

impl Router {
    /// Read the route from the current URL — or, on a hydration boot, from
    /// the embedded SSR state (whose `addr`/`route` are exactly the resolved
    /// route the server rendered with). A bare or unparseable path is
    /// rewritten (replaceState) to the demo default so a refresh is stable.
    pub(crate) fn bootstrap(initial: Option<&SsrState>) -> Self {
        // On a hydration boot the embedded state carries the exact route the
        // server rendered with (canonical space/local, or just the resolved
        // address for a legacy URL); on a CSR boot parse the location.
        let (route, addr) = match initial {
            Some(s) => match s.route {
                Some((space, local)) => (
                    ClientRoute::Canonical { space, local },
                    Some((s.addr.site.clone(), s.addr.slug.clone())),
                ),
                None => (
                    ClientRoute::Legacy {
                        site: s.addr.site.clone(),
                        slug: s.addr.slug.clone(),
                    },
                    None,
                ),
            },
            None => (
                route_for(&window().location().pathname().unwrap_or_default())
                    .unwrap_or_else(fallback),
                None,
            ),
        };
        // The SSR `addr` seeds the page/shell subscriptions for a canonical
        // hydration boot (hash-checked, no resolution round-trip); a resolving
        // CSR boot starts with the address unknown.
        let (space, local, site, slug) = match route {
            ClientRoute::Canonical { space, local } => {
                let (site, slug) = match addr {
                    Some((site, slug)) => (Some(site), Some(slug)),
                    None => (None, None),
                };
                (Some(space), Some(local), site, slug)
            }
            ClientRoute::Legacy { site, slug } => (None, None, Some(site), Some(slug)),
        };
        Self {
            space: RwSignal::new(space),
            local: RwSignal::new(local),
            site: RwSignal::new(site),
            slug: RwSignal::new(slug),
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
        let Some(route) = route_for(&path) else {
            return;
        };
        let path = match &route {
            // Never carry the decorative slug: the address bar shows the
            // canonical two-segment form (mirroring the server's 301).
            ClientRoute::Canonical { space, local } => format!("/{space}/{local}"),
            ClientRoute::Legacy { .. } => path,
        };
        if let Ok(h) = window().history() {
            let _ = h.push_state_with_url(&JsValue::NULL, "", Some(&path));
        }
        self.apply(route);
    }

    fn sync_from_location(&self) {
        if let Some(route) = route_for(&window().location().pathname().unwrap_or_default()) {
            self.apply(route);
        }
    }

    /// Apply a route to the signals, each only when it actually changes
    /// (`RwSignal::set` notifies unconditionally — a plain `set` would tear
    /// down and re-subscribe every signal-keyed subscription, notably the
    /// `shell` gear, on same-site navigations).
    ///
    /// A canonical route clears the resolved address: the `page_addr`
    /// subscription re-fills it, and until then the page/shell queries are
    /// `None` (nothing stale renders under the new URL).
    fn apply(&self, route: ClientRoute) {
        match route {
            ClientRoute::Canonical { space, local } => {
                self.set_opt_space(Some(space));
                self.set_opt_local(Some(local));
                self.set_site(None);
                self.set_slug(None);
            }
            ClientRoute::Legacy { site, slug } => {
                self.set_opt_space(None);
                self.set_opt_local(None);
                self.set_site(Some(site));
                self.set_slug(Some(slug));
            }
        }
    }

    /// The `page_addr` resolution landed: set the serving address for the
    /// current canonical route (does not touch `space`/`local`).
    pub(crate) fn set_resolved(&self, site: SafePathComponent, slug: Slug) {
        self.set_site(Some(site));
        self.set_slug(Some(slug));
    }

    fn set_opt_space(&self, v: Option<SpaceId>) {
        if self.space.with_untracked(|s| *s != v) {
            self.space.set(v);
        }
    }

    fn set_opt_local(&self, v: Option<LocalId>) {
        if self.local.with_untracked(|s| *s != v) {
            self.local.set(v);
        }
    }

    fn set_site(&self, v: Option<SafePathComponent>) {
        if self.site.with_untracked(|s| *s != v) {
            self.site.set(v);
        }
    }

    fn set_slug(&self, v: Option<Slug>) {
        if self.slug.with_untracked(|s| *s != v) {
            self.slug.set(v);
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
    /// them into client-side navigation. Asset-like hrefs (a `.` in the last
    /// segment, e.g. `/pkg/x.js`) and modified clicks fall through to the
    /// browser so new-tab / open-in-background keep working.
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
            if route_for(path).is_some() {
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
