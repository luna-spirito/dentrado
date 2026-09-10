//! Serve content-addressed site assets out of the evakuilo publication.
//!
//! One shape, one gear (both `shared` — cached + deduplicated across cores —
//! and HTTP-only: never shipped over WebTransport):
//! - **Content-addressed** `/-/repo/<site>/files/<xx>/<yy>/<hash>[.<ext>]` —
//!   the [`crate::wikidot_page::asset`] gear reads the `files_ca/…/<hash>` blob, answers with the
//!   MIME the reverse index's recorded type decides (the URL's extension is
//!   decorative), rewrites CSS `url()`/`@import` — relative refs included,
//!   against the blob's original URL — to CA URLs, and compresses. Immutable
//!   key, so the client caches it forever.
//!
//! Everything under `/-/` is system namespace (these mirrored blobs, future
//! platform endpoints): blob-or-404, never content routing, never the SPA
//! fallback.
//!
//! Every resource a page or stylesheet references is resolved to a CA URL at
//! render time (mirrored) or left as its original absolute URL (a hotlink the
//! browser fetches straight from the origin), so there is no path-based form
//! to serve here.

use std::rc::Rc;

use dentrado::core::{core_ctx::Core, storage::InMemoryStorage};
use kolorinko_rt::{Body, RepoAssetPath, SafePathComponent};

use crate::runtime::{KolorinkoRT, asset};

const PREFIX: &str = "/-/repo/";

/// Result of a repo-asset request.
pub(crate) enum RepoResp {
    Ok { mime: String, body: Body },
}

/// The validated pieces of a `/-/repo/<site>/files/<xx>/<yy>/<hash>[.<ext>]`
/// request: `(site, hash)`, or `None` for anything outside the `/-/repo/`
/// namespace, not under `files/`, with an unsafe path, and not the CA shape.
/// The URL's extension is decorative — kept for conventional tooling, never
/// consulted — so it is checked for shape only, not returned. Pure (no disk,
/// no core) so the SPA-fallback and traversal guards are testable without a
/// runtime.
pub(crate) fn parse_ca_request(full: &str) -> Option<(SafePathComponent, String)> {
    let rest = full.strip_prefix(PREFIX)?;
    let mut segs = rest.split('/');
    let site = SafePathComponent::new(segs.next()?.to_string())?;
    if segs.next()? != "files" {
        return None;
    }
    let tail = segs.collect::<Vec<_>>().join("/");
    let (disk_rel, _query) = tail.split_once('?').unwrap_or((&tail, ""));
    let path = RepoAssetPath::new(disk_rel.to_string())?;
    Some((site, ca_parts(&path)?))
}

/// Resolve one CA request via the [`crate::wikidot_page::asset`] gear, or
/// `None` for anything outside the `/-/repo/` namespace or a missing blob
/// (404). `full` is the raw request path (with query, if any).
pub(crate) async fn serve(
    full: &str,
    core: &Rc<Core<KolorinkoRT, InMemoryStorage<KolorinkoRT>>>,
) -> Option<RepoResp> {
    let (site, hash) = parse_ca_request(full)?;
    let blob = asset(site, hash).subscribe(core).await.current();
    (*blob).as_ref().map(|blob| RepoResp::Ok {
        mime: blob.mime.clone(),
        body: blob.body.clone(),
    })
}

/// The hash of a CA request path `<xx>/<yy>/<hash>[.<ext>]`, or `None` if it
/// isn't the content-addressed shape (two 2-hex dir shards + a 64-hex hash
/// leaf; everything after the hash's first `.` is the decorative extension —
/// a plain `png`, or a flattened type like `text.css` which itself carries
/// dots).
fn ca_parts(path: &RepoAssetPath) -> Option<String> {
    let mut segs = path.as_str().split('/');
    let (xx, yy, leaf) = (segs.next()?, segs.next()?, segs.next()?);
    if segs.next().is_some() || xx.len() != 2 || yy.len() != 2 {
        return None;
    }
    if !xx.bytes().all(|b| b.is_ascii_hexdigit()) || !yy.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let hash = leaf.split_once('.').map_or(leaf, |(h, _)| h);
    if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    Some(hash.to_string())
}

#[cfg(test)]
mod tests {
    use super::{ca_parts, parse_ca_request};
    use kolorinko_rt::RepoAssetPath;

    #[test]
    fn non_ca_requests_are_rejected() {
        assert!(parse_ca_request("/-/repo/rpcauthority/theme/../etc/passwd").is_none());
        assert!(parse_ca_request("/-/repo/rpcauthority//files/x").is_none());
        assert!(parse_ca_request("/-/repo/rpcauthority/files/../secret").is_none());
        assert!(parse_ca_request("/-/repo/rpcauthority/bogus/x").is_none()); // not `files`
        assert!(parse_ca_request("/-/repo/rpcauthority/files/d8/4a/deadbeef.png").is_none()); // short hash
        assert!(parse_ca_request("/-/notrepo/x").is_none()); // outside namespace
        assert!(parse_ca_request("/repo/rpcauthority/files/d8/4a/x").is_none()); // root-relative asset, not ours
    }

    #[test]
    fn parses_ca_request() {
        let h = "d84a29109fe0e70c7a5c22c39bda120fdbc56bd192f5927af95b9af8d0f87c27";
        let (site, hash) = parse_ca_request(&format!("/-/repo/rpcauthority/files/d8/4a/{h}.jpg"))
            .expect("CA request");
        assert_eq!((*site).clone(), "rpcauthority");
        assert_eq!(hash, h);
        // A flattened-type extension carries its own dots — everything past
        // the hash's first `.` is the (decorative, ignored) extension.
        let (_site, hash) =
            parse_ca_request(&format!("/-/repo/rpcauthority/files/d8/4a/{h}.text.css"))
                .expect("CA request");
        assert_eq!(hash, h);
    }

    #[test]
    fn ca_parts_detects_blob_path() {
        let p = |s: &str| RepoAssetPath::new(s.into()).unwrap();
        let h = "d84a29109fe0e70c7a5c22c39bda120fdbc56bd192f5927af95b9af8d0f87c27";
        assert_eq!(ca_parts(&p(&format!("d8/4a/{h}.jpg"))), Some(h.into()));
        // Bare hash, no extension.
        assert_eq!(ca_parts(&p(&format!("d8/4a/{h}"))), Some(h.into()));
        // Wrong shard widths / non-hex.
        assert!(ca_parts(&p(&format!("d8/4/{h}.png"))).is_none());
        assert!(ca_parts(&p(&format!("zz/4a/{h}.png"))).is_none());
        // Hash too short.
        assert!(ca_parts(&p("d8/4a/deadbeef.png")).is_none());
    }
}
