//! The platform's own page (`/~/about`): a hand-designed Dentrado-styled
//! screen — deliberately *not* the Wikidot skeleton the mirrored pages wear,
//! the point is to show the engine's new face. `/~/…` is the namespace of
//! the app's auxiliary screens — pages kolorinko-web renders itself, and for
//! which the ServiceWorker serves the app shell — sibling to the
//! server-owned `/-…` system namespace (CA blobs, future platform APIs:
//! passthrough, never the shell). An SSR page like any other, minus the
//! parts it has none of: the server seals [`about_page`] into the same
//! `index.html` template (no state to embed — the screen carries no data),
//! the client boots CSR there (clearing the sealed markup — identical to
//! what it re-renders — and owning routing from [`ABOUT_PATH`]); in a live
//! app the session and its subscriptions stay alive (switching to the page
//! is part of the app, not a navigation away from it), and the ServiceWorker
//! serves its navigations the cached shell once warm.
//!
//! The content is hardcoded markup (not markdown-over-a-renderer): full
//! control over the design is the feature. The text is the manifesto,
//! verbatim. The `<style>` element
//! rides the view itself, so entering the screen repaints `body` and
//! leaving it restores the wiki look — no global stylesheet to manage.

use leptos::prelude::*;

/// The route this screen answers: `/~/about`. The `/~/…` namespace is the
/// app's auxiliary screens — kolorinko-web renders them (the ServiceWorker
/// hands their navigations the app shell) — disjoint from the server-owned
/// `/-…` system namespace and from content (ids start with 'S'/'L', slugs
/// are lowercase; `~` is none of those).
pub const ABOUT_PATH: &str = "/~/about";

/// The screen's whole stylesheet. Rendered as a `<style>` child of the view
/// (a raw-text element: no entity escaping, so no `<` may ever occur here —
/// `</style` would end it early; asserted in the tests). `body` rules are
/// global on purpose: the wiki's base stylesheet (loaded in `<head>`) loses
/// to this block by document order while the screen is mounted, and loses
/// nothing once it unmounts.
const ABOUT_CSS: &str = "\
body{margin:0;color:#d8dee9;background:radial-gradient(55rem 28rem at 50% -8rem,rgba(199,92,22,.16),transparent 65%),#0c0e12;color-scheme:dark;font:16px/1.75 ui-sans-serif,system-ui,Segoe UI,Roboto,Helvetica Neue,Arial,sans-serif;-webkit-font-smoothing:antialiased}\
.dntrd-about main{width:100%;max-width:46rem;margin:0 auto;padding:0 1.4rem}\
.dntrd-about .hero{padding:5rem 0 2.5rem;text-align:center}\
.dntrd-about .hero svg{width:64px;height:64px;display:block;margin:0 auto 1.1rem}\
.dntrd-about .hero h1{margin:0;font-size:2.6rem;font-weight:800;letter-spacing:.04em;color:#f4f6f9}\
.dntrd-about .hero h1::after{content:'';display:block;width:3.2rem;height:3px;margin:1.1rem auto 0;background:linear-gradient(90deg,transparent,#c75c16,transparent)}\
.dntrd-about .hero p{margin:1.1rem 0 0;color:#8b93a3;font-size:.95rem}\
.dntrd-about article>p:first-of-type{font-size:1.16rem;color:#e9edf3}\
.dntrd-about article h1{margin:3.4rem 0 1rem;padding-top:1.6rem;border-top:1px solid rgba(216,222,233,.09);font-size:.85rem;font-weight:700;letter-spacing:.16em;text-transform:uppercase;color:#e6965a}\
.dntrd-about a{color:#e6965a;text-decoration:none;border-bottom:1px solid rgba(230,150,90,.35)}\
.dntrd-about a:hover{border-bottom-color:#e6965a}\
.dntrd-about ul{padding-left:1.4rem;margin:1rem 0}\
.dntrd-about li{margin:.35rem 0}\
.dntrd-about li::marker{color:#c75c16}\
.dntrd-about ul ul{margin:.35rem 0}\
.dntrd-about code{background:#141821;border:1px solid rgba(216,222,233,.09);border-radius:4px;padding:.1em .35em;font-size:.9em}\
.dntrd-about ::selection{background:rgba(199,92,22,.45)}\
";

/// The logo mark (two strokes of the Dentrado glyph), the `logo-ng.svg`
/// shapes inlined so the view carries no external reference.
const LOGO: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" xmlns:graphite="https://graphite.art" viewBox="0 0 64 64" width="64" height="64"><defs><clipPath id="artboard-12031998624894171892"><rect x="0" y="0" width="64" height="64" /></clipPath></defs><g>
<rect fill="#ffffff" fill-opacity="0" x="0" y="0" width="64" height="64"/>
<g clip-path="url(#artboard-12031998624894171892)">
	<g transform="matrix(1.35,0,0,1.35,32,32)">
		<path d="M0.0000000000000009797174393178826,-16 L13.85640646055102,7.999999999999997 L-13.856406460551016,8.000000000000005 L0.0000000000000009797174393178826,-16 Z" transform="matrix(0.707106781,0.707106781,-0.707106781,0.707106781,-7.071067812,7.071067812)" fill="none" stroke-width="4" stroke-linejoin="bevel" stroke="#c75c16"/>
		<path d="M0.0000000000000009797174393178826,-16 L13.85640646055102,7.999999999999997 L-13.856406460551016,8.000000000000005 L0.0000000000000009797174393178826,-16 Z" transform="matrix(-0.707106781,-0.707106781,0.707106781,-0.707106781,7.071067812,-7.071067812)" fill="none" stroke-width="4" stroke-linejoin="bevel" stroke="#c75c16"/>
	</g>
</g></g></svg>"##;

/// The about screen: hero, the manifesto as hand-set markup. Mode-agnostic
/// like [`layout`](crate::layout) — the server seals it into the shell
/// template, the client renders it live.
pub fn about_page() -> AnyView {
    view! {
        <style>{ABOUT_CSS}</style>
        <div class="dntrd-about">
            <main>
                <header class="hero">
                    <div inner_html=LOGO></div>
                    <h1>"Dentrado"</h1>
                    <p>"open-source (AGPLv3) wiki engine, developed by Luna Spirit (that's me, hi)"</p>
                </header>
                <article>
                    <p>"Contacts:"</p>
                    <ul>
                        <li>
                            "Discord server: "
                            <a href="https://discord.gg/2nXMjqSCGq">"https://discord.gg/2nXMjqSCGq"</a>
                        </li>
                        <li>
                            "Luna Spirito"
                            <ul>
                                <li>
                                    "Email: "
                                    <a href="mailto:guardspirit@protonmail.com">"guardspirit@protonmail.com"</a>
                                </li>
                                <li>"Discord: gardanta_spirito"</li>
                                <li>
                                    "Telegram: "
                                    <a href="https://t.me/luna_spirito">"https://t.me/luna_spirito"</a>
                                </li>
                            </ul>
                        </li>
                    </ul>

                    <h1>"Wikidot evacuation initiative"</h1>
                    <p>
                        "Right now, my focus is to make sure that Wikidot projects aren't gone
                        as soon as Wikidot does. As part of this plan:"
                    </p>
                    <ul>
                        <li>
                            "I create backups of Wikidot projects and host them publicly: "
                            <a href="https://github.com/luna-spirito/wikidot-kolorinko-export">
                                "https://github.com/luna-spirito/wikidot-kolorinko-export"
                            </a>
                        </li>
                        <li>
                            "I deploy a (for now, read-only) mirror for projects that desire one,
                            taken down on first request, aiming to be as self-contained as
                            possible. If you want one, please contact me via links above."
                        </li>
                    </ul>
                    <p>
                        "I'm trying to match Wikidot's behaviour & rendering as close as
                        possible, so that minimal intervention is necessary, but:"
                    </p>
                    <ul>
                        <li>
                            "We're still in beta, there are lots of known rendering issues I had
                            no time to fix just yet, but I'm getting there."
                        </li>
                        <li>
                            "Fully matching wikidot's broken behaviour is not always possible
                            unless I deliberately reimplement its bugs in Dentrado."
                        </li>
                    </ul>

                    <h1>"Long-term Goal"</h1>
                    <p>
                        "My goal is to develop "
                        <strong>"the"</strong>
                        " platform for creative textual & web content, providing all the
                        tooling necessary to:"
                    </p>
                    <ul>
                        <li>"manage large-scale knowledge databases (inspirational example: Obsidian),"</li>
                        <li>"brainstorm ideas,"</li>
                        <li>"publish and share content,"</li>
                        <li>"manage communities,"</li>
                        <li>"fearlessly collaborate on the content."</li>
                    </ul>
                    <p>"... all while trying my best to ensure:"</p>
                    <ul>
                        <li>"preservation of human creativity,"</li>
                        <li>
                            "performant always-available service,"
                            <ul>
                                <li>
                                    "the intention is to provide a local-first application,
                                    and, generally, make Dentrado the Waffle House of the
                                    internet (always open, even in a hurricane), but this
                                    takes time."
                                </li>
                            </ul>
                        </li>
                        <li>"user-friendly experience built on top of modern technologies."</li>
                    </ul>

                    <h1>"Technology"</h1>
                    <p>
                        "Dentrado is unique in a way that it's built on top of experimental
                        database technology, intended to make it much easier to implement
                        complex features."
                    </p>
                    <p>"Key quirks of our approach:"</p>
                    <ul>
                        <li>
                            "Embedded, easy-to-deploy architecture: dentrado is just a single
                            binary + a bunch of static assets. You can easily deploy it
                            yourself: all it takes is a few lines in NixOS config."
                        </li>
                        <li>"We're HTTP/3-first."</li>
                        <li>
                            "We're realtime-first. Changes made to the website are
                            automatically propagated to all connected clients."
                        </li>
                        <li>
                            "We're multithreaded, for real: the core is built on top of
                            Thread-per-Core architecture, which is meant to make it scale
                            nicely with stronger hardware."
                        </li>
                        <li>
                            "Pluggable architecture: components are interchangeable, making it
                            easy to swap functionality instead of forcing every use case
                            through a single narrow gate."
                        </li>
                    </ul>
                    <p>"Key future priorities:"</p>
                    <ul>
                        <li>
                            "Distributed, low-latency reliable network, powered by eventual
                            consistency. Each Dentrado instance is both capable of working
                            autonomously and synchronizing its state with others as soon as
                            this is possible."
                        </li>
                        <li>
                            "Federated network. I don't trust any single point of failure,
                            and dentrado.art isn't meant to be one. The service should be
                            hostable by anyone. And, if one instance falls, others should be
                            able to continue serving its content."
                        </li>
                        <li>
                            "Version-control-system architecture, making it possible to freely
                            experiment with existing content and roll back."
                        </li>
                        <li>
                            "Local-first, Obsidian-like experience. Dentrado should be able to
                            serve as an encrypted personal knowledge database, and shouldn't
                            necessarily depend on network connection for normal function."
                        </li>
                    </ul>
                    <p>
                        "Codebase is written mostly in Rust, GitHub repo: "
                        <a href="https://github.com/luna-spirito/dentrado/">
                            "https://github.com/luna-spirito/dentrado/"
                        </a>
                        " LLM-assistance is utilized to make solo developing the project of "
                        <em>"this"</em>
                        " scale feasible. The design is always human-written, the code is
                        constantly verified."
                    </p>
                </article>
            </main>
        </div>
    }
    .into_any()
}

#[cfg(test)]
mod tests {
    use super::{ABOUT_CSS, about_page};
    use leptos::prelude::RenderHtml as _;

    /// `<style>` is a raw-text element: the serializer writes the stylesheet
    /// verbatim, so a `<` would end it early (and an entity would render
    /// literally). The stylesheet avoids `<` by construction.
    #[test]
    fn css_survives_raw() {
        assert!(!ABOUT_CSS.contains('<'));
        let page = about_page().to_html();
        assert!(page.contains(ABOUT_CSS));
    }

    /// The page carries the manifesto, verbatim — down to its opening
    /// sentence and its "..." ellipsis — plus the placeholder marking the
    /// one piece of copy the design wants and the manifesto doesn't carry.
    #[test]
    fn page_carries_manifesto() {
        let page = about_page().to_html();
        assert!(page.contains("Dentrado, open-source (AGPLv3) wiki engine"));
        assert!(page.contains("Wikidot evacuation initiative"));
        assert!(page.contains("Long-term Goal"));
        assert!(page.contains(r#"href="https://t.me/luna_spirito""#));
        assert!(page.contains("... all while trying my best to ensure:"));
        assert!(page.contains("Waffle House"));
    }
}
