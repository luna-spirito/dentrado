//! Path-based client router: the single source of truth for which page the
//! app shows. The route is `/<site>/<category?>/<page>` (Wikidot's
//! `category:name` flattened to `/`), parsed from `location.pathname`.
//!
//! `site`/`slug` live as reactive signals so subscriptions re-subscribe on
//! navigation. Two global listeners keep them in sync with the URL:
//! - a `popstate` listener (back / forward), and
//! - a delegated `click` listener that turns left-clicks on internal `<a>`
//!   into client-side navigation (pushState) instead of a full reload — so the
//!   WebTransport session and its subscriptions stay live across page changes.

use kolorinko_rt::SafePathComponent;
use leptos::prelude::*;
use wasm_bindgen::{JsCast, JsValue, closure::Closure};
use web_sys::{Element, Event, MouseEvent};

pub(crate) type Slug = (Option<SafePathComponent>, SafePathComponent);

const FALLBACK_SITE: &str = "obscurative";
const FALLBACK_PAGE: &str = "syntax";

fn window() -> web_sys::Window {
    web_sys::window().expect("no window")
}

/// `/<site>/<page>` or `/<site>/<category>/<page>` → `(site, slug)`. `None` for
/// anything else (bare `/`, asset paths, wrong arity, unsafe segments).
fn parse_path(path: &str) -> Option<(SafePathComponent, Slug)> {
    let segs: Vec<&str> = path
        .trim_start_matches('/')
        .split(['/', '#', '?'])
        .filter(|s| !s.is_empty())
        .collect();
    match segs.as_slice() {
        [s, p] => Some((
            SafePathComponent::new((*s).into())?,
            (None, SafePathComponent::new((*p).into())?),
        )),
        [s, c, p] => Some((
            SafePathComponent::new((*s).into())?,
            (
                Some(SafePathComponent::new((*c).into())?),
                SafePathComponent::new((*p).into())?,
            ),
        )),
        _ => None,
    }
}

#[derive(Clone)]
pub(crate) struct Router {
    pub site: RwSignal<SafePathComponent>,
    pub slug: RwSignal<Slug>,
}

impl Router {
    /// Read the route from the current URL. A bare or unparseable path is
    /// rewritten (replaceState) to the demo default so a refresh is stable.
    pub(crate) fn bootstrap() -> Self {
        let path = window().location().pathname().unwrap_or_default();
        let (site, slug) = match parse_path(&path) {
            Some(v) => v,
            None => {
                let href = format!("/{FALLBACK_SITE}/{FALLBACK_PAGE}");
                if let Ok(h) = window().history() {
                    let _ = h.replace_state_with_url(&JsValue::NULL, "", Some(&href));
                }
                (
                    SafePathComponent::new(FALLBACK_SITE.into()).unwrap(),
                    (None, SafePathComponent::new(FALLBACK_PAGE.into()).unwrap()),
                )
            }
        };
        Self {
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
        let Some((site, slug)) = parse_path(&path) else {
            return;
        };
        if let Ok(h) = window().history() {
            let _ = h.push_state_with_url(&JsValue::NULL, "", Some(&path));
        }
        self.set_route(site, slug);
    }

    fn sync_from_location(&self) {
        let path = window().location().pathname().unwrap_or_default();
        if let Some((site, slug)) = parse_path(&path) {
            self.set_route(site, slug);
        }
    }

    /// Set the route signals only when they actually change. `RwSignal::set`
    /// notifies subscribers unconditionally (Leptos does no equality dedup), so a
    /// plain `set` here would tear down and re-subscribe every signal-keyed
    /// subscription — notably the `shell` gear — on every same-site navigation,
    /// even though the site didn't change.
    fn set_route(&self, site: SafePathComponent, slug: Slug) {
        if self.site.with_untracked(|s| *s != site) {
            println!("Setting new site");
            self.site.set(site);
        }
        if self.slug.with_untracked(|s| *s != slug) {
            self.slug.set(slug);
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
            if parse_path(path).is_some() {
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
