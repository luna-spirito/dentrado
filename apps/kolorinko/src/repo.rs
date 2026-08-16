//! Serve content-addressed site assets out of the export repository.
//!
//! One shape, one gear (both `shared` — cached + deduplicated across cores —
//! and HTTP-only: never shipped over WebTransport):
//! - **Content-addressed** `/repo/<site>/files/<xx>/<yy>/<hash>.<ext>` — the
//!   [`Asset`] gear reads the `_files/…/<hash>` blob, rewrites CSS
//!   `url()`/`@import` to CA URLs, and compresses. Immutable key, so the client
//!   caches it forever.
//!
//! Every resource a page or stylesheet references is resolved to a CA URL at
//! render time (mirrored) or left as its original absolute URL (a hotlink the
//! browser fetches straight from the origin), so there is no path-based form
//! to serve here.
//!
//! [`Asset`]: kolorinko_rt gear

use std::rc::Rc;

use dentrado::core::{core_ctx::Core, gear::GearResult, storage::InMemoryStorage};
use kolorinko_rt::{Body, RepoAssetPath, SafePathComponent};

use crate::assets::mime_for_ext;
use crate::runtime::{GearOutShared, KolorinkoRT, asset};
use crate::wikidot_page::RepoMeta;

const PREFIX: &str = "/repo/";

/// Result of a repo-asset request.
pub(crate) enum RepoResp {
    Ok { mime: &'static str, body: Body },
}

/// The validated pieces of a `/repo/<site>/files/<xx>/<yy>/<hash>[.<ext>]`
/// request: `(site, hash, ext)`, or `None` for anything outside the `/repo/`
/// namespace, not under `files/`, with an unsafe path, or not the CA shape.
/// Pure (no disk, no core) so the SPA-fallback and traversal guards are
/// testable without a runtime.
pub(crate) fn parse_ca_request(full: &str) -> Option<(SafePathComponent, String, String)> {
    let rest = full.strip_prefix(PREFIX)?;
    let mut segs = rest.split('/');
    let site = SafePathComponent::new(segs.next()?.to_string())?;
    if segs.next()? != "files" {
        return None;
    }
    let tail = segs.collect::<Vec<_>>().join("/");
    let (disk_rel, _query) = tail.split_once('?').unwrap_or((&tail, ""));
    let path = RepoAssetPath::new(disk_rel.to_string())?;
    let (_xx, _yy, hash, ext) = ca_parts(&path)?;
    Some((site, hash, ext))
}

/// Resolve one CA request via the [`Asset`] gear, or `None` for anything
/// outside the `/repo/` namespace or a missing blob (404). `full` is the raw
/// request path (with query, if any).
///
/// [`Asset`]: kolorinko_rt gear
pub(crate) async fn serve(
    full: &str,
    repo_meta: RepoMeta,
    core: &Rc<Core<KolorinkoRT, InMemoryStorage<KolorinkoRT>>>,
) -> Option<RepoResp> {
    let (site, hash, ext) = parse_ca_request(full)?;
    let mime = mime_for_ext(&ext);
    let q = asset(repo_meta, site, hash, ext);
    let GearResult::Shared(s) = core.read_gear(q.id).await else {
        return None;
    };
    match &*s {
        GearOutShared::AssetOut(Some(body)) => Some(RepoResp::Ok {
            mime,
            body: body.clone(),
        }),
        _ => None,
    }
}

/// Split a CA request path `<xx>/<yy>/<hash>.<ext>` into its shards, or `None`
/// if it isn't the content-addressed shape (two 2-hex dir shards + a 64-hex
/// hash leaf, with an optional extension).
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
    use super::{ca_parts, parse_ca_request};
    use kolorinko_rt::{RepoAssetPath, SafePathComponent};

    #[test]
    fn non_ca_requests_are_rejected() {
        assert!(parse_ca_request("/repo/rpcauthority/theme/../etc/passwd").is_none());
        assert!(parse_ca_request("/repo/rpcauthority//files/x").is_none());
        assert!(parse_ca_request("/repo/rpcauthority/files/../secret").is_none());
        assert!(parse_ca_request("/repo/rpcauthority/bogus/x").is_none()); // not `files`
        assert!(parse_ca_request("/repo/rpcauthority/files/d8/4a/deadbeef.png").is_none()); // short hash
        assert!(parse_ca_request("/notrepo/x").is_none()); // outside namespace
    }

    #[test]
    fn parses_ca_request() {
        let h = "d84a29109fe0e70c7a5c22c39bda120fdbc56bd192f5927af95b9af8d0f87c27";
        let (site, hash, ext) =
            parse_ca_request(&format!("/repo/rpcauthority/files/d8/4a/{h}.jpg"))
                .expect("CA request");
        assert_eq!((*site).clone(), "rpcauthority");
        assert_eq!(hash, h);
        assert_eq!(ext, "jpg");
    }

    #[test]
    fn ca_parts_detects_blob_path() {
        let p = |s: &str| RepoAssetPath::new(s.into()).unwrap();
        let h = "d84a29109fe0e70c7a5c22c39bda120fdbc56bd192f5927af95b9af8d0f87c27";
        assert_eq!(
            ca_parts(&p(&format!("d8/4a/{h}.jpg"))),
            Some(("d8".into(), "4a".into(), h.into(), "jpg".into()))
        );
        // Bare hash, no extension.
        assert_eq!(
            ca_parts(&p(&format!("d8/4a/{h}"))),
            Some(("d8".into(), "4a".into(), h.into(), "".into()))
        );
        // Wrong shard widths / non-hex.
        assert!(ca_parts(&p(&format!("d8/4/{h}.png"))).is_none());
        assert!(ca_parts(&p(&format!("zz/4a/{h}.png"))).is_none());
        // Hash too short.
        assert!(ca_parts(&p("d8/4a/deadbeef.png")).is_none());
    }
}
