//! Serve mirrored site assets (`theme/` and `files/` trees) out of the export
//! repository over the existing static-serving loops.
//!
//! The repo trees are keyed by the origin URL: `<site>/theme/<host>/<path>` and
//! `<site>/files/<host>/<path>` (content stored as symlinks into the `_files`
//! blob store, which `compio::fs` follows transparently). A request
//! `/repo/<site>/<kind>/<host>/<path>` maps straight onto that layout, so no
//! URL decode is applied (percent-encoding is preserved exactly as in the URL,
//! e.g. `nav%3Aside`). Stylesheets additionally run through
//! [`crate::css::rewrite`] so their `@import`/`url()` references become local
//! too. Missing files fall back to a redirect back onto the original host.

use std::path::Path;

use compio::fs;
use kolorinko_render::rewrite;
use kolorinko_rt::SafePathComponent;

use crate::assets::mime_for;

const PREFIX: &str = "/repo/";

/// Result of a repo-asset request.
pub(crate) enum RepoResp {
    Ok { mime: &'static str, body: Vec<u8> },
    Redirect { location: String },
}

/// Resolve one repo-asset request, or `None` for anything outside the
/// `/repo/` namespace. `path` is the raw request path (with query, if any).
pub(crate) async fn serve(path: &str, repo_root: &Path) -> Option<RepoResp> {
    let rest = path.strip_prefix(PREFIX)?;
    let mut segs = rest.split('/');
    let site = segs.next()?;
    let kind = segs.next()?;
    if kind != "theme" && kind != "files" {
        return None;
    }
    if SafePathComponent::new(site.to_string()).is_none() {
        return None;
    }
    let url_path = segs.collect::<Vec<_>>().join("/");
    let (disk_rel, _query) = url_path.split_once('?').unwrap_or((&url_path, ""));
    // Guard the on-disk path: no traversal (`..`), no absolute/empty segments.
    if disk_rel.is_empty()
        || disk_rel
            .split('/')
            .any(|s| s.is_empty() || s == "." || s == "..")
    {
        return None;
    }
    let file = repo_root.join(site).join(kind).join(disk_rel);
    match fs::read(&file).await {
        Ok(bytes) => {
            let mime = mime_for(disk_rel);
            let body = if mime == "text/css" {
                let base = format!("https://{disk_rel}");
                // `to_string_lossy`: binary `.css` blobs are replaced char-for-char.
                let text = String::from_utf8_lossy(&bytes);
                rewrite(&text, Some(&base), site, kind).into_bytes()
            } else {
                bytes
            };
            Some(RepoResp::Ok { mime, body })
        }
        Err(_) => Some(RepoResp::Redirect {
            location: format!("https://{url_path}"),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::serve;
    use std::path::Path;

    use compio::runtime::Runtime;

    fn repo_root() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.kolorinko/repo")
    }

    fn resolve(path: &str, root: &Path) -> Option<super::RepoResp> {
        Runtime::new().unwrap().block_on(serve(path, root))
    }

    #[test]
    fn missing_asset_redirects_to_original() {
        let root = repo_root();
        if !root.exists() {
            return;
        }
        let Some(super::RepoResp::Redirect { location }) =
            resolve("/repo/rpcauthority/files/not/mirrored.png", &root)
        else {
            panic!("expected redirect");
        };
        assert_eq!(location, "https://not/mirrored.png");
    }

    #[test]
    fn traversal_is_rejected() {
        let root = repo_root();
        assert!(resolve("/repo/rpcauthority/theme/../etc/passwd", &root).is_none());
        assert!(resolve("/repo/rpcauthority//theme/x", &root).is_none());
    }

    #[test]
    fn real_theme_css_is_served_and_rewritten() {
        let root = repo_root();
        let path = root.join("rpcauthority/theme");
        if !path.exists() {
            return;
        }
        let Some(super::RepoResp::Ok { mime, body }) = resolve(
            "/repo/rpcauthority/theme/cdn.jsdelivr.net/gh/DoubleDenial/rpc-black-supremacy@refs/heads/main/style.css",
            &root,
        ) else {
            panic!("expected served theme css");
        };
        assert_eq!(mime, "text/css");
        let css = String::from_utf8(body).unwrap();
        assert!(
            !css.contains("@import url('https://"),
            "refs must be localized"
        );
        assert!(
            css.contains("/repo/rpcauthority/theme/"),
            "refs must be local"
        );
    }
}
