use super::*;
use std::io::Read;
use tar::Archive;

// =========================================================================
// out/ publication readers (the evakuilo layout)
// =========================================================================

/// One `pages.json` row — the fields the incremental diff keys on. Row
/// equality implies archive equality: the publisher only rewrites an archive
/// when its stored-revision count drifts, and only rewrites the manifest
/// when a row does, so an unchanged row names a byte-identical archive
/// (deterministic tar+zst, write-if-changed).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct PageRow {
    pub slug: String,
    pub title: String,
    pub tags: Vec<String>,
    /// Revisions with stored content (pages with none have no archive).
    pub stored: i64,
    pub max_rev: i64,
    /// Site-relative archive path (`pages_by_id/ab/cd/<rest>.zst`).
    pub archive: String,
}

/// `pages_by_id` sharding, matching the publisher exactly: 2/2/rest of a
/// zero-padded (≥5-digit) page id. (Test fixtures lay archives down by it;
/// the walker itself trusts the manifest's `archive` path.)
#[cfg(test)]
fn page_id_shards(page_id: &str) -> (String, String, String) {
    let padded = format!("{page_id:0>5}");
    (
        padded[..2].to_string(),
        padded[2..4].to_string(),
        padded[4..].to_string(),
    )
}

/// The site-relative archive path for a page id.
#[cfg(test)]
pub(super) fn archive_rel(page_id: &str) -> String {
    let (a, b, rest) = page_id_shards(page_id);
    format!("pages_by_id/{a}/{b}/{rest}.zst")
}

// ── Change detection stamps ──

/// `(mtime, size)` of one file — the site-level change gate. The publisher
/// writes tmp+rename and skips byte-identical rewrites, so an untouched
/// publication keeps its stamp however often the daemon republishes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Stamp(std::time::SystemTime, u64);

pub(super) fn stamp(path: &Path) -> Option<Stamp> {
    let md = std::fs::metadata(path).ok()?;
    Some(Stamp(md.modified().ok()?, md.len()))
}

// ── pages.json ──

#[derive(serde::Deserialize)]
struct ManifestDoc {
    #[serde(default)]
    pages: Vec<ManifestPage>,
}

#[derive(serde::Deserialize)]
struct ManifestPage {
    id: String,
    slug: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    revisions_stored: i64,
    #[serde(default)]
    max_rev: i64,
    #[serde(default)]
    archive: String,
}

/// Parse a site's `pages.json` into `{page id → row}`. `None` on any read or
/// parse error (the caller keeps serving the last adopted rows).
pub(super) fn read_pages_manifest(site_dir: &Path) -> Option<HashMap<u64, PageRow>> {
    let bytes = std::fs::read(site_dir.join("pages.json")).ok()?;
    let doc: ManifestDoc = serde_json::from_slice(&bytes).ok()?;
    let mut rows = HashMap::with_capacity(doc.pages.len());
    for p in doc.pages {
        let Ok(id) = p.id.parse::<u64>() else {
            continue;
        };
        rows.insert(
            id,
            PageRow {
                slug: p.slug,
                title: p.title,
                tags: p.tags,
                stored: p.revisions_stored,
                max_rev: p.max_rev,
                archive: p.archive,
            },
        );
    }
    Some(rows)
}

// ── Page archives ──

/// Read one page's tar+zst archive: every `rNNN.txt` entry contributes its
/// frontmatter to the revision table, and the highest revision's body is
/// materialised into `bodies` (frontmatter stripped, NBSP-normalised,
/// content-addressed). Page identity comes from the manifest row, not the
/// frontmatter (the row is the fresher half of the same publication).
/// `None` when the archive is missing or unreadable — the page reads as
/// gone, matching the stored-count-0 convention.
pub(super) fn read_page(
    site_dir: &Path,
    page_id: &str,
    row: &PageRow,
    bodies: &mut ImHashMap<BlobId, Arc<str>>,
) -> Option<Article> {
    let bytes = std::fs::read(site_dir.join(&row.archive)).ok()?;
    let raw = zstd::stream::decode_all(&bytes[..]).ok()?;
    let mut revisions = Vec::new();
    let mut latest: Option<(u64, String)> = None;
    let mut archive = Archive::new(&raw[..]);
    let entries = archive.entries().ok()?;
    for entry in entries {
        let mut entry = entry.ok()?;
        let Ok(name) = entry.path() else {
            continue;
        };
        let Some(no) = rev_number(&name.to_string_lossy()) else {
            continue;
        };
        let mut text = String::new();
        if entry.read_to_string(&mut text).is_err() {
            continue;
        }
        let Some(rev) = parse_rev_meta(&text) else {
            continue;
        };
        if latest.as_ref().is_none_or(|(n, _)| no >= *n) {
            latest = Some((no, revision_body(&text).to_string()));
        }
        revisions.push(rev);
    }
    let (_, body_text) = latest?;
    let (body_id, body) = materialize_body(&body_text);
    bodies.insert(body_id, body);
    revisions.sort_unstable_by_key(|r| r.revision);
    Some(Article {
        meta: ArticleMeta {
            title: row.title.clone(),
            tags: row.tags.clone(),
            slug: row.slug.clone(),
            page_id: page_id.to_string(),
        },
        latest_body: body_id,
        revisions,
    })
}

/// The `rNNN.txt` frontmatter's revision row: `revision`/`revision_id`/
/// `timestamp`/`author` (identity keys only — the page-level title/tags/slug
/// come from the manifest). `None` when the frontmatter is absent or carries
/// no revision number (the entry is skipped).
fn parse_rev_meta(text: &str) -> Option<RevMeta> {
    let rest = text.strip_prefix("---\n")?;
    let header = rest.split("\n---\n").next()?;
    let mut revision = None;
    let mut revision_id = String::new();
    let mut timestamp = 0;
    let mut author = String::new();
    for line in header.lines() {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        match k.trim() {
            "revision" => revision = v.trim().parse().ok(),
            "revision_id" => revision_id = strip_quotes(v.trim()).to_string(),
            "timestamp" => timestamp = v.trim().parse().unwrap_or(0),
            "author" => author = strip_quotes(v.trim()).to_string(),
            _ => {}
        }
    }
    Some(RevMeta {
        revision: revision?,
        revision_id,
        timestamp,
        author,
    })
}

/// Materialise one body as its [`RepoSnapshot`] entry: every NBSP (U+00A0)
/// rewritten to a plain space, then content-addressed by SHA-256 (truncated
/// to the 20-byte [`BlobId`]). The rewrite is the dataset-boundary
/// normalisation Wikidot exports lean on NBSP for (list indentation, trailing
/// whitespace); the content addressing is what makes an update additive — a
/// changed body is a new id, and identical bodies dedup to one entry.
pub(super) fn materialize_body(body: &str) -> (BlobId, Arc<str>) {
    let normalized: Arc<str> = Arc::from(body.replace('\u{a0}', " "));
    let sum = ring::digest::digest(&ring::digest::SHA256, normalized.as_bytes());
    let mut id = [0u8; 20];
    id.copy_from_slice(&sum.as_ref()[..20]);
    (BlobId(id), normalized)
}

// ── files.json ──

#[derive(serde::Deserialize)]
struct FilesDoc {
    #[serde(default)]
    files: Vec<FileRow>,
}

#[derive(serde::Deserialize)]
struct FileRow {
    /// The absolute URL the bytes came from (`https://host/path`).
    path: String,
    sha256: Option<String>,
    status: String,
}

/// Parse `files.json` into the site's `files/` index — a faithful projection:
/// each *saved* row keyed exactly as the DB names it, percent-decoded. The DB
/// keys a file by its site-relative path (`local--files/…`) when it lives on
/// this site, or its absolute URL when it doesn't; lookups that name a
/// `host/path` tail (the form [`http_tail`] yields from an in-article URL,
/// or [`parse_shell`] from `theme_root`) retry the bare relative key for the
/// site's own hosts — see [`dataset::resource`]. Pending and missing entries
/// stay unindexed (a request for them misses, then falls back to the source
/// site — same as an unmirrored hotlink).
pub(super) fn read_files_index(site_dir: &Path) -> HashMap<RepoAssetPath, CaRef> {
    let mut map = HashMap::new();
    let Ok(bytes) = std::fs::read(site_dir.join("files.json")) else {
        return map;
    };
    let Ok(doc) = serde_json::from_slice::<FilesDoc>(&bytes) else {
        return map;
    };
    for f in doc.files {
        if f.status != "saved" {
            continue;
        }
        let Some(hash) = f.sha256 else { continue };
        if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
            continue;
        }
        let key = percent_decode(url_tail(&f.path).as_deref().unwrap_or(&f.path));
        let Some(path) = RepoAssetPath::new(key) else {
            continue;
        };
        let ext = Path::new(&f.path)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_string();
        map.insert(path, CaRef { hash, ext });
    }
    map
}

/// `https://host/path…` → `host/path…` (`None` for any non-URL shape).
pub(super) fn url_tail(url: &str) -> Option<String> {
    url.strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .map(str::to_string)
}

// ── shell ──

/// Read the site chrome (`shell`) — see [`parse_shell`] for the format.
pub(super) fn read_shell(
    site_dir: &Path,
) -> (Option<String>, Option<String>, Option<RepoAssetPath>) {
    match std::fs::read_to_string(site_dir.join("shell")) {
        Ok(text) => parse_shell(&text),
        Err(_) => (None, None, None),
    }
}

// ── Whole-corpus build ──

/// Build one site's [`WDWebsite`] out of its publication: every stored page
/// read and materialised, the `files/` index and shell folded in. Returns
/// the adopted manifest rows alongside (the diff base for later ticks).
/// `None` when `pages.json` is unreadable (nothing servable).
pub(super) fn build_site(
    site_dir: &Path,
    bodies: &mut ImHashMap<BlobId, Arc<str>>,
) -> Option<(HashMap<u64, PageRow>, WDWebsite)> {
    let rows = read_pages_manifest(site_dir)?;
    let mut w = WDWebsite::default();
    for (id, row) in &rows {
        let Some(article) = read_stored_page(site_dir, id, row, bodies) else {
            continue;
        };
        let Some((cat, name)) = slug_to_key(&article.meta.slug) else {
            continue;
        };
        w.by_page_id.insert(*id, (cat.clone(), name.clone()));
        w.articles.entry(cat).or_default().insert(name, article);
    }
    w.files = read_files_index(site_dir).into_iter().collect();
    let (title, subtitle, theme_root) = read_shell(site_dir);
    w.title = title;
    w.subtitle = subtitle;
    w.theme_root = theme_root;
    Some((rows, w))
}

/// [`read_page`] behind the stored-count gate: a page whose revisions are
/// all still unfetched has no archive on disk.
pub(super) fn read_stored_page(
    site_dir: &Path,
    id: &u64,
    row: &PageRow,
    bodies: &mut ImHashMap<BlobId, Arc<str>>,
) -> Option<Article> {
    if row.stored == 0 {
        return None;
    }
    read_page(site_dir, &id.to_string(), row, bodies)
}

/// Every published site directory under `<dir>/out/` (site name → path).
/// A missing or unreadable `out/` yields an empty map — an empty corpus,
/// same as an absent clone before it.
pub(super) fn site_dirs(out_dir: &Path) -> HashMap<String, PathBuf> {
    let Ok(rd) = std::fs::read_dir(out_dir) else {
        return HashMap::new();
    };
    rd.filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            Some((name, e.path()))
        })
        .collect()
}

/// Parse the revision number out of an archive entry name: `r12.txt` → `12`.
pub(super) fn rev_number(name: &str) -> Option<u64> {
    name.strip_prefix('r')
        .and_then(|s| s.strip_suffix(".txt"))
        .and_then(|s| s.parse().ok())
}

/// Percent-decode `%XX` escapes (e.g. the publisher's `component%3Atheme` →
/// `component:theme`). Used to normalise `files.json` keys to the same form
/// [`http_tail`] yields from an in-article URL (which carries the literal
/// character), so a lookup matches regardless of which side encoded it.
pub(super) fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Some(b) = hex_pair(bytes[i + 1], bytes[i + 2])
        {
            out.push(b);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| s.to_string())
}

/// Decode one `%XX` byte, or `None` if either nibble isn't hex.
pub(super) fn hex_pair(a: u8, b: u8) -> Option<u8> {
    Some((hex_digit(a)? << 4) | hex_digit(b)?)
}

pub(super) fn hex_digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Strip a revision file's `---\n…\n---\n` frontmatter, returning the body.
/// If the frontmatter is absent the whole text is the body.
pub(super) fn revision_body(text: &str) -> &str {
    let Some(rest) = text.strip_prefix("---\n") else {
        return text;
    };
    let Some(end) = rest.find("\n---\n") else {
        return text;
    };
    &rest[end + "\n---\n".len()..]
}

/// Split a canonical slug into `(Option<category>, name)`: `help:foo` →
/// `(Some("help"), "foo")`, `foo` → `(None, "foo")`.
pub(super) fn slug_parts(slug: &str) -> (Option<String>, String) {
    match slug.split_once(':') {
        Some((cat, name)) => (Some(cat.to_string()), name.to_string()),
        None => (None, slug.to_string()),
    }
}

/// `(Option<category>, name)` as validated [`SafePathComponent`]s, or `None` if
/// either segment is unsafe (the page is dropped, as in [`build_site`]).
pub(super) fn slug_to_key(slug: &str) -> Option<(Option<SafePathComponent>, SafePathComponent)> {
    let (cat, name) = slug_parts(slug);
    let name = SafePathComponent::new(name)?;
    let cat = match cat {
        None => None,
        Some(c) => Some(SafePathComponent::new(c)?),
    };
    Some((cat, name))
}

/// Strip one layer of surrounding double quotes, if present.
pub(super) fn strip_quotes(s: &str) -> &str {
    if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
        &s[1..s.len() - 1]
    } else {
        s
    }
}
