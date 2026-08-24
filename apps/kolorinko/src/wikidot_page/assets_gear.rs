use super::*;

// =========================================================================
// `repo_resource` gear — content-addressed resolution
// =========================================================================

/// No carry-over state: a [`repo_resource`] run is a pure lookup in the
/// followed [`RepoData`] snapshot's `files/` index, re-derived whenever the
/// export tip moves.
#[derive(Default, Clone, Debug)]
pub(crate) struct RepoResourceCache;

/// Resolve one `files/<host>/<path>` attachment to its content-addressed
/// [`CaRef`] by table lookup in [`RepoData`]. The index is built from the
/// `files/` symlinks (pointing into `_files/<xx>/<yy>/<hash>`) at tree-build
/// time; `path` is the percent-decoded `<host>/<path>` tail. `None` when the
/// URL is not mirrored (a hotlink) — the caller then leaves the original URL.
///
/// `.wdfiles.com` is Wikidot's file-serving CDN alias for the canonical
/// `.wikidot.com` host, and a site's configured alias domains are its custom
/// domains: pages reference either, but the mirror indexes attachments under
/// the canonical host. So a tail whose host is an alias is retried there
/// before concluding it is a hotlink.
pub(crate) fn repo_resource(
    data: &RepoData,
    site: &SafePathComponent,
    path: &RepoAssetPath,
) -> Option<CaRef> {
    let files = &data.sites.get(site)?.files;
    files.get(path).cloned().or_else(|| {
        crate::globals::space_of(site)
            .and_then(|space| crate::globals::domains_of(&space))
            .and_then(|domains| repo_alias(site, domains, path))
            .and_then(|alt| files.get(&alt).cloned())
    })
}

/// The canonical-host form of a [`RepoAssetPath`] whose host is an alias of
/// `site`: `<sub>.wdfiles.com` (Wikidot's file CDN) rewrites to
/// `<sub>.wikidot.com`, and any of the site's configured alias domains
/// (case-insensitively — hosts are) rewrites to `<site>.wikidot.com`.
/// `None` when the host is neither — including another site's alias domain.
fn repo_alias(
    site: &SafePathComponent,
    domains: &[String],
    path: &RepoAssetPath,
) -> Option<RepoAssetPath> {
    let s = path.as_str();
    let (host, rest) = s.split_once('/')?;
    let canonical = match host.strip_suffix(".wdfiles.com") {
        Some(sub) => format!("{sub}.wikidot.com"),
        None if domains.iter().any(|d| d.eq_ignore_ascii_case(host)) => {
            format!("{}.wikidot.com", **site)
        }
        None => return None,
    };
    RepoAssetPath::new(format!("{canonical}/{rest}"))
}

/// Serialize a [`CaRef`] to its served URL:
/// `/-/repo/<site>/files/<xx>/<yy>/<hash>.<ext>`, embedding the sha256
/// (xx=key[0:2], yy=key[2:4], leaf=full hash) so the URL is self-describing
/// and collision-free with real `files/<host>/<path>` paths (a 64-hex leaf
/// never occurs naturally). The extension rides along so the server derives
/// the MIME without a side table; an empty extension yields a bare `<hash>`
/// leaf. The on-disk `_files/` layout (sharded rest-leaf) is reconstructed at
/// read time.
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
/// Serve one content-addressed blob `<site>/_files/<xx>/<yy>/<hash>`: read it,
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
    // The blob store lives under the configured repo dir (a process global
    // since the `RepoMeta` gear-field removal).
    let meta = crate::globals::repo();
    // A wire-constructed `GearId::Asset` could carry a malformed hash; only the
    // HTTP path (`serve` → `ca_parts`) pre-validates. Guard before slicing.
    if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    // The exporter shards the blob store `_files/<d1>/<d2>/<rest>` with
    // rest=key[4:] (the 60-char tail), so the leaf is NOT the full hash.
    let file = meta
        .path()
        .join(&**site)
        .join("_files")
        .join(&hash[..2])
        .join(&hash[2..4])
        .join(&hash[4..]);
    let bytes = fs::read(&file).await.ok()?;
    let body = if crate::assets::mime_for_ext(ext) == "text/css" {
        let text = String::from_utf8_lossy(&bytes);
        let refs = http_refs(&text);
        let mut map: HashMap<String, String> = HashMap::new();
        for tail in &refs {
            if let Some(path) = RepoAssetPath::new(percent_decode(tail))
                && let Some(ca) = get_ca(site, path, ctx).await
            {
                map.insert(tail.clone(), ca_url(site, &ca));
            }
        }
        crate::assets::compress(rewrite_with(&text, None, |t| map.get(t).cloned()).into_bytes())
    } else {
        crate::assets::compress(bytes)
    };
    Some(body)
}

/// Resolve one `host/path` tail to its [`CaRef`] via the [`repo_resource`] gear
/// using a **non-tracking** (stale) read — so [`asset`]'s cache key stays the
/// blob identity alone (see the PURE-FUNCTION note above). `None` when the URL
/// is not mirrored (a hotlink).
pub(super) async fn get_ca<S: Storage<KolorinkoRT>>(
    site: &SafePathComponent,
    path: RepoAssetPath,
    ctx: &mut GearCtx<KolorinkoRT, S>,
) -> Option<CaRef> {
    let id = crate::runtime::repo_resource(site.clone(), path)
        .id()
        .clone();
    match ctx.core().read_gear_stale(id).await {
        GearResult::Shared(s) => match &*s {
            GearOutShared::RepoResourceOut(ca) => ca.clone(),
            _ => None,
        },
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_alias_maps_wdfiles_to_wikidot() {
        let site = SafePathComponent::new("rpcauthority".into()).unwrap();
        let p = RepoAssetPath::new("rpcauthority.wdfiles.com/local--files/x/y.png".into()).unwrap();
        assert_eq!(
            repo_alias(&site, &[], &p).map(|a| a.as_str().to_owned()),
            Some("rpcauthority.wikidot.com/local--files/x/y.png".to_owned()),
        );
    }

    #[test]
    fn repo_alias_maps_configured_domains_case_insensitively() {
        let site = SafePathComponent::new("obscurative".into()).unwrap();
        let domains = ["www.obscurative.ru".to_owned()];
        let p = RepoAssetPath::new("WWW.OBSCURATIVE.RU/local--files/x/y.png".into()).unwrap();
        assert_eq!(
            repo_alias(&site, &domains, &p).map(|a| a.as_str().to_owned()),
            Some("obscurative.wikidot.com/local--files/x/y.png".to_owned()),
        );
    }

    #[test]
    fn repo_alias_skips_non_alias_hosts() {
        let site = SafePathComponent::new("rpcauthority".into()).unwrap();
        let domains = ["rpc-wiki.net".to_owned()];
        let canonical =
            RepoAssetPath::new("rpcauthority.wikidot.com/local--files/x/y.png".into()).unwrap();
        let foreign = RepoAssetPath::new("i.imgur.com/x.jpg".into()).unwrap();
        let other_site =
            RepoAssetPath::new("www.obscurative.ru/local--files/x/y.png".into()).unwrap();
        assert_eq!(repo_alias(&site, &domains, &canonical), None);
        assert_eq!(repo_alias(&site, &domains, &foreign), None);
        // A domain of *another* site is not this site's alias.
        assert_eq!(repo_alias(&site, &domains, &other_site), None);
    }
}
