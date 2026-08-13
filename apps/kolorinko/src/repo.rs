//! Serve mirrored site assets out of the export repository.
//!
//! Two shapes, two gears (both `shared` — cached + deduplicated across cores
//! — and HTTP-only: never shipped over WebTransport):
//! - **Content-addressed** `/repo/<site>/files/<xx>/<yy>/<hash>.<ext>` — the
//!   [`Asset`] gear reads the `_files/…/<hash>` blob, rewrites CSS
//!   `url()`/`@import` to CA URLs, and compresses. Immutable key, so the client
//!   caches it forever.
//! - **Path-based** `/repo/<site>/theme/<host>/<path>` — the [`RepoAsset`] gear
//!   reads the `theme/` symlink, rewrites CSS to local paths, and compresses; a
//!   missing file redirects back onto the original host.
//!
//! [`Asset`]: kolorinko_rt gear
//! [`RepoAsset`]: kolorinko_rt gear

use std::rc::Rc;

use dentrado::core::{core_ctx::Core, gear::GearResult, storage::InMemoryStorage};
use kolorinko_rt::{AssetKind, Body, RepoAssetOut, RepoAssetPath, SafePathComponent};

use crate::assets::{mime_for, mime_for_ext};
use crate::runtime::{GearOutShared, KolorinkoRT, asset, repo_asset};
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
    // Content-addressed blob: `/repo/<site>/files/<xx>/<yy>/<hash>.<ext>`.
    // Served via the [`Asset`] gear (cached, deduplicated across cores); the
    // gear rewrites CSS `url()`/`@import` to CA URLs and compresses. A missing
    // blob yields `None` (404).
    //
    // [`Asset`]: kolorinko_rt gear
    if kind == AssetKind::Files
        && let Some((_xx, _yy, hash, ext)) = ca_parts(&path)
    {
        let mime = mime_for_ext(&ext);
        let q = asset(repo_meta.clone(), site.clone(), hash, ext);
        let GearResult::Shared(s) = core.read_gear(q.id).await else {
            return None;
        };
        return match &*s {
            GearOutShared::AssetOut(Some(body)) => Some(RepoResp::Ok {
                mime,
                body: body.clone(),
            }),
            _ => None,
        };
    }
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

/// Split a CA request path `<xx>/<yy>/<hash>.<ext>` into its shards, or `None`
/// if it isn't the content-addressed shape (two 2-hex dir shards + a 64-hex
/// hash leaf, with an optional extension). The shape can't collide with a real
/// mirrored `files/<host>/<path…>` attachment (whose first segment is a host
/// name, not 2 hex chars).
fn ca_parts(path: &RepoAssetPath) -> Option<(String, String, String, String)> {
    let mut segs = path.as_str().split('/');
    let (xx, yy, leaf) = (segs.next()?, segs.next()?, segs.next()?);
    if segs.next().is_some() || xx.len() != 2 || yy.len() != 2 {
        return None;
    }
    if !xx.bytes().all(|b| b.is_ascii_hexdigit()) || !yy.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let (hash, ext) = match leaf.rsplit_once('.') {
        Some((h, e)) => (h, e),
        None => (leaf, ""),
    };
    if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    Some((
        xx.to_string(),
        yy.to_string(),
        hash.to_string(),
        ext.to_string(),
    ))
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
    fn ca_parts_detects_blob_path() {
        let p = |s: &str| RepoAssetPath::new(s.into()).unwrap();
        let h = "d84a29109fe0e70c7a5c22c39bda120fdbc56bd192f5927af95b9af8d0f87c27";
        assert_eq!(
            super::ca_parts(&p(&format!("d8/4a/{h}.jpg"))),
            Some(("d8".into(), "4a".into(), h.into(), "jpg".into()))
        );
        // Bare hash, no extension.
        assert_eq!(
            super::ca_parts(&p(&format!("d8/4a/{h}"))),
            Some(("d8".into(), "4a".into(), h.into(), "".into()))
        );
        // Not the CA shape: a real mirrored attachment path (host/path…).
        assert!(super::ca_parts(&p("rpcauthority.wikidot.com/local--files/slug/a.png")).is_none());
        // Wrong shard widths / non-hex.
        assert!(super::ca_parts(&p(&format!("d8/4/{h}.png"))).is_none());
        assert!(super::ca_parts(&p(&format!("zz/4a/{h}.png"))).is_none());
        // Hash too short.
        assert!(super::ca_parts(&p("d8/4a/deadbeef.png")).is_none());
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
}
