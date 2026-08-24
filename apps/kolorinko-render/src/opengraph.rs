//! OpenGraph card for a served page: the lead of a compiled article flattened
//! into `og:…` `<meta>` tags for `<head>`. Unfurlers (Discord, Telegram, …)
//! never run the app, so the card must ride the SSR document — the hydrated
//! client never looks at `<head>`, which is why this stays server-side.

use kolorinko_rt::SiteShell;
use kolorinko_wikitext::{ArticleView, Content, Node, TextObj};

use crate::layout::html_escape;

/// Cap on `og:description`: about two preview lines everywhere.
const DESC_LIMIT: usize = 200;

/// The `og:…` `<meta>` tags describing `page` under `shell`. `host` — the
/// request's `host[:port]`, always TLS-served — absolutizes the (relative,
/// `/repo/…`) first-image URL; `canonical` — the page's canonical absolute
/// URL — is `og:url`. Both `None` in the debug CLI: no `og:url`, the image
/// stays relative.
pub fn meta(
    site: &str,
    shell: &SiteShell,
    page: &ArticleView,
    host: Option<&str>,
    canonical: Option<&str>,
) -> String {
    let site_name = shell.title.clone().unwrap_or_else(|| site.to_string());
    let title = if page.meta.title.is_empty() {
        site_name.clone()
    } else {
        page.meta.title.clone()
    };
    let mut tags =
        tag("og:title", &title) + &tag("og:site_name", &site_name) + &tag("og:type", "article");
    if let Some(u) = canonical {
        tags += &tag("og:url", u);
    }
    let desc = truncate(&lead_text(&page.content));
    if !desc.is_empty() {
        tags += &tag("og:description", &desc);
    }
    if let Some(src) = first_image(&page.content) {
        tags += &tag("og:image", &absolutize(host, &src));
    }
    tags
}

/// One `<meta property="og:…" content="…">` tag.
fn tag(prop: &str, content: &str) -> String {
    format!(
        r#"<meta property="{prop}" content="{}">"#,
        html_escape(content)
    )
}

/// Absolutize a page-relative URL against the request host; external URLs and
/// hostless (debug-CLI) renders pass through unchanged.
fn absolutize(host: Option<&str>, url: &str) -> String {
    match (host, url.starts_with('/')) {
        (Some(h), true) => format!("https://{h}{url}"),
        _ => url.to_string(),
    }
}

/// The page's lead as plain text: text runs in document order, whitespace
/// collapsed, word-boundary truncated to [`DESC_LIMIT`] (+`…`).
fn lead_text(content: &Content) -> String {
    let mut out = String::new();
    collect_runs(content, &mut out);
    truncate(&out.split_whitespace().collect::<Vec<_>>().join(" "))
}

/// Gather text runs (space-joined) until the lead is over-full — `DESC_LIMIT`
/// chars of prose fit in four times that many UTF-8 bytes. Headings restate
/// the title and `Raw` is unparsed source: neither is prose.
fn collect_runs(content: &Content, out: &mut String) {
    for node in content {
        if out.len() > DESC_LIMIT * 4 {
            return;
        }
        match node {
            Node::Text(TextObj::Plain(s)) if !s.trim().is_empty() => {
                out.push(' ');
                out.push_str(s.trim());
            }
            Node::Raw(_) | Node::Heading { .. } => {}
            other => other.visit_node(&mut |c| collect_runs(c, out)),
        }
    }
}

/// First image URL in document order — the lead's infobox image on wiki pages
/// with one. Unresolvable sources (variable slots left in the URL) are skipped.
fn first_image(content: &Content) -> Option<String> {
    for node in content {
        let found = match node {
            Node::Image { source, .. } => TextObj::plain_concat(source).filter(|s| !s.is_empty()),
            other => {
                let mut found = None;
                other.visit_node(&mut |c| {
                    if found.is_none() {
                        found = first_image(c);
                    }
                });
                found
            }
        };
        if found.is_some() {
            return found;
        }
    }
    None
}

/// Cut to [`DESC_LIMIT`] chars on the last word boundary, marking with `…`;
/// shorter (or empty) input passes through.
fn truncate(s: &str) -> String {
    if s.chars().count() <= DESC_LIMIT {
        return s.to_string();
    }
    let cut: String = s.chars().take(DESC_LIMIT).collect();
    let head = cut.trim_end();
    match head.rfind(' ') {
        Some(at) => format!("{}…", &head[..at]),
        None => format!("{head}…"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kolorinko_wikitext::ArticleMeta;

    fn page(content: Content) -> ArticleView {
        ArticleView {
            meta: ArticleMeta {
                title: "A Page".into(),
                slug: "docs:guide".into(),
                ..Default::default()
            },
            revisions: Vec::new(),
            content,
            deps: Vec::new(),
        }
    }

    fn shell() -> SiteShell {
        SiteShell {
            title: Some("Site Title".into()),
            subtitle: None,
            theme_root: None,
            nav_top: page(Vec::new()),
            nav_side: page(Vec::new()),
        }
    }

    fn txt(s: &str) -> Node {
        Node::Text(TextObj::Plain(s.into()))
    }

    #[test]
    fn card_basics() {
        let mut p = page(vec![
            txt("First "),
            Node::Container {
                kind: kolorinko_wikitext::ContainerKind::Style(kolorinko_wikitext::TextStyle::Bold),
                content: vec![txt("paragraph")],
            },
            txt("of the guide."),
        ]);
        p.meta.slug = "start".into();
        let m = meta(
            "kolorinko",
            &shell(),
            &p,
            Some("wiki.example:4433"),
            Some("https://wiki.example:4433/Sxx/Lyy/a-page"),
        );
        assert!(m.contains(r#"<meta property="og:title" content="A Page">"#));
        assert!(m.contains(r#"<meta property="og:site_name" content="Site Title">"#));
        assert!(m.contains(r##"og:url" content="https://wiki.example:4433/Sxx/Lyy/a-page""##));
        assert!(m.contains(
            r#"<meta property="og:description" content="First paragraph of the guide.">"#
        ));
        assert!(!m.contains("og:image"));
    }

    #[test]
    fn nested_image_and_colon_slug() {
        let p = page(vec![Node::Container {
            kind: kolorinko_wikitext::ContainerKind::Quote,
            content: vec![Node::Image {
                align: None,
                source: vec![TextObj::Plain("/repo/x/files/ab/cd/hash.png".into())],
                params: Default::default(),
            }],
        }]);
        let m = meta(
            "kolorinko",
            &shell(),
            &p,
            Some("wiki.example"),
            Some("https://wiki.example/kolorinko/docs/guide"),
        );
        assert!(
            m.contains(r##"og:image" content="https://wiki.example/repo/x/files/ab/cd/hash.png""##)
        );
        assert!(m.contains(r##"og:url" content="https://wiki.example/kolorinko/docs/guide""##));
    }

    #[test]
    fn description_truncates_on_word_boundary() {
        let long = "word ".repeat(60);
        let p = page(vec![txt(&long)]);
        let m = meta("kolorinko", &shell(), &p, None, None);
        let at =
            m.find(r#"og:description" content=""#).unwrap() + r#"og:description" content=""#.len();
        let desc = &m[at..at + m[at..].find('"').unwrap()];
        assert!(desc.chars().count() <= DESC_LIMIT + 1 && desc.ends_with('…'));
        assert!(desc.ends_with("word…"));
    }

    #[test]
    fn headings_and_raw_stay_out() {
        let p = page(vec![
            Node::Heading {
                level: 1,
                anchor: None,
                content: vec![txt("A Page")],
            },
            Node::Raw("[[module junk".into()),
            txt("Only this."),
        ]);
        let m = meta("kolorinko", &shell(), &p, None, None);
        assert!(m.contains(r#"<meta property="og:description" content="Only this.">"#));
    }

    #[test]
    fn empty_title_falls_back_to_site() {
        let mut p = page(vec![txt("hello")]);
        p.meta.title.clear();
        let m = meta("kolorinko", &shell(), &p, None, None);
        assert!(m.contains(r#"<meta property="og:title" content="Site Title">"#));
    }
}
