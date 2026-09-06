use super::*;

// =========================================================================
// `asset` gear — content-addressed blob serving
// =========================================================================

/// No carry-over state: a run reads the blob fresh; the compressed bytes are
/// cached by the runtime (`shared`: one allocation, refcounted across cores).
#[derive(Default, Clone, Debug)]
pub(crate) struct AssetCache;

/// **PURE-FUNCTION ASSUMPTION (known to be wrong).** The CSS rewrite here
/// resolves each `url()`/`@import` to its content-addressed form against a
/// **non-tracking** (stale) read of the snapshot. The gear's cache key is
/// therefore just `(site, hash, ext)` — the rewrite is treated as a pure
/// function of the blob, so a re-mirror that changes a sub-resource's hash
/// does **not** invalidate the cached stylesheet. The cached CSS keeps
/// pointing at the old (still-resolvable — CA blobs are immutable)
/// sub-resource hash until evicted. Acceptable for now; revisit if stale
/// stylesheets bite.
///
/// Serve one content-addressed blob — the publication's
/// `<site>/files_ca/<xx>/<yy>/<rest>` (rest = hash[4..]): read it,
/// and for CSS rewrite every mirrored `url()`/`@import` to its CA URL —
/// through [`resolve_tails`], the same substitution decision the page
/// pipeline applies — then compress. `None` when the blob is absent (the
/// HTTP layer serves a 404). `shared` so the compressed bytes are cached +
/// deduplicated across cores; HTTP-only — never shipped over WebTransport
/// (`to_wire_out` drops it).
pub(crate) async fn asset<S: Storage<KolorinkoRT>>(
    site: &SafePathComponent,
    hash: &str,
    ext: &str,
    ctx: &mut GearCtx<KolorinkoRT, S>,
) -> Option<Body> {
    // The publication root lives under the configured evakuilo directory (a
    // process global since the gear-field removal).
    let meta = crate::globals::repo();
    // A wire-constructed `GearId::Asset` could carry a malformed hash; only the
    // HTTP path (`serve` → `ca_parts`) pre-validates. Guard before slicing.
    if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    // The publisher shards the blob store `files_ca/<d1>/<d2>/<rest>` with
    // rest=key[4:] (the 60-char tail), so the leaf is NOT the full hash.
    let file = meta
        .site_dir(site)
        .join("files_ca")
        .join(&hash[..2])
        .join(&hash[2..4])
        .join(&hash[4..]);
    let bytes = fs::read(&file).await.ok()?;
    let body = if crate::assets::mime_for_ext(ext) == "text/css" {
        let text = String::from_utf8_lossy(&bytes);
        let refs = http_refs(&text);
        // The one stale snapshot read of the run (see the PURE-FUNCTION note).
        let id = crate::runtime::repo_snap().id().clone();
        let resolved = match ctx.core().read_gear_stale(id).await {
            GearResult::Shared(s) => match &*s {
                GearOutShared::RepoSnapOut(snap) => {
                    resolve_tails(site, &refs, |path| resource(snap, site, &path))
                }
                _ => HashMap::new(),
            },
            _ => HashMap::new(),
        };
        crate::assets::compress(
            rewrite_with(&text, None, |t| resolved.get(t).cloned()).into_bytes(),
        )
    } else {
        crate::assets::compress(bytes)
    };
    Some(body)
}

/// Serialize a [`CaRef`] to its served URL:
/// `/-/repo/<site>/files/<xx>/<yy>/<hash>.<ext>`, embedding the sha256
/// (`xx=key[0:2]`, `yy=key[2:4]`, leaf=full hash) so the URL is self-describing
/// and collision-free with real `files/<host>/<path>` paths (a 64-hex leaf
/// never occurs naturally). The extension rides along so the server derives
/// the MIME without a side table; an empty extension yields a bare `<hash>`
/// leaf. The on-disk `files_ca/` layout (sharded rest-leaf) is reconstructed
/// at read time.
pub(crate) fn ca_url(site: &SafePathComponent, ca: &CaRef) -> String {
    let site = &**site;
    let h = &ca.hash;
    let ext = if ca.ext.is_empty() {
        String::new()
    } else {
        format!(".{}", ca.ext)
    };
    format!("/-/repo/{site}/files/{}/{}/{}{}", &h[..2], &h[2..4], h, ext)
}
