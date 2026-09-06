use super::*;

// =========================================================================
// `asset` gear — content-addressed blob serving
// =========================================================================

/// No carry-over state: a run reads the blob fresh; the compressed bytes are
/// cached by the runtime (`shared`: one allocation, refcounted across cores).
#[derive(Default, Clone, Debug)]
pub(crate) struct AssetCache;

/// **PURE-FUNCTION ASSUMPTION (known to be wrong).** The CSS rewrite here
/// resolves each `url()`/`@import` to its content-addressed form via
/// [`get_ca`], but with a **non-tracking** (stale) read of the index. The
/// gear's cache key is therefore just `(site, hash, ext)` — the rewrite is
/// treated as a pure function of the blob, so a re-mirror that changes a
/// sub-resource's hash does **not** invalidate the cached stylesheet. The
/// cached CSS keeps pointing at the old (still-resolvable — CA blobs are
/// immutable) sub-resource hash until evicted. Acceptable for now; revisit if
/// stale stylesheets bite.
///
/// Serve one content-addressed blob — the publication's
/// `<site>/files_ca/<xx>/<yy>/<rest>` (rest = hash[4..]): read it,
/// and for CSS rewrite every mirrored `url()`/`@import` to its CA URL, then
/// compress. `None` when the blob is absent (the HTTP layer serves a 404).
/// `shared` so the compressed bytes are cached + deduplicated across cores;
/// HTTP-only — never shipped over WebTransport (`to_wire_out` drops it).
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
        let mut map: HashMap<String, String> = HashMap::new();
        for tail in &refs {
            if let Some(path) = canon_file_key(tail)
                && let Some(ca) = get_ca(site, path, ctx).await
            {
                map.insert(tail.clone(), ca_url(site, &ca));
            } else if let Some(url) = code_url_for_tail(tail) {
                // Not mirrored: Wikidot's `/code/N` endpoint still serves
                // locally — the same fallback [`resolve_resources`] applies
                // to page-embedded stylesheets.
                map.insert(tail.clone(), url);
            }
        }
        crate::assets::compress(rewrite_with(&text, None, |t| map.get(t).cloned()).into_bytes())
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

/// Resolve one `host/path` tail to its [`CaRef`] via the [`repo_snap`]
/// snapshot using a **non-tracking** (stale) read — so [`asset`]'s cache key
/// stays the blob identity alone (see the PURE-FUNCTION note above). `None`
/// when the URL is not mirrored (a hotlink).
///
/// [`repo_snap`]: crate::runtime::repo_snap
/// [`asset`]: crate::wikidot_page::asset
pub(super) async fn get_ca<S: Storage<KolorinkoRT>>(
    site: &SafePathComponent,
    path: RepoAssetPath,
    ctx: &mut GearCtx<KolorinkoRT, S>,
) -> Option<CaRef> {
    let id = crate::runtime::repo_snap().id().clone();
    match ctx.core().read_gear_stale(id).await {
        GearResult::Shared(s) => match &*s {
            GearOutShared::RepoSnapOut(snap) => resource(snap, site, &path),
            _ => None,
        },
        _ => None,
    }
}
