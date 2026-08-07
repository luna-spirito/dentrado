//! `sfhover` toggling for the Wikidot `#top-bar` dropdown menu.
//!
//! Site themes target the legacy suckerfish class `.top-bar li.sfhover`, not
//! `:hover`, so a delegated handler pair on `#top-bar` flips that class on the
//! enclosing top-level `<li>`. No DOM rewriting and no per-render work — the
//! list itself is rendered by the shared renderer; we only toggle a class.

use wasm_bindgen::JsCast;
use web_sys::MouseEvent;

pub fn on_over(ev: MouseEvent) {
    if let Some(li) = enclosing_top_li(&ev) {
        let _ = li.class_list().add_1("sfhover");
    }
}

pub fn on_out(ev: MouseEvent) {
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
