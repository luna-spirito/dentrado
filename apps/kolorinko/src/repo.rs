//! Serve mirrored site assets (`theme/` and `files/` trees) out of the export
//! repository via the [`RepoAsset`] gear.
//!
//! The repo trees are keyed by the origin URL: `<site>/theme/<host>/<path>` and
//! `<site>/files/<host>/<path>` (content stored as symlinks into the `_files`
//! blob store, which `compio::fs` follows transparently). A request
//! `/repo/<site>/<kind>/<host>/<path>` maps straight onto that layout. The
//! [`RepoAsset`] gear reads + (for CSS) rewrites + zstd-compresses the file and
//! caches the bytes shared across cores; a missing file falls back to a
//! redirect back onto the original host.
//!
//! [`RepoAsset`]: kolorinko_rt gear

use std::rc::Rc;

use dentrado::core::{core_ctx::Core, gear::GearResult, storage::InMemoryStorage};
use kolorinko_rt::{AssetKind, Body, RepoAssetOut, RepoAssetPath, SafePathComponent};

use crate::assets::mime_for;
use crate::runtime::{GearOutShared, KolorinkoRT, repo_asset};
use crate::wikidot_page::RepoMeta;

const PREFIX: &str = "/repo/";

/// Result of a repo-asset request.
pub(crate) enum RepoResp {
    Ok { mime: &'static str, body: Body },
    Redirect { location: String },
}

/// The validated pieces of a `/repo/<site>/<kind>/<host>/<path…>` request, or
/// `None` for anything outside the `/repo/` namespace or with an unsafe path.
/// Pure (no disk, no core) so the SPA-fallback and traversal guards are testable
/// without a runtime.
pub(crate) struct ParsedRepoReq {
    site: SafePathComponent,
    kind: AssetKind,
    path: RepoAssetPath,
    query: String,
}

pub(crate) fn parse_repo_request(full: &str) -> Option<ParsedRepoReq> {
    let rest = full.strip_prefix(PREFIX)?;
    let mut segs = rest.split('/');
    let site = segs.next()?;
    let kind = AssetKind::parse(segs.next()?)?;
    let site = SafePathComponent::new(site.to_string())?;
    let url_path = segs.collect::<Vec<_>>().join("/");
    let (disk_rel, query) = url_path.split_once('?').unwrap_or((&url_path, ""));
    let path = RepoAssetPath::new(disk_rel.to_string())?;
    Some(ParsedRepoReq {
        site,
        kind,
        path,
        query: query.to_string(),
    })
}

/// Resolve one repo-asset request via the [`RepoAsset`] gear, or `None` for
/// anything outside the `/repo/` namespace. `full` is the raw request path
/// (with query, if any).
///
/// [`RepoAsset`]: kolorinko_rt gear
pub(crate) async fn serve(
    full: &str,
    repo_meta: RepoMeta,
    core: &Rc<Core<KolorinkoRT, InMemoryStorage<KolorinkoRT>>>,
) -> Option<RepoResp> {
    let ParsedRepoReq {
        site,
        kind,
        path,
        query,
    } = parse_repo_request(full)?;
    let disk_rel = path.as_str().to_owned();
    let q = repo_asset(repo_meta, site, kind, path);
    let res = core.read_gear(q.id).await;
    let GearResult::Shared(s) = res else {
        return None;
    };
    match &*s {
        GearOutShared::RepoAssetOut(RepoAssetOut::Ok(body)) => Some(RepoResp::Ok {
            mime: mime_for(&disk_rel),
            body: body.clone(),
        }),
        GearOutShared::RepoAssetOut(RepoAssetOut::Redirect { location }) => {
            // Reattach the request's query (a cache-buster on the original host)
            // since the gear only sees the validated, query-stripped path.
            let location = if query.is_empty() {
                location.clone()
            } else {
                format!("{location}?{query}")
            };
            Some(RepoResp::Redirect { location })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::parse_repo_request;
    use crate::wikidot_page::{RepoMeta, repo_asset};
    use compio::runtime::Runtime;
    use kolorinko_rt::{AssetKind, RepoAssetPath, SafePathComponent};
    use std::path::Path;

    fn repo_root() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.kolorinko/repo")
    }

    fn meta(root: &Path) -> RepoMeta {
        let root: &'static Path = Box::leak(root.to_path_buf().into_boxed_path());
        RepoMeta::new("unused", root, 900)
    }

    #[test]
    fn traversal_is_rejected() {
        assert!(parse_repo_request("/repo/rpcauthority/theme/../etc/passwd").is_none());
        assert!(parse_repo_request("/repo/rpcauthority//theme/x").is_none());
        assert!(parse_repo_request("/repo/rpcauthority/files/../secret").is_none());
        assert!(parse_repo_request("/repo/rpcauthority/bogus/x").is_none()); // bad kind
        assert!(parse_repo_request("/notrepo/x").is_none()); // outside namespace
    }

    #[test]
    fn missing_asset_redirects_to_original() {
        let root = repo_root();
        if !root.exists() {
            return;
        }
        let site = SafePathComponent::new("rpcauthority".into()).unwrap();
        let path = RepoAssetPath::new("not/mirrored.png".into()).unwrap();
        let out = Runtime::new().unwrap().block_on(repo_asset(
            &meta(&root),
            &site,
            AssetKind::Files,
            &path,
        ));
        let kolorinko_rt::RepoAssetOut::Redirect { location } = out else {
            panic!("expected redirect, got {out:?}");
        };
        assert_eq!(location, "https://not/mirrored.png");
    }

    #[test]
    fn real_theme_css_is_served_and_rewritten() {
        let root = repo_root();
        let path = root.join("rpcauthority/theme");
        if !path.exists() {
            return;
        }
        let site = SafePathComponent::new("rpcauthority".into()).unwrap();
        let asset_path = RepoAssetPath::new(
            "cdn.jsdelivr.net/gh/DoubleDenial/rpc-black-supremacy@refs/heads/main/style.css".into(),
        )
        .unwrap();
        let out = Runtime::new().unwrap().block_on(repo_asset(
            &meta(&root),
            &site,
            AssetKind::Theme,
            &asset_path,
        ));
        let kolorinko_rt::RepoAssetOut::Ok(body) = out else {
            panic!("expected ok, got {out:?}");
        };
        let bytes = match body {
            kolorinko_rt::Body::Raw(b) => b.to_vec(),
            kolorinko_rt::Body::Zstd(b) => zstd::decode_all(&b[..]).unwrap(),
        };
        let css = String::from_utf8(bytes).unwrap();
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
