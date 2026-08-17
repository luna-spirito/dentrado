//! `sfhover` toggling for the Wikidot `#top-bar` dropdown menu.
//!
//! Site themes target the legacy suckerfish class `.top-bar li.sfhover`, not
//! `:hover`, so a delegated handler pair flips that class on the enclosing
//! top-level `<li>`. Attached once to `document` (the handlers themselves
//! ignore events outside `#top-bar`), which keeps event wiring out of the
//! shared SSR layout: the server renders pure HTML, and the listeners attach
//! identically whether the layout was hydrated or client-rendered.

use wasm_bindgen::{JsCast, closure::Closure};
use web_sys::MouseEvent;

/// Install the delegated `mouseover` / `mouseout` pair (fire-and-forget: they
/// outlive the reactive scope, for the page lifetime).
pub fn install() {
    let over = Closure::<dyn Fn(MouseEvent)>::new(on_over);
    let out = Closure::<dyn Fn(MouseEvent)>::new(on_out);
    let doc = web_sys::window()
        .expect("no window")
        .document()
        .expect("no document");
    let _ = doc.add_event_listener_with_callback(
        "mouseover",
        over.as_ref().unchecked_ref::<js_sys::Function>(),
    );
    let _ = doc.add_event_listener_with_callback(
        "mouseout",
        out.as_ref().unchecked_ref::<js_sys::Function>(),
    );
    over.forget();
    out.forget();
}

fn on_over(ev: MouseEvent) {
    if let Some(li) = enclosing_top_li(&ev) {
        let _ = li.class_list().add_1("sfhover");
    }
}

fn on_out(ev: MouseEvent) {
    let Some(li) = enclosing_top_li(&ev) else {
        return;
    };
    let left = ev
        .related_target()
        .and_then(|rt| rt.dyn_into::<web_sys::Node>().ok())
        .is_none_or(|rt| !li.contains(Some(&rt)));
    if left {
        let _ = li.class_list().remove_1("sfhover");
    }
}

/// Nearest ancestor `<li>` whose grandparent is `#top-bar` — a top-level menu
/// entry — climbing past nested submenus.
fn enclosing_top_li(ev: &MouseEvent) -> Option<web_sys::Element> {
    let mut el = ev.target()?.dyn_into::<web_sys::Element>().ok()?;
    loop {
        if el.tag_name().eq_ignore_ascii_case("li")
            && el
                .parent_element()
                .and_then(|ul| ul.parent_element())
                .is_some_and(|g| g.id() == "top-bar")
        {
            return Some(el);
        }
        el = el.parent_element()?;
    }
}
