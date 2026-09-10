use super::*;

// =========================================================================
// `asset` gear — content-addressed blob serving
// =========================================================================

/// No carry-over state: a run reads the blob fresh; the compressed bytes are
/// cached by the runtime (`shared`: one allocation, refcounted across cores).
#[derive(Default, Clone, Debug)]
pub(crate) struct AssetCache;

/// **PURE-FUNCTION ASSUMPTION (known to be wrong).** The gear's output — the
/// MIME, and the CSS rewrite, which resolves each `url()`/`@import` (relative
/// ones against the blob's original URL) through a **non-tracking** (stale)
/// read of the snapshot — is treated as a pure function of `(site, hash)`,
/// so a re-mirror that changes a sub-resource's hash does **not** invalidate
/// the cached blob. It keeps pointing at the old (still-resolvable — CA
/// blobs are immutable) sub-resource hash until evicted. Acceptable for
/// now; revisit if stale stylesheets bite.
///
/// Serve one content-addressed blob — the publication's
/// `<site>/files_ca/<xx>/<yy>/<rest>` (rest = hash[4..]): read it, then let
/// the reverse index's entry decide everything — the MIME (and the
/// CSS-rewrite branch) from the recorded `content_type`, the base for
/// relative refs from the original URL — never the extension the request
/// URL happens to carry. `None` when the blob or its index row is absent
/// (the HTTP layer serves a 404). `shared` so the compressed bytes are
/// cached + deduplicated across cores; HTTP-only — never shipped over
/// WebTransport (`to_wire_out` drops it).
pub(crate) async fn asset<S: Storage<KolorinkoRT>>(
    site: &SafePathComponent,
    hash: &str,
    ctx: &mut GearCtx<KolorinkoRT, S>,
) -> Option<ServedBlob> {
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
    // The one stale snapshot read of the run (see the PURE-FUNCTION note).
    let id = crate::runtime::repo_snap().id().clone();
    match ctx.core().read_gear_stale(id).await {
        GearResult::Shared(s) => match &*s {
            GearOutShared::RepoSnapOut(snap) => {
                let entry = snap.sites.get(site)?.files_ca.get(hash)?;
                let mime = crate::assets::mime_for_ext(&crate::assets::ca_file_ext(entry));
                let body = if mime == "text/css" {
                    let text = String::from_utf8_lossy(&bytes);
                    // The blob's original URL is the base its relative
                    // `url()`/`@import` refs resolve against.
                    let base = format!("http://{}", entry.path.as_str());
                    crate::assets::compress(localize_css(&text, &base, site, snap).into_bytes())
                } else {
                    crate::assets::compress(bytes)
                };
                Some(ServedBlob {
                    mime: mime.into_owned(),
                    body,
                })
            }
            _ => None,
        },
        _ => None,
    }
}

/// The one CSS localization pass a served blob goes through: every
/// `url()`/`@import` — an absolute ref, or a relative one resolved against
/// the blob's original URL `base` — goes through [`resolve_tails`], the same
/// substitution decision the page pipeline applies.
pub(crate) fn localize_css(
    text: &str,
    base: &str,
    site: &SafePathComponent,
    snap: &RepoSnapshot,
) -> String {
    let resolved = resolve_tails(&http_refs(text, Some(base)), |path| {
        resource_url(snap, site, path)
    });
    rewrite_with(text, Some(base), |t| resolved.get(t).cloned())
}

/// Serialize one mirrored file to its served URL:
/// `/-/repo/<site>/files/<xx>/<yy>/<hash>.<ext>`, embedding the sha256
/// (`xx=key[0:2]`, `yy=key[2:4]`, leaf=full hash) so the URL is self-describing
/// and collision-free with real `files/<host>/<path>` paths (a 64-hex leaf
/// never occurs naturally). The extension derives from the file's recorded
/// type ([`crate::assets::ca_file_ext`]) and rides along for convention —
/// the serving path keys on the hash alone and re-derives the MIME from the
/// index, so the URL's own spelling never decides what the blob is. An empty
/// extension yields a bare `<hash>` leaf.
pub(crate) fn ca_url(site: &SafePathComponent, hash: &str, file: &CaFile) -> String {
    let ext = crate::assets::ca_file_ext(file);
    let ext = if ext.is_empty() {
        String::new()
    } else {
        format!(".{ext}")
    };
    format!(
        "/-/repo/{}/files/{}/{}/{}{}",
        &**site,
        &hash[..2],
        &hash[2..4],
        hash,
        ext
    )
}
