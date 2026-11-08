use super::*;
use std::io::{ErrorKind, Read};
use tar::Archive;

// =========================================================================
// out/ publication readers (the evakuilo immutable generation)
// =========================================================================

/// A SHA-256 hex string long enough to shard (`aa/bb/rest`).
fn valid_sha(sha: &str) -> bool {
    sha.len() >= 4 && sha.bytes().all(|b| b.is_ascii_hexdigit())
}

/// One immutable object under `<site>/objects/<kind>/aa/bb/<sha><ext>`.
/// The JSON sections carry a `.json` suffix; the shell object is raw bytes.
fn object_path(site_dir: &Path, kind: &str, sha: &str) -> Option<PathBuf> {
    if !valid_sha(sha) {
        return None;
    }
    let name = match kind {
        "pages" | "files" => format!("{sha}.json"),
        _ => sha.to_string(),
    };
    Some(
        site_dir
            .join("objects")
            .join(kind)
            .join(&sha[..2])
            .join(&sha[2..4])
            .join(name),
    )
}

/// One immutable root under `<site>/roots/aa/bb/<sha>.json`.
fn root_path(site_dir: &Path, sha: &str) -> Option<PathBuf> {
    if !valid_sha(sha) {
        return None;
    }
    Some(
        site_dir
            .join("roots")
            .join(&sha[..2])
            .join(&sha[2..4])
            .join(format!("{sha}.json")),
    )
}

async fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    serde_json::from_slice(&fs::read(path).await.ok()?).ok()
}

/// A publication root: the three section hashes of one generation.
#[derive(serde::Deserialize)]
struct RootDoc {
    pages: String,
    files: String,
    shell: String,
}

// ── pages section ──

#[derive(serde::Deserialize)]
struct PagesDoc {
    #[serde(default)]
    pages: Vec<PageEntry>,
}

#[derive(serde::Deserialize)]
struct PageEntry {
    id: String,
    slug: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    revisions_stored: u64,
    #[serde(default)]
    revisions_known: u64,
    #[serde(default)]
    max_rev: u64,
    #[serde(default)]
    created_by: Option<String>,
    #[serde(default)]
    created_at: Option<i64>,
    #[serde(default)]
    updated_by: Option<String>,
    #[serde(default)]
    updated_at: Option<i64>,
    #[serde(default)]
    archive: Option<String>,
    #[serde(default)]
    archive_sha256: Option<String>,
}

// ── files section ──

#[derive(serde::Deserialize)]
struct FilesDoc {
    #[serde(default)]
    files: Vec<FileEntry>,
}

#[derive(serde::Deserialize)]
struct FileEntry {
    path: String,
    #[serde(default)]
    sha256: Option<String>,
    #[serde(default)]
    content_type: Option<String>,
    #[serde(default)]
    status: String,
}

// ── Whole-generation reader ──

/// Read `<site_dir>/HEAD`: `Ok(Some(head))` for a publication, `Ok(None)`
/// when the site has no `HEAD` file, `Err(())` on a transient read failure or
/// a malformed `HEAD` (the caller keeps its previous output).
pub(super) async fn read_head(site_dir: &Path) -> Result<Option<String>, ()> {
    match fs::read(site_dir.join("HEAD")).await {
        Ok(bytes) => {
            let head = String::from_utf8(bytes).map_err(|_| ())?;
            let head = head.trim();
            if head.is_empty() {
                return Err(());
            }
            Ok(Some(head.to_string()))
        }
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(_) => Err(()),
    }
}

/// Decode the generation named by `<site_dir>/HEAD` into [`RepoMeta`].
///
/// A missing `HEAD` means the site has no publication and yields the empty
/// metadata; any other read failure yields `None` (the caller keeps its
/// previous output rather than flapping on a transient I/O error).
///
/// The pages section carries the first/last stored revision summary, so this
/// never opens a page archive.
pub(super) async fn read_repo_meta(site: &SafePathComponent, site_dir: &Path) -> Option<RepoMeta> {
    let head = match read_head(site_dir).await {
        Ok(Some(head)) => head,
        Ok(None) => return Some(RepoMeta::default()),
        Err(()) => return None,
    };
    let root: RootDoc = read_json(&root_path(site_dir, &head)?).await?;
    let pages: PagesDoc = read_json(&object_path(site_dir, "pages", &root.pages)?).await?;
    let files: FilesDoc = read_json(&object_path(site_dir, "files", &root.files)?).await?;
    let shell = fs::read(object_path(site_dir, "shell", &root.shell)?)
        .await
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok())?;
    let chrome = parse_shell(&shell);

    let mut meta = RepoMeta {
        head: Some(head),
        title: chrome.title,
        subtitle: chrome.subtitle,
        theme_root: chrome.theme_root,
        landing: chrome.landing,
        ..RepoMeta::default()
    };
    for entry in pages.pages {
        let Ok(id) = entry.id.parse::<u64>() else {
            continue;
        };
        let Some(slug) = parse_slug(&entry.slug) else {
            continue;
        };
        let archive = match (entry.archive, entry.archive_sha256) {
            (Some(path), Some(sha256)) => Some(PageArchive { path, sha256 }),
            _ => None,
        };
        meta.by_slug.insert(slug, id);
        meta.pages.insert(
            id,
            PageMeta {
                id,
                slug: entry.slug,
                title: entry.title,
                tags: entry.tags,
                revisions_stored: entry.revisions_stored,
                revisions_known: entry.revisions_known,
                max_rev: entry.max_rev,
                created_by: entry.created_by,
                created_at: entry.created_at,
                updated_by: entry.updated_by,
                updated_at: entry.updated_at,
                archive,
            },
        );
    }
    let (files, files_ca) = files_index(site, files);
    meta.files = files.into_iter().collect();
    meta.files_ca = files_ca.into_iter().collect();
    Some(meta)
}

/// One archive decode's worth of page data: the highest revision's body plus
/// every stored revision's frontmatter.
pub(super) struct LatestPage {
    pub body: Arc<str>,
    pub revisions: Vec<RevMeta>,
}

/// What one archive entry contributes while streaming through it.
struct ArchiveRead {
    latest: Option<(u64, String)>,
    revisions: Vec<RevMeta>,
}

/// Decode one archive once, keeping its highest revision body and (when
/// asked) every `rNNN.txt` frontmatter row, oldest first.
fn read_archive(bytes: &[u8], collect_revisions: bool) -> Option<ArchiveRead> {
    let raw = zstd::stream::decode_all(bytes).ok()?;
    let mut latest: Option<(u64, String)> = None;
    let mut revisions = Vec::new();
    let mut archive = Archive::new(&raw[..]);
    for entry in archive.entries().ok()? {
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
        if collect_revisions && let Some(rev) = parse_rev_meta(&text) {
            revisions.push(rev);
        }
        if latest.as_ref().is_none_or(|(n, _)| no >= *n) {
            latest = Some((no, revision_body(&text).to_string()));
        }
    }
    revisions.sort_unstable_by_key(|r| r.revision);
    Some(ArchiveRead { latest, revisions })
}

/// Decode one archive and return its highest revision body, NBSP-normalised.
fn latest_body_of(bytes: &[u8]) -> Option<Arc<str>> {
    let (_, body) = read_archive(bytes, false)?.latest?;
    Some(Arc::from(body.replace('\u{a0}', " ")))
}

/// Read one page's latest body and full revision history out of its immutable
/// archive in one decode. `None` when the page has no archive or the archive
/// is unreadable.
pub(super) async fn read_latest_page_at(site_dir: &Path, page: &PageMeta) -> Option<LatestPage> {
    let archive = page.archive.as_ref()?;
    let bytes = fs::read(site_dir.join(&archive.path)).await.ok()?;
    let read = read_archive(&bytes, true)?;
    let (_, body) = read.latest?;
    Some(LatestPage {
        body: Arc::from(body.replace('\u{a0}', " ")),
        revisions: read.revisions,
    })
}

/// Read one page's latest body out of its immutable archive. `None` when the
/// page has no archive or the archive is unreadable.
pub(super) async fn read_latest_body_at(site_dir: &Path, page: &PageMeta) -> Option<Arc<str>> {
    let archive = page.archive.as_ref()?;
    let bytes = fs::read(site_dir.join(&archive.path)).await.ok()?;
    latest_body_of(&bytes)
}

/// [`read_latest_page_at`] for a page on a registered site, resolving the
/// publication directory through the process globals.
pub(super) async fn read_latest_page(
    site: &SafePathComponent,
    page: &PageMeta,
) -> Option<LatestPage> {
    let site_dir = crate::globals::repo().site_dir(site);
    read_latest_page_at(&site_dir, page).await
}

/// [`read_latest_body_at`] for a page on a registered site, resolving the
/// publication directory through the process globals.
pub(super) async fn read_latest_body(
    site: &SafePathComponent,
    page: &PageMeta,
) -> Option<Arc<str>> {
    let site_dir = crate::globals::repo().site_dir(site);
    read_latest_body_at(&site_dir, page).await
}

/// The `rNNN.txt` frontmatter's revision row: `revision`/`revision_id`/
/// `timestamp`/`author` (identity keys only — the page-level title/tags/slug
/// come from the pages section). `None` when the frontmatter is absent or
/// carries no revision number.
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

/// Parse a files section into the site's `files/` index, both halves: the
/// forward map (each *saved* row keyed by [`canon_file_key`]) and the reverse
/// map (each CA hash → the row's recorded `content_type` and original URL).
/// Pending and missing entries stay unindexed.
fn files_index(
    site: &SafePathComponent,
    doc: FilesDoc,
) -> (HashMap<RepoAssetPath, String>, HashMap<String, CaFile>) {
    let mut files = HashMap::new();
    let mut files_ca = HashMap::new();
    for f in doc.files {
        if f.status != "saved" {
            continue;
        }
        let Some(hash) = f.sha256 else { continue };
        if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
            continue;
        }
        // A scheme-less row is a legacy site-relative path (`local--…`) —
        // the same file on its site's canonical host; anything else is the
        // publisher's canonical absolute URL.
        let url = if f.path.starts_with("http://") || f.path.starts_with("https://") {
            f.path
        } else {
            format!("http://{}.wikidot.com/{}", &**site, f.path)
        };
        let Some(path) = canon_file_key(&url) else {
            continue;
        };
        files.insert(path.clone(), hash.clone());
        files_ca.insert(
            hash,
            CaFile {
                content_type: f.content_type,
                path,
            },
        );
    }
    (files, files_ca)
}

/// Canonicalise one file URL — a files-section `path`, the shell's
/// `theme_root`, or the `host/path` tail [`http_tail`] yields from an
/// in-article reference — into the `files/` index key `host/path`:
/// - the scheme drops (a key never carries it);
/// - a `.wikidot.com` / `.wdfiles.com` host (± `www.`, this site or a foreign
///   one — sandboxes) rewrites to `<sub>.wikidot.com` — the one host all of a
///   file's wikidot spellings share;
/// - every other host (custom domains, font/theme CDNs) keeps its `www.` and
///   spelling as written;
/// - the fragment and query drop (Wikidot serves `…?width=…` with the same
///   bytes), `//` collapses (the corpus's `…//x.png` quirk), and percent
///   escapes decode (`component%3Atheme` → `component:theme`, `%20` →
///   space — the index and the lookup must agree, and decoding both sides
///   collapses strictly more spellings of one URL).
/// Both sides of every lookup run through this one function, so any two
/// spellings of one file meet at one key. `None` for shapes that name no
/// file (`..`, empty segments, non-URLs).
///
/// [`http_tail`]: kolorinko_render::http_tail
pub(super) fn canon_file_key(url: &str) -> Option<RepoAssetPath> {
    let url = url.split(['#', '?']).next().unwrap_or(url);
    let tail = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);
    let (host, path) = tail.split_once('/')?;
    let host = host.to_ascii_lowercase();
    let no_www = host.strip_prefix("www.").unwrap_or(&host);
    let sub = no_www
        .strip_suffix(".wikidot.com")
        .or_else(|| no_www.strip_suffix(".wdfiles.com"));
    let key = match sub {
        Some(sub) => format!("{sub}.wikidot.com/{}", percent_decode(path)),
        None => format!("{host}/{}", percent_decode(path)),
    };
    RepoAssetPath::new(key.replace("//", "/"))
}

// ── shell ──

/// The parsed `shell` object — one site's chrome. The `landing` slug is always
/// present ([`start_slug`] unless the shell names one); everything else is
/// `Option` because the shell may omit it.
pub(super) struct SiteChrome {
    pub(super) title: Option<String>,
    pub(super) subtitle: Option<String>,
    pub(super) theme_root: Option<RepoAssetPath>,
    pub(super) landing: Slug,
}

/// Parse a `shell` object: `title`, `subtitle` (both quoted), `theme_root`,
/// and `landing` (a quoted `cat:name` slug). Returns the title/subtitle
/// verbatim and the theme root as the canonical [`canon_file_key`] index key.
/// Unknown keys are ignored; a missing `theme_root` — or one with no mirrored
/// identity (e.g. a site-relative CSS path, no host) — yields `None`; a
/// missing or unparsable `landing` falls back to [`start_slug`].
pub(super) fn parse_shell(text: &str) -> SiteChrome {
    let mut chrome = SiteChrome {
        title: None,
        subtitle: None,
        theme_root: None,
        landing: start_slug(),
    };
    for line in text.lines() {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let v = v.trim();
        match k.trim() {
            "title" => chrome.title = Some(quoted_scalar(v)),
            "subtitle" => chrome.subtitle = Some(quoted_scalar(v)),
            "theme_root" => chrome.theme_root = canon_file_key(v),
            "landing" => chrome.landing = parse_slug(strip_quotes(v)).unwrap_or_else(start_slug),
            _ => {}
        }
    }
    chrome
}

/// Decode a YAML double-quoted scalar (the `shell` object quotes title and
/// subtitle): the archiver escapes embedded quotes, and WikiDot serves the
/// decoded text — `\"` is a literal quote, `\\` a backslash, `\n`/`\t` the
/// control characters. Escapes beyond those don't occur in the corpus and
/// pass through verbatim.
fn quoted_scalar(s: &str) -> String {
    let body = strip_quotes(s);
    let mut out = String::with_capacity(body.len());
    let mut chars = body.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// Parse the revision number out of an archive entry name: `r12.txt` → `12`.
pub(super) fn rev_number(name: &str) -> Option<u64> {
    name.strip_prefix('r')
        .and_then(|s| s.strip_suffix(".txt"))
        .and_then(|s| s.parse().ok())
}

/// Percent-decode `%XX` escapes (e.g. the publisher's `component%3Atheme` →
/// `component:theme`). Used to normalise files-section keys to the same form
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

/// Strip one layer of surrounding double quotes, if present.
pub(super) fn strip_quotes(s: &str) -> &str {
    if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
        &s[1..s.len() - 1]
    } else {
        s
    }
}
