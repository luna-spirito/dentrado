use super::*;

// =========================================================================
// `code_block` gear — Wikidot's `/code/N` endpoint, served locally
// =========================================================================
//
// Wikidot pages load theme components by `@import`ing a page's code block:
// `@import url(http://www.rpc-wiki.net/component:theme/code/1);` serves the
// first `[[code]]` block of `component:theme` as CSS (verified against the
// live site): `<host>/<page>/code/N` 302s to `<host>.wdfiles.com`'s
// `local--code/<page>/<N>`, the MIME is `text/css; charset=utf-8` when the
// opener declared `type="css"` (any case), otherwise `text/plain`, and a page
// with fewer than N blocks gets an odd `200 text/plain` body.
//
// Kolorinko serves the same resource on the legacy slug family —
// `/SPACE/<cat:><name>/code/N` on the main origin, `/<cat:><name>/code/N` on
// a wiki's own domain (see `crate::respond`); the render-time rewrite in
// [`crate::wikidot_page::resources`] points CSS `@import`s here. The body
// goes through Wikidot's verbatim-stylesheet pipeline
// ([`wikidot_verbatim`], shared with `[[module css]]`); one deliberate
// divergence remains — a plain 404 for a missing block or page (Wikidot
// answers an odd `200 text/plain`), which a CSS `@import` silently
// no-ops anyway.

/// No carry-over state: a run re-derives the block from the shared parse;
/// the compressed output is cached by the runtime (`shared`).
#[derive(Default, Clone, Debug)]
pub(crate) struct CodeBlockCache;

/// Extract the Nth (1-indexed) `[[code]]` block of the page's latest
/// revision, straight from the shared parse: the gear is a `follow` lens
/// over [`article_latest_parsed`](crate::wikidot_page::article_latest_parsed)
/// — statically bound to the `(space, local)` record derived from its own id,
/// co-located with it, and handed the parse as a `&ArticleView` borrow
/// (zero-copy; no cross-core shipping of the whole tree to extract one
/// block). It re-runs exactly when the parse output changes — which itself
/// re-runs only when the page body does; the HTTP layer then revalidates
/// via the block's ETag. Wikidot numbers `/code/N` over the page's **own**
/// source, so the unresolved parse — includes and ListPages templates still
/// directives — is exactly the right index, and the parser is the single
/// authority on what a code block is (an unclosed opener is plain text; a
/// nested-looking `[[code]]` pairs LIFO as the engine pairs it). The body
/// is put through Wikidot's serving pipeline, then compressed and the ETag
/// computed **once here** (the asset pattern), never per request. `None`
/// when the page or the Nth block is absent.
pub(crate) fn code_block(
    _space: SpaceId,
    _local: LocalId,
    n: u32,
    parsed: &ArticleView,
) -> Option<CodeBlock> {
    Some(serve(nth_code_block(&parsed.content, n)?))
}

/// Shape a located block into the served [`CodeBlock`] (the body through
/// [`wikidot_verbatim`], the `type="css"` MIME hint, ETag of the served
/// body).
fn serve(block: CodeNode<'_>) -> CodeBlock {
    let served = wikidot_verbatim(block.raw);
    CodeBlock {
        css: block
            .ty
            .as_deref()
            .is_some_and(|t| t.eq_ignore_ascii_case("css")),
        etag: etag_of(served.as_bytes()),
        body: crate::assets::compress(served.into_bytes()),
    }
}

/// A located code block — the opener's `type` attribute as written and the
/// verbatim interior — borrowed from the parsed tree.
pub(super) struct CodeNode<'a> {
    pub(super) ty: &'a Option<String>,
    pub(super) raw: &'a String,
}

/// The Nth (1-indexed) [`Node::Code`] of `content` in document order. The
/// walk recurses through [`Node::sub_contents`] — every place content can
/// nest — so a block inside a container, table, or module is numbered by
/// its position in the document; code interiors, `[[raw]]` regions and
/// include targets are opaque leaves, and a nested-looking `[[code]]` pairs
/// LIFO exactly as the engine pairs it — the endpoint serves what the
/// renderer shows, never an ad-hoc reading of the source.
pub(super) fn nth_code_block<'a>(content: &'a Content, n: u32) -> Option<CodeNode<'a>> {
    fn walk<'a>(content: &'a Content, n: u32, seen: &mut u32) -> Option<CodeNode<'a>> {
        for node in content {
            if let Node::Code { ty, raw } = node {
                *seen += 1;
                if *seen == n {
                    return Some(CodeNode { ty, raw });
                }
                continue;
            }
            for sub in node.sub_contents() {
                if let Some(found) = walk(sub, n, seen) {
                    return Some(found);
                }
            }
        }
        None
    }
    walk(content, n, &mut 0)
}

/// Strong quoted ETag: SHA-256 of the *decoded* body (stable across
/// encodings, so a zstd-capable and a plain client revalidate identically).
fn etag_of(bytes: &[u8]) -> String {
    use ring::digest::{Context, SHA256};
    let mut cx = Context::new(&SHA256);
    cx.update(bytes);
    format!(
        "\"{}\"",
        cx.finish()
            .as_ref()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    )
}

#[cfg(test)]
mod tests {
    use super::nth_code_block;
    use crate::wikidot_parser::{parse, wikidot_verbatim};

    /// `ty` of block 1 of `src` (the caller owns the parsed content).
    macro_rules! ty1 {
        ($src:expr) => {{
            let c = parse($src);
            nth_code_block(&c, 1).unwrap().ty.clone()
        }};
    }

    #[test]
    fn nth_code_block_extracts_typed_and_plain() {
        let src = "intro\n\
                   [[code]]\nfirst & <raw>\n[[/code]]\n\
                   middle\n\
                   [[code type=\"css\"]]\n.second { color: red }\n[[/code]]\n\
                   [[code type=\"CSS\"]]upper[[/code]]\n";
        let content = parse(src);
        let block = |n| nth_code_block(&content, n).map(|b| (b.ty.clone(), b.raw.clone()));
        assert_eq!(block(1), Some((None, "\nfirst & <raw>\n".into())));
        assert_eq!(
            block(2),
            Some((Some("css".into()), "\n.second { color: red }\n".into()))
        );
        assert_eq!(block(3), Some((Some("CSS".into()), "upper".into())));
        assert!(block(4).is_none());
    }

    #[test]
    fn nth_code_block_counts_document_order_across_nesting() {
        // A block inside a container is numbered by its position in the
        // document, not by depth.
        let src = "[[code]]top[[/code]][[div]]\n[[code type=\"css\"]]\nin\n[[/code]]\n[[/div]]";
        let content = parse(src);
        let b1 = nth_code_block(&content, 1).expect("block 1");
        assert_eq!(b1.raw, "top");
        let b2 = nth_code_block(&content, 2).expect("block 2 inside div");
        assert_eq!(b2.ty.as_deref(), Some("css"));
        assert_eq!(b2.raw, "\nin\n");
        assert!(nth_code_block(&content, 3).is_none());
    }

    #[test]
    fn nth_code_block_interior_and_neighbours_are_opaque() {
        // The engine pairs nested openers LIFO: the inner `[[code]]b[[/code]]`
        // is the only block and the unclosed outer degrades to raw text —
        // `/code/N` serves exactly what the renderer shows (the Nth
        // `Node::Code`), not an ad-hoc reading of the source.
        let content = parse("[[code]]a[[code]]b[[/code]]");
        assert_eq!(nth_code_block(&content, 1).unwrap().raw, "b");
        assert!(nth_code_block(&content, 2).is_none());

        // An unclosed opener is plain text (its `type` never applies), a
        // `[[module css]]` region is a stylesheet, `[[codex]]` is text — only
        // the closed block counts.
        let src = "[[code type=\"css\"\n[[module css]]m[[/module]]\n[[codex]]x[[/codex]]\n[[code]]real[[/code]]";
        let content = parse(src);
        let b1 = nth_code_block(&content, 1).expect("block 1");
        assert_eq!(b1.raw, "real");
        assert!(b1.ty.is_none());
        assert!(nth_code_block(&content, 2).is_none());
    }

    #[test]
    fn nth_code_block_type_attr_forms() {
        assert_eq!(ty1!("[[code type=css]]x[[/code]]").as_deref(), Some("css"));
        assert_eq!(
            ty1!("[[code Type=\"CSS\"]]x[[/code]]").as_deref(),
            Some("CSS")
        );
        assert_eq!(ty1!("[[code subtype=\"css\"]]x[[/code]]").as_deref(), None);
        assert_eq!(ty1!("[[code]]x[[/code]]").as_deref(), None);
        // Single quotes are part of the value — the uniform param semantics
        // of the whole engine (`[[div class='x']]` keeps them too) — so only
        // the double-quoted/bare form marks a stylesheet.
        assert_eq!(
            ty1!("[[code type='css']]x[[/code]]").as_deref(),
            Some("'css'")
        );
    }

    #[test]
    fn wikidot_serve_pipeline() {
        // NBSP → space, `&amp;` → `&amp;amp;` (bare `&` untouched), edges
        // trimmed, one trailing newline — byte-verified against the live
        // endpoint on two corpus pages.
        let content = parse(
            "[[code type=\"css\"]]\n\n\u{a0}a { content: \"A &amp; B\" }\n\u{a0}b&c\n\n[[/code]]",
        );
        let b1 = nth_code_block(&content, 1).unwrap();
        assert_eq!(
            wikidot_verbatim(b1.raw),
            "a { content: \"A &amp;amp; B\" }\n b&c\n"
        );
        let served = super::serve(b1);
        assert!(served.css, "type=\"css\" → CSS MIME");
        assert_eq!(
            served.etag,
            format!("\"{}\"", {
                use ring::digest::{Context, SHA256};
                let mut cx = Context::new(&SHA256);
                cx.update(b"a { content: \"A &amp;amp; B\" }\n b&c\n");
                cx.finish()
                    .as_ref()
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<String>()
            })
        );
    }

    /// Against the real export repo: `component:research-style` — the page
    /// the terrible-trio-event theme `@import`s — must yield exactly one CSS
    /// block, byte-identical to Wikidot's own serving of it (the ETag is the
    /// strong SHA-256 of the served body, taken from this implementation
    /// after the live-endpoint byte verification). Skipped when the real
    /// repo isn't checked out.
    #[test]
    fn real_repo_research_style_block_one_is_css() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../.kolorinko/repo/rpcauthority/_pages_by_id/13/05/795112/r048.txt");
        let Ok(raw) = std::fs::read_to_string(&path) else {
            eprintln!("skipping: real repo not present");
            return;
        };
        // Strip the leading frontmatter block (blank-line-terminated header).
        let body = raw.split_once("\n\n").map(|(_, b)| b).unwrap_or(&raw);
        let content = parse(body);
        let b1 = nth_code_block(&content, 1).expect("block 1");
        assert_eq!(
            b1.ty.as_deref(),
            Some("css"),
            "block 1 must be type=\"css\""
        );
        assert!(b1.raw.trim_start().starts_with("/*"), "CSS comment first");
        assert!(
            !b1.raw.contains("[[code"),
            "interior must end at the closer"
        );
        assert!(nth_code_block(&content, 2).is_none(), "exactly one block");
        // Byte-fidelity through the parse tree: the served body (pipeline
        // applied) is exactly what the live endpoint returns.
        assert_eq!(
            super::etag_of(wikidot_verbatim(b1.raw).as_bytes()),
            "\"34cc20f112337e4ab7f00b3000f2c82d9b275a39ce29b1905cda37cb19d864c6\""
        );
    }
}
