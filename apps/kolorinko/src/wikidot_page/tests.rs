use super::repo_gear::{load_meta, store_meta};
use super::*;
use std::fs;

/// A 64-hex sha256 stand-in (of the literal bytes "css").
const HASH: &str = "d1f69a9854765a4f1e7c8b1e8a9e5c9bd1e0a2f3c4b5a6978899aabbccddeeff";
const HASH2: &str = "e2f69a9854765a4f1e7c8b1e8a9e5c9bd1e0a2f3c4b5a6978899aabbccddeef0";
const HASH3: &str = "f3f69a9854765a4f1e7c8b1e8a9e5c9bd1e0a2f3c4b5a6978899aabbccddee11";

fn site(s: &str) -> SafePathComponent {
    SafePathComponent::new(s.into()).unwrap()
}

/// Drive one async publication read (compio supplies the runtime the blocking
/// decode pool needs).
fn block_on<F: std::future::Future>(f: F) -> F::Output {
    compio::runtime::Runtime::new().unwrap().block_on(f)
}

/// A [`PageSource`] pinned to a fixture `out/` tree and a fixed metadata set.
struct FixtureSource {
    root: PathBuf,
    metas: HashMap<SafePathComponent, RepoMeta>,
}

impl PageSource for FixtureSource {
    async fn meta(&mut self, site: &SafePathComponent) -> Option<RepoMeta> {
        self.metas.get(site).cloned()
    }

    async fn body(&mut self, site: &SafePathComponent, page: &PageMeta) -> Option<Arc<str>> {
        read_latest_body_at(&self.root.join(&**site), page).await
    }
}

fn root_slug(name: &str) -> Slug {
    (None, SafePathComponent::new(name.into()).unwrap())
}

fn sha256_hex(bytes: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, bytes)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// `<kind>/aa/bb/<sha><ext>` — the publisher's object sharding.
fn object_rel(kind: &str, sha: &str, ext: &str) -> String {
    format!("{kind}/{}/{}/{sha}{ext}", &sha[..2], &sha[2..4])
}

/// Write one immutable object (roots live outside `objects/`).
fn write_object(site_dir: &Path, kind: &str, bytes: &[u8], ext: &str) -> String {
    let sha = sha256_hex(bytes);
    let rel = object_rel(kind, &sha, ext);
    let path = if kind == "roots" {
        site_dir.join(&rel)
    } else {
        site_dir.join("objects").join(&rel)
    };
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
    sha
}

/// Pack one page's revisions into the publisher's deterministic tar+zstd.
fn pack_page(
    site: &str,
    id: &str,
    slug: &str,
    title: &str,
    revs: &[(u64, &str, i64, &str)],
) -> Vec<u8> {
    let mut tarb = tar::Builder::new(Vec::new());
    for (no, rid, ts, body) in revs {
        let text = format!(
            "---\n\
             title: \"{title}\"\n\
             tags: []\n\
             page_id: \"{id}\"\n\
             site: \"{site}\"\n\
             slug: \"{slug}\"\n\
             revision: {no}\n\
             revision_id: \"{rid}\"\n\
             author: 7\n\
             timestamp: {ts}\n\
             ---\n\
             {body}\n"
        );
        let mut header = tar::Header::new_gnu();
        header.set_size(text.len() as u64);
        header.set_mode(0o644);
        header.set_mtime(0);
        tarb.append_data(&mut header, format!("r{no:0>3}.txt"), text.as_bytes())
            .unwrap();
    }
    zstd::stream::encode_all(&tarb.into_inner().unwrap()[..], 3).unwrap()
}

/// A publication builder: accumulate pages/files/shell, then write one
/// immutable generation and point `HEAD` at it.
struct Pub {
    root: PathBuf,
    site: String,
    pages: Vec<serde_json::Value>,
    files: Vec<serde_json::Value>,
    shell: String,
}

impl Pub {
    fn new(root: &Path, site: &str) -> Self {
        Self {
            root: root.to_path_buf(),
            site: site.to_string(),
            pages: Vec::new(),
            files: Vec::new(),
            shell: String::new(),
        }
    }

    fn site_dir(&self) -> PathBuf {
        self.root.join("out").join(&self.site)
    }

    fn page(&mut self, id: &str, slug: &str, revs: &[(u64, &str, i64, &str)]) -> &mut Self {
        let title = format!("T {slug}");
        let dir = self.site_dir();
        let (archive, archive_sha256) = if revs.is_empty() {
            (None, None)
        } else {
            let bytes = pack_page(&self.site, id, slug, &title, revs);
            let sha = sha256_hex(&bytes);
            let rel = object_rel("archives", &sha, ".zst");
            let path = dir.join("objects").join(&rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, &bytes).unwrap();
            (Some(format!("objects/{rel}")), Some(sha))
        };
        let max_rev = revs.iter().map(|(n, ..)| *n).max().unwrap_or(0);
        let first = revs.iter().min_by_key(|(n, ..)| *n);
        let last = revs.iter().max_by_key(|(n, ..)| *n);
        self.pages.push(serde_json::json!({
            "id": id,
            "slug": slug,
            "title": title,
            "tags": [],
            "revisions_stored": revs.len(),
            "revisions_known": revs.len(),
            "max_rev": max_rev,
            "created_by": first.map(|_| "7"),
            "created_at": first.map(|(_, _, ts, _)| *ts),
            "updated_by": last.map(|_| "7"),
            "updated_at": last.map(|(_, _, ts, _)| *ts),
            "archive": archive,
            "archive_sha256": archive_sha256,
        }));
        self
    }

    fn file(&mut self, url: &str, sha: &str, status: &str, ct: &str, bytes: &[u8]) -> &mut Self {
        let dir = self.site_dir();
        if status == "saved" {
            let blob = dir
                .join("files_ca")
                .join(&sha[..2])
                .join(&sha[2..4])
                .join(&sha[4..]);
            fs::create_dir_all(blob.parent().unwrap()).unwrap();
            fs::write(blob, bytes).unwrap();
        }
        self.files.push(serde_json::json!({
            "path": url,
            "sha256": sha,
            "size": bytes.len(),
            "content_type": ct,
            "status": status,
            "blob": format!("files_ca/{}/{}/{}", &sha[..2], &sha[2..4], &sha[4..]),
        }));
        self
    }

    fn write(&self) -> PathBuf {
        let dir = self.site_dir();
        fs::create_dir_all(&dir).unwrap();
        let pages = serde_json::to_vec(
            &serde_json::json!({ "v": 3, "site": self.site, "pages": self.pages }),
        )
        .unwrap();
        let files = serde_json::to_vec(
            &serde_json::json!({ "v": 2, "site": self.site, "files": self.files }),
        )
        .unwrap();
        let pages_sha = write_object(&dir, "pages", &pages, ".json");
        let files_sha = write_object(&dir, "files", &files, ".json");
        let shell_sha = write_object(&dir, "shell", self.shell.as_bytes(), "");
        let root = serde_json::to_vec(&serde_json::json!({
            "v": 2,
            "site": self.site,
            "pages": pages_sha,
            "files": files_sha,
            "shell": shell_sha,
        }))
        .unwrap();
        let root_sha = write_object(&dir, "roots", &root, ".json");
        fs::write(dir.join("HEAD"), format!("{root_sha}\n")).unwrap();
        dir
    }
}

/// A cold site with no `HEAD` reads as the empty metadata.
#[test]
fn repo_missing_publication_is_empty() {
    let dir = std::env::temp_dir().join(format!("kolorinko_nopath_{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    let meta = block_on(repo_step(
        &site("nope"),
        &dir.join("out").join("nope"),
        None,
    ));
    assert!(meta.pages.is_empty());
    assert!(meta.files.is_empty());
    assert!(meta.head.is_none());
}

/// The revision summary lives in `pages/`, so repo metadata is complete even
/// after every archive is gone.
#[test]
fn repo_metadata_needs_no_archives() {
    let dir = std::env::temp_dir().join(format!("kolorinko_noarch_{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    let mut pub_builder = Pub::new(&dir, "scp");
    pub_builder.page(
        "1",
        "foo",
        &[(1, "rid-1", 100, "Foo"), (2, "rid-2", 200, "Foo v2")],
    );
    let site_dir = pub_builder.write();
    let archive = {
        let meta = block_on(read_repo_meta(&site("scp"), &site_dir)).unwrap();
        meta.pages[&1].archive.clone().unwrap()
    };
    fs::remove_file(site_dir.join(&archive.path)).unwrap();

    let meta = block_on(read_repo_meta(&site("scp"), &site_dir)).unwrap();
    let page = &meta.pages[&1];
    assert_eq!(page.created_by.as_deref(), Some("7"));
    assert_eq!(page.created_at, Some(100));
    assert_eq!(page.updated_by.as_deref(), Some("7"));
    assert_eq!(page.updated_at, Some(200));
}

/// The repo generation image round-trips through the gear view (persistent
/// imbl maps and revision frontmatter included), so a restarted process reuses
/// it instead of decoding every archive again.
#[test]
fn repo_cache_round_trips_the_generation() {
    use compio::runtime::Runtime;
    use dentrado::core::storage::InMemoryStorage;
    let dir = std::env::temp_dir().join(format!("kolorinko_repocache_{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    let mut pub_builder = Pub::new(&dir, "scp");
    pub_builder.page("1", "foo", &[(1, "rid-1", 100, "Foo")]);
    let site_dir = pub_builder.write();
    let meta = block_on(read_repo_meta(&site("scp"), &site_dir)).unwrap();

    let rt = Runtime::new().unwrap();
    let storage: InMemoryStorage<KolorinkoRT> = InMemoryStorage::new();
    let mut view = rt.block_on(storage.gear_view(&crate::runtime::GearId::Repo(site("scp"))));
    rt.block_on(store_meta(&mut view, &meta));
    view.commit();

    let mut view = rt.block_on(storage.gear_view(&crate::runtime::GearId::Repo(site("scp"))));
    let cached = rt
        .block_on(load_meta(&mut view))
        .expect("cached generation");
    assert_eq!(cached.head, meta.head);
    assert_eq!(cached.title, meta.title);
    assert_eq!(cached.pages.len(), meta.pages.len());
    assert_eq!(cached.by_slug, meta.by_slug);
    assert_eq!(cached.files, meta.files);
    assert_eq!(cached.pages[&1].created_at, Some(100));
    assert_eq!(cached.pages[&1].updated_at, Some(100));
}

/// The chunked layer splits a blob across records and reassembles it;
/// overwriting with a shorter image drops the stale tail blocks.
#[test]
fn chunked_round_trips_and_drops_stale_blocks() {
    use compio::runtime::Runtime;
    use dentrado::core::storage::InMemoryStorage;
    let rt = Runtime::new().unwrap();
    let storage: InMemoryStorage<KolorinkoRT> = InMemoryStorage::new();
    let mut view = rt.block_on(storage.gear_view(&crate::runtime::GearId::Repo(site("scp"))));
    let big: Vec<u8> = (0..20_000).map(|i| (i % 251) as u8).collect();
    rt.block_on(chunked::store(&mut view, b"k", &big));
    view.commit();
    let mut view = rt.block_on(storage.gear_view(&crate::runtime::GearId::Repo(site("scp"))));
    assert_eq!(rt.block_on(chunked::load(&mut view, b"k")).unwrap(), big);

    let small = b"small".to_vec();
    rt.block_on(chunked::store(&mut view, b"k", &small));
    view.commit();
    let mut view = rt.block_on(storage.gear_view(&crate::runtime::GearId::Repo(site("scp"))));
    assert_eq!(rt.block_on(chunked::load(&mut view, b"k")).unwrap(), small);
    assert_eq!(
        rt.block_on(view.scan(b"k/b/")).len(),
        1,
        "stale blocks were dropped"
    );
}

#[test]
fn read_repo_meta_reads_pages_files_and_summary() {
    let dir = std::env::temp_dir().join(format!("kolorinko_out_{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    let mut pub_builder = Pub::new(&dir, "scp");
    pub_builder
        .page("1305054470", "foo", &[(1, "rid-1", 100, "Foo body")])
        .page(
            "1305054471",
            "bar",
            &[(1, "rid-2", 101, "Bar body"), (2, "rid-3", 102, "Bar v2")],
        )
        .file(
            "https://scp.wikidot.com/local--files/foo/a.css",
            HASH,
            "saved",
            "text/css",
            b"css",
        );
    let site_dir = pub_builder.write();
    let meta = block_on(read_repo_meta(&site("scp"), &site_dir)).unwrap();

    assert!(meta.head.is_some(), "HEAD recorded");
    assert_eq!(meta.pages.len(), 2);
    let foo = article(&meta, &root_slug("foo")).unwrap();
    assert_eq!(foo.id, 1305054470);
    assert_eq!(foo.title, "T foo");
    assert_eq!(foo.created_by.as_deref(), Some("7"));
    assert_eq!(foo.created_at, Some(100));
    assert_eq!(foo.updated_by.as_deref(), Some("7"));
    assert_eq!(foo.updated_at, Some(100));
    let bar = article(&meta, &root_slug("bar")).unwrap();
    assert_eq!(bar.created_at, Some(101));
    assert_eq!(bar.updated_at, Some(102));
    // The full history stays page-local: one archive decode yields both the
    // latest body and every stored revision.
    let latest = block_on(read_latest_page_at(&site_dir, bar)).unwrap();
    assert_eq!(&*latest.body, "Bar v2\n");
    assert_eq!(latest.revisions.len(), 2);
    assert_eq!(latest.revisions[1].revision_id, "rid-3");
    // Canonical addressing: slug → page id.
    assert_eq!(meta.by_slug.get(&root_slug("foo")), Some(&1305054470));
    // The files index maps the percent-decoded host/path tail to the CA
    // hash; the reverse half carries the recorded type + original URL.
    let key = RepoAssetPath::new("scp.wikidot.com/local--files/foo/a.css".into()).unwrap();
    assert_eq!(meta.files.get(&key).map(String::as_str), Some(HASH));
    let file = meta.files_ca.get(HASH).expect("reverse row");
    assert_eq!(file.content_type.as_deref(), Some("text/css"));
    assert_eq!(file.path, key);
}

/// A legacy site-relative row (`local--files/…`) lifts onto the site's
/// canonical host, and every alias spelling of the URL — the wdfiles CDN,
/// `%3A` escapes, a configured custom domain — resolves to it through
/// [`canon_file_key`] + [`resource`]'s own-host retry.
#[test]
fn legacy_rows_lift_and_alias_spellings_resolve() {
    init_test_globals();
    let dir = std::env::temp_dir().join(format!("kolorinko_rel_{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    let mut pub_builder = Pub::new(&dir, "obscurative");
    pub_builder
        .page("1305054470", "foo", &[(1, "rid-1", 100, "Foo body")])
        .file(
            "local--theme/t/style.css",
            HASH,
            "saved",
            "text/css",
            b"css",
        )
        .file(
            "local--files/front-page:css/sublimity",
            HASH2,
            "saved",
            "text/css",
            b"css",
        );
    let site_dir = pub_builder.write();
    let meta = block_on(read_repo_meta(&site("obscurative"), &site_dir)).unwrap();
    // The relative row is indexed under the site's canonical host.
    let key =
        RepoAssetPath::new("obscurative.wikidot.com/local--theme/t/style.css".into()).unwrap();
    assert!(meta.files.contains_key(&key));
    // The extensionless row lifts the same way; the reverse entry keeps the
    // raw type, and the effective extension derives from it.
    let sublimity_key =
        RepoAssetPath::new("obscurative.wikidot.com/local--files/front-page:css/sublimity".into())
            .unwrap();
    let (hash, file) = resource(&meta, &site("obscurative"), &sublimity_key)
        .expect("sublimity resolves through the index");
    assert_eq!(hash, HASH2);
    assert_eq!(crate::assets::ca_file_ext(file), "text.css");
    // Every alias spelling canonicalizes (or retries) to that one row.
    for url in [
        "http://obscurative.wikidot.com/local--theme/t/style.css",
        "http://obscurative.wdfiles.com/local--theme/t/style.css",
        "http://www.obscurative.wikidot.com/local--theme/t/style.css",
        "https://files.www.obscurative.ru/local--theme/t/style.css",
    ] {
        let path = canon_file_key(url).unwrap_or_else(|| panic!("canon {url}"));
        let ca = resource(&meta, &site("obscurative"), &path);
        assert_eq!(ca.map(|(h, _)| h), Some(HASH), "{url}");
    }
    // A foreign host with the same path stays a hotlink.
    let foreign = RepoAssetPath::new("i.imgur.com/local--theme/t/style.css".into()).unwrap();
    assert!(resource(&meta, &site("obscurative"), &foreign).is_none());
}

/// A `local--resized-images/…` reference (a variant the export never saved)
/// resolves to the original `local--files/…` file — same host, whatever its
/// spelling.
#[test]
fn resized_variants_resolve_to_their_originals() {
    init_test_globals();
    let dir = std::env::temp_dir().join(format!("kolorinko_rsz_{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    let mut pub_builder = Pub::new(&dir, "rpcauthority");
    pub_builder.file(
        "http://rpcauthority.wikidot.com/local--files/foo/bar.png",
        HASH,
        "saved",
        "image/png",
        b"png",
    );
    let site_dir = pub_builder.write();
    let meta = block_on(read_repo_meta(&site("rpcauthority"), &site_dir)).unwrap();
    let variant = canon_file_key(
        "http://rpcauthority.wikidot.com/local--resized-images/foo/bar.png/small.png",
    )
    .unwrap();
    assert_eq!(
        resource(&meta, &site("rpcauthority"), &variant).map(|(h, _)| h),
        Some(HASH)
    );
}

/// The recorded `content_type` **is** the CA URL's extension, flattened
/// (`text/css` → `text.css` — the `/` can't live in a path segment), so any
/// type serves as itself with no format table to lag behind; non-statements
/// (`application/octet-stream` — wdfiles' answer for everything —
/// `text/plain`, XML serializations) and absent or malformed answers defer
/// to the URL's own extension.
#[test]
fn content_type_flattens_into_the_ca_extension() {
    let ca = |url_ext: &str, ct: Option<&str>| crate::assets::ca_ext(url_ext, ct);
    assert_eq!(ca("", Some("text/css")), "text.css");
    assert_eq!(ca("png", Some("image/webp")), "image.webp");
    assert_eq!(ca("jpg", Some("image/png; charset=binary")), "image.png");
    assert_eq!(ca("svg", Some("image/svg+xml")), "image.svg+xml");
    assert_eq!(ca("ttf", Some("font/ttf;unlikely;params")), "font.ttf");
    // Non-statements, absence, and malformed answers defer to the URL's
    // own extension.
    assert_eq!(ca("png", Some("application/octet-stream")), "png");
    assert_eq!(ca("svg", Some("text/xml")), "svg");
    assert_eq!(ca("webp", None), "webp");
    assert_eq!(ca("", Some("")), "");
    assert_eq!(ca("ttf", Some("malformed type")), "ttf");
    // Round-trip: the flattened extension names the recorded type back —
    // the `/` returns where the first `.` sat, and a subtype's own dots
    // survive it.
    assert_eq!(
        crate::assets::mime_for_ext(&ca("", Some("text/css"))),
        "text/css"
    );
    assert_eq!(
        crate::assets::mime_for_ext(&ca("png", Some("image/webp"))),
        "image/webp"
    );
    assert_eq!(
        crate::assets::mime_for_ext("application.vnd.ms-fontobject"),
        "application/vnd.ms-fontobject"
    );
    // Plain file extensions keep the static table.
    assert_eq!(crate::assets::mime_for_ext("png"), "image/png");
}

/// The publisher's canonicalization rules, verbatim from the format change:
/// every spelling of one file meets at one key, custom hosts stay as
/// written, garbage names no file.
#[test]
fn canon_file_key_matches_the_publisher_rules() {
    let p = |s: &str| canon_file_key(s).map(|p| p.as_str().to_owned());
    // The wikidot family (any mirrored site or a foreign sandbox) collapses
    // onto `<sub>.wikidot.com`, `www.` dropped, scheme dropped, `%3A`
    // decoded.
    assert_eq!(
        p("http://rpcsandbox.wdfiles.com/local--files/m/x.png").as_deref(),
        Some("rpcsandbox.wikidot.com/local--files/m/x.png")
    );
    assert_eq!(
        p("http://www.rpcauthority.wikidot.com/local--files/nav%3Aside/discord.png").as_deref(),
        Some("rpcauthority.wikidot.com/local--files/nav:side/discord.png")
    );
    assert_eq!(
        p("https://SCP-WIKI.Wikidot.Com/local--files/a%3ab/c%20d.png").as_deref(),
        Some("scp-wiki.wikidot.com/local--files/a:b/c d.png")
    );
    // An already-`host/path` tail (what `http_tail` yields) canonicalizes
    // identically.
    assert_eq!(
        p("rpcsandbox.wdfiles.com/local--files/m/x.png").as_deref(),
        Some("rpcsandbox.wikidot.com/local--files/m/x.png")
    );
    // Every other host — custom domains, CDNs — is preserved as written,
    // `www.` and `%3A` included; only the fragment drops.
    assert_eq!(
        p("https://www.rpc-wiki.net/local--files/forum/t-123/reply/32781.html#f").as_deref(),
        Some("www.rpc-wiki.net/local--files/forum/t-123/reply/32781.html")
    );
    assert_eq!(
        p("https://fonts.googleapis.com/css?family=Exo+2").as_deref(),
        Some("fonts.googleapis.com/css")
    );
    // The query drops (Wikidot serves `…?width=…` with the same bytes), `//`
    // collapses, and unusable shapes name no file.
    assert_eq!(
        p("http://s.wikidot.com/a.png?width=210&height=68").as_deref(),
        Some("s.wikidot.com/a.png")
    );
    assert_eq!(
        p("http://s.wikidot.com/local--files/widget-hub//x.png").as_deref(),
        Some("s.wikidot.com/local--files/widget-hub/x.png")
    );
    assert_eq!(p("http://s.wikidot.com/../etc/passwd"), None);
    assert_eq!(p("not a url"), None);
}

/// A rename keeps the numeric id but moves the slug index to the new name.
#[test]
fn rename_moves_slug_index() {
    let dir = std::env::temp_dir().join(format!("kolorinko_rename_{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    let mut first_pub = Pub::new(&dir, "scp");
    first_pub.page("1305054470", "foo", &[(1, "rid-1", 100, "Foo body")]);
    let site_dir = first_pub.write();
    let first = block_on(read_repo_meta(&site("scp"), &site_dir)).unwrap();
    assert!(article(&first, &root_slug("foo")).is_some());

    let mut second_pub = Pub::new(&dir, "scp");
    second_pub.page("1305054470", "renamed", &[(1, "rid-1", 100, "Foo body")]);
    second_pub.write();
    let second = block_on(read_repo_meta(&site("scp"), &site_dir)).unwrap();
    assert!(article(&second, &root_slug("foo")).is_none());
    assert_eq!(
        article(&second, &root_slug("renamed")).map(|p| p.id),
        Some(1305054470)
    );
}

fn key_at(site_name: &str, cat: Option<&str>, name: &str) -> Key {
    (
        site(site_name),
        cat.map(|c| SafePathComponent::new(c.into()).unwrap()),
        SafePathComponent::new(name.into()).unwrap(),
    )
}

fn key(cat: Option<&str>, name: &str) -> Key {
    key_at("scp", cat, name)
}

fn flat(content: &Content) -> String {
    let mut s = String::new();
    collect_plain(content, &mut s);
    s
}

fn raws(pairs: Vec<(Key, &str)>) -> HashMap<Key, Arc<str>> {
    pairs
        .into_iter()
        .map(|(k, body)| (k, Arc::from(body)))
        .collect()
}

/// One origin body textually assembled (vars substituted, includes spliced)
/// and parsed — the `article_latest` include half, minus the fetching.
fn assemble(origin: &str, raws: &HashMap<Key, Arc<str>>) -> Content {
    parse(&splice_includes(
        &subst_vars(origin, &[]),
        &site("scp"),
        raws,
        &[key(None, "root")],
    ))
}

#[test]
fn include_assembly_splices_nested_cone_with_cascading_vars() {
    // root includes b with x=1; b passes its {$x} through to c.
    let raws = raws(vec![
        (key(None, "b"), "B {$x}\n[[include c | y={$x}]]"),
        (key(None, "c"), "C({$y})"),
    ]);
    let out = assemble("[[include b | x=1]]", &raws);
    assert_eq!(flat(&out), "B 1\nC(1)");
}

#[test]
fn absolute_self_site_include_splices() {
    // `[[include :site:page]]` is absolute: the current site's own name is
    // dropped, the tail is one slug (names may contain colons — bau's
    // `fragment:theme:inverton`); another site's resolves against that
    // site's dataset (the cross-site tests below).
    let raws = raws(vec![
        (key(None, "footer"), "F"),
        (key(Some("fragment"), "theme:inverton"), "T"),
    ]);
    let out = assemble(
        "[[include :scp:footer]]\n[[include :scp:fragment:theme:inverton]]",
        &raws,
    );
    assert_eq!(flat(&out), "F\nT");
    let out = assemble("[[include :other-site:x]]", &raws);
    assert_eq!(flat(&out), ""); // other-site has no bodies here — unspliced
}

#[test]
fn cross_site_include_splices_from_the_named_site() {
    // `[[include :insurgency:theme:fasa]]` names the *insurgency* dataset:
    // the slug is that site's, category included — fasa's nav sidebar
    // pulling its theme page off another site.
    let raws = raws(vec![
        (
            key_at("insurgency", Some("theme"), "fasa"),
            "FASA({$accent})",
        ),
        (key_at("insurgency", Some("nav"), "side"), "SIDE"),
    ]);
    let out = assemble(
        "[[include :insurgency:theme:fasa | accent=blue]]\n[[include :insurgency:nav:side]]",
        &raws,
    );
    assert_eq!(flat(&out), "FASA(blue)\nSIDE");
}

#[test]
fn cross_site_include_fetches_the_named_site() {
    // The fetching half: fasa's nav sidebar on scp pulls its theme page off
    // the mirrored insurgency site in the same snapshot — spliced, and the
    // dep tree records the foreign page.
    let dir = std::env::temp_dir().join(format!("kolorinko_xinc_{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    let mut scp = Pub::new(&dir, "scp");
    scp.page(
        "1",
        "fasa:nav:side",
        &[(1, "r1", 100, "[[include :insurgency:theme:fasa]]\n")],
    );
    let scp_dir = scp.write();
    let mut insurgency = Pub::new(&dir, "insurgency");
    insurgency.page("2", "theme:fasa", &[(1, "r2", 200, "FASA")]);
    let insurgency_dir = insurgency.write();
    let mut metas = HashMap::new();
    metas.insert(
        site("scp"),
        block_on(read_repo_meta(&site("scp"), &scp_dir)).unwrap(),
    );
    metas.insert(
        site("insurgency"),
        block_on(read_repo_meta(&site("insurgency"), &insurgency_dir)).unwrap(),
    );
    // The gear's fetch path is the direct archive read; pin the fixture tree
    // so the source finds the published archives.
    let mut source = FixtureSource {
        root: dir.join("out"),
        metas,
    };
    let mut resolver = Resolver::new(site("scp"), &mut source);
    let (assembled, deps) = block_on(resolve_include(
        "[[include :insurgency:theme:fasa]]\n",
        &key(Some("fasa"), "nav:side"),
        &mut resolver,
    ));
    // The stored body keeps its trailing newline; the origin's own line
    // break follows the splice.
    assert_eq!(flat(&parse(&assembled)), "FASA\n\n");
    let [dep] = &deps[..] else {
        panic!("one dep: {deps:?}")
    };
    assert_eq!(dep.site, "insurgency");
    assert_eq!(dep.category.as_deref(), Some("theme"));
    assert_eq!(dep.page, "fasa");
}

#[test]
fn include_diamond_splices_target_in_both_branches() {
    let raws = raws(vec![
        (key(None, "b"), "B\n[[include d]]"),
        (key(None, "c"), "C\n[[include d]]"),
        (key(None, "d"), "D"),
    ]);
    let out = assemble("[[include b]]\n[[include c]]", &raws);
    assert_eq!(flat(&out), "B\nD\nC\nD");
}

#[test]
fn include_cycle_stops_at_the_back_edge() {
    // b includes c includes b: both splice once, the back-edge stays a
    // literal directive (render-degraded to text — the cycle leaves the
    // directive unspliced) and its var values erase to defaults.
    let raws = raws(vec![
        (key(None, "b"), "B0\n[[include c | z=9]]"),
        (key(None, "c"), "C0\n[[include b | z={$z}]]"),
    ]);
    let out = assemble("[[include b | z=7]]", &raws);
    assert_eq!(flat(&out), "B0\nC0\n");
    let back_edge = out.iter().any(|n| match n {
        Node::Raw(s) => s.contains("[[include b | z=9]]"),
        Node::Include(Include { source, .. }) if source.path == ["b".to_string()] => true,
        _ => false,
    });
    assert!(back_edge, "no literal back-edge: {:#?}", out);
}

#[test]
fn include_off_line_start_stays_unspliced() {
    // Wikidot's include rule is `^`-anchored: a directive with anything
    // before it on the line (a quote prefix, an attribute value) never
    // splices — documented samples stay literal markup.
    let raws = raws(vec![(key(None, "b"), "B")]);
    let out = assemble("> [[include b]]\n[[div]]prepend=\"[[include b]]\"", &raws);
    assert!(!flat(&out).contains('B'), "spliced: {:#?}", out);
    assert!(
        format!("{out:?}").contains("[[include b]]"),
        "directive not literal: {:#?}",
        out
    );
}

#[test]
fn include_splice_pairs_brackets_across_the_boundary() {
    // The component opens a div and a table cell it never closes; the
    // includer closes both after the include point — Wikidot assembles
    // includes textually before parsing, so the brackets pair across the
    // seam instead of degrading to raw markup on both halves.
    let raws = raws(vec![(
        key(None, "b"),
        "[[div class=\"box\"]]
[[table]]
[[row]]
[[cell style=\"padding: 3px\"]]
",
    )]);
    let out = assemble(
        "[[include b]]
cell body
[[/cell]][[/row]][[/table]][[/div]]",
        &raws,
    );
    let Node::Container {
        kind: ContainerKind::Div { .. },
        content,
        ..
    } = &out[0]
    else {
        panic!("expected the div to pair: {out:#?}")
    };
    let Node::BlockTable(t) = content
        .iter()
        .find(|n| matches!(n, Node::BlockTable(_)))
        .unwrap()
    else {
        unreachable!()
    };
    let cell = &t.rows[0].content;
    assert_eq!(flat(cell).trim(), "cell body");
}

#[test]
fn include_inside_code_block_stays_literal() {
    let raws = raws(vec![(key(None, "b"), "B")]);
    let out = assemble("[[code]][[include b]][[/code]]", &raws);
    let Node::Code { raw, .. } = &out[0] else {
        panic!("expected a code block: {out:#?}")
    };
    assert_eq!(raw, "[[include b]]");
}

#[test]
fn directive_past_unclosed_code_block_stays_live() {
    // An unclosed [[code]] degrades to raw text with the rest parsing
    // normally, so an include after one still splices.
    let raws = raws(vec![(key(None, "b"), "B")]);
    let out = assemble("[[code]]\ntail\n[[include b]]", &raws);
    // The unclosed opener itself renders as raw markup (invisible to the
    // text projection), the tail and the splice parse normally.
    assert_eq!(flat(&out), "\ntail\nB");
}

#[test]
fn dep_tree_nests_each_page_under_its_includer() {
    let (root, b, c, d) = (
        key(None, "root"),
        key(None, "b"),
        key(None, "c"),
        key(Some("nav"), "d"),
    );
    // root includes b and c; b includes d — discovery order.
    let edges = vec![
        (root.clone(), b.clone()),
        (root.clone(), c.clone()),
        (b.clone(), d.clone()),
    ];
    let deps = dep_tree(&root, edges);
    assert_eq!(
        deps,
        vec![
            PageDep {
                site: "scp".into(),
                category: None,
                page: "b".into(),
                deps: vec![PageDep {
                    site: "scp".into(),
                    category: Some("nav".into()),
                    page: "d".into(),
                    deps: vec![],
                }],
            },
            PageDep {
                site: "scp".into(),
                category: None,
                page: "c".into(),
                deps: vec![],
            },
        ]
    );
}

#[test]
fn include_var_resolves_to_value() {
    let vars = vec![("align".to_string(), "right".to_string())];
    assert_eq!(subst_vars("{$align}", &vars), "right");
}

#[test]
fn include_var_fallback_idiom_prefers_passed_value() {
    // `k={$k}|k=default`: a passed value shadows the literal default.
    let vars = vec![
        ("name".to_string(), "conspiracy.png".to_string()),
        ("name".to_string(), "unknown.png".to_string()),
    ];
    assert_eq!(subst_vars("{$name}", &vars), "conspiracy.png");
}

#[test]
fn include_var_fallback_idiom_uses_default_when_passthrough_empty() {
    // An empty passthrough (an unset `{$k}`) is skipped, so the literal
    // default is used — the fallback half of the idiom.
    let vars = vec![
        ("name".to_string(), String::new()),
        ("name".to_string(), "unknown.png".to_string()),
    ];
    assert_eq!(subst_vars("{$name}", &vars), "unknown.png");
}

#[test]
fn unresolved_include_var_uses_default() {
    assert_eq!(subst_vars("{$x//fallback}", &[]), "fallback");
}

#[test]
fn unresolved_include_var_without_default_vanishes() {
    assert_eq!(subst_vars("a{$x}b", &[]), "ab");
}

#[test]
fn newline_before_the_closing_brace_leaves_the_slot_literal() {
    // The lexer's own {$…} grammar: a slot runs to the line's end, so an
    // unclosed slot is plain text, not a variable.
    assert_eq!(subst_vars("a {$x\n} b", &[]), "a {$x\n} b");
}

#[test]
fn include_var_in_div_param_substitutes_before_parse() {
    // A value pasted into an attribute re-parses in place as literal text.
    let raws = raws(vec![(
        key(None, "b"),
        "[[div style=\"text-align: {$align}\"]]x[[/div]]",
    )]);
    let out = assemble("[[include b | align=right]]", &raws);
    let Node::Container {
        kind: ContainerKind::Div { params, .. },
        ..
    } = &out[0]
    else {
        panic!("expected a div: {out:#?}")
    };
    assert_eq!(
        params.get("style"),
        Some(&vec![TextObj::Plain("text-align: right".to_string())])
    );
}

#[test]
fn include_vars_outside_any_include_erase_to_defaults() {
    let out = assemble("a {$x//dflt} b {$y}", &HashMap::new());
    assert_eq!(flat(&out), "a dflt b ");
}

#[test]
fn include_vars_in_root_level_attributes_erase_to_defaults() {
    let out = assemble(
        "[[div style=\"color:{$c//red}\"]]x[[/div]]",
        &HashMap::new(),
    );
    let Node::Container {
        kind: ContainerKind::Div { params, .. },
        ..
    } = &out[0]
    else {
        panic!("expected div: {out:#?}")
    };
    assert_eq!(
        params.get("style"),
        Some(&vec![TextObj::Plain("color:red".to_string())])
    );
}

#[test]
fn external_refs_are_collected_and_content_addressed() {
    init_test_globals();
    let img_url = "https://scp.wikidot.com/local--files/foo/a.png";
    let hot = "https://i.imgur.com/x.jpg";
    let css = "a{background:url(https://scp.wikidot.com/local--files/foo/bg.png)}";
    let html = "<style>@import url('https://scp.wikidot.com/local--files/foo/a.css');</style>";
    let content: Content = vec![
        Node::Image {
            align: None,
            source: vec![TextObj::Plain(img_url.into())],
            params: Default::default(),
        },
        Node::Link {
            target: LinkTarget::Url(hot.into()),
            text: vec![],
            class: None,
            new_tab: false,
        },
        Node::Stylesheet(css.into()),
        Node::Html { raw: html.into() },
    ];
    // All four external refs are collected (the hotlink too, and the styles
    // a raw-HTML block embeds); resolution later keeps only the mirrored
    // ones.
    let mut tails = Vec::new();
    super::collect_external_refs(&content, &mut tails);
    assert_eq!(
        tails,
        vec![
            "scp.wikidot.com/local--files/foo/a.png".to_string(),
            "i.imgur.com/x.jpg".to_string(),
            "scp.wikidot.com/local--files/foo/bg.png".to_string(),
            "scp.wikidot.com/local--files/foo/a.css".to_string(),
        ]
    );
    // Only the mirrored scp tails resolve — through `resolve_tails`, the one
    // substitution decision; the hotlink doesn't.
    let h = "d84a29109fe0e70c7a5c22c39bda120fdbc56bd192f5927af95b9af8d0f87c27";
    let mirror = |path: &RepoAssetPath| match path.as_str() {
        "scp.wikidot.com/local--files/foo/a.png" | "scp.wikidot.com/local--files/foo/bg.png" => {
            Some(format!("/-/repo/scp/files/d8/4a/{h}.png"))
        }
        "scp.wikidot.com/local--files/foo/a.css" => {
            Some(format!("/-/repo/scp/files/d8/4a/{h}.text.css"))
        }
        _ => None,
    };
    let resolved = super::resolve_tails(&tails, mirror);
    assert!(!resolved.contains_key("i.imgur.com/x.jpg"));
    let out = super::substitute_resources(content, &resolved);
    // Image source → CA url.
    let Node::Image { source, .. } = &out[0] else {
        panic!("expected image")
    };
    assert_eq!(
            source,
            &vec![TextObj::Plain(
                "/-/repo/scp/files/d8/4a/d84a29109fe0e70c7a5c22c39bda120fdbc56bd192f5927af95b9af8d0f87c27.png".into()
            )]
        );
    // Hotlink left untouched.
    let Node::Link {
        target: LinkTarget::Url(u),
        ..
    } = &out[1]
    else {
        panic!("expected url link")
    };
    assert_eq!(u, hot);
    // Stylesheet url() → CA url.
    let Node::Stylesheet(css) = &out[2] else {
        panic!("expected stylesheet")
    };
    assert!(css.contains("/-/repo/scp/files/d8/4a/"));
    assert!(!css.contains("https://scp.wikidot.com"));
    // The same textual rewriter covers the styles a raw-HTML block embeds.
    let Node::Html { raw } = &out[3] else {
        panic!("expected html block")
    };
    assert!(raw.contains("/-/repo/scp/files/d8/4a/"));
    assert!(!raw.contains("https://scp.wikidot.com"));
}

/// A served CSS blob's relative refs — the backrooms theme's
/// `liminal-impact.css` is nothing but `@import url("./liminal.css")` —
/// resolve against the blob's *original* URL (the reverse index's path) and
/// localize to CA URLs, instead of breaking against the `/-/repo/…` path
/// the blob is actually served from.
#[test]
fn relative_css_refs_localize_against_the_original_url() {
    init_test_globals();
    let dir = std::env::temp_dir().join(format!("kolorinko_relcss_{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let css = "@import url(\"./liminal.css\");\n@import \"./sidebar.css\";\n";
    let mut pub_builder = Pub::new(&dir, "backrooms-wiki-cn");
    pub_builder
        .file(
            "https://github.backroomswiki.cn/Old_BHL/css/liminal-impact.css",
            HASH,
            "saved",
            "text/css",
            css.as_bytes(),
        )
        .file(
            "https://github.backroomswiki.cn/Old_BHL/css/liminal.css",
            HASH2,
            "saved",
            "text/css",
            b"liminal",
        )
        .file(
            "https://github.backroomswiki.cn/Old_BHL/css/sidebar.css",
            HASH3,
            "saved",
            "text/css",
            b"sidebar",
        );
    let site = site("backrooms-wiki-cn");
    let site_dir = pub_builder.write();
    let meta = block_on(read_repo_meta(&site, &site_dir)).unwrap();
    let out = super::assets_gear::localize_css(
        css,
        "http://github.backroomswiki.cn/Old_BHL/css/liminal-impact.css",
        &site,
        &meta,
    );
    let ca = |h: &str| {
        format!(
            "/-/repo/backrooms-wiki-cn/files/{}/{}/{}.text.css",
            &h[..2],
            &h[2..4],
            h
        )
    };
    assert_eq!(
        out,
        format!(
            "@import url(\"{}\");\n@import url(\"{}\");\n",
            ca(HASH2),
            ca(HASH3)
        )
    );
}

#[test]
fn parse_shell_reads_title_subtitle_and_theme_root() {
    let text = "\
title: \"RPC Authority\"
subtitle: \"Research, Protection, Containment\"
theme_root: https://cdn.jsdelivr.net/gh/x/y@main/style.css
";
    let chrome = super::parse_shell(text);
    assert_eq!(chrome.title.as_deref(), Some("RPC Authority"));
    assert_eq!(
        chrome.subtitle.as_deref(),
        Some("Research, Protection, Containment")
    );
    let theme_root = chrome.theme_root.expect("theme_root");
    assert_eq!(
        theme_root.as_str(),
        "cdn.jsdelivr.net/gh/x/y@main/style.css"
    );
    // The publisher writes the raw archived URL — the `wdfiles` spelling and
    // `%3A` escapes canonicalize to the same key the `files/` index builds.
    let chrome = super::parse_shell(
        "theme_root: http://obscurative.wdfiles.com/local--theme/obskura-h/style.css\n",
    );
    assert_eq!(
        chrome.theme_root.expect("url theme_root").as_str(),
        "obscurative.wikidot.com/local--theme/obskura-h/style.css"
    );
    let chrome = super::parse_shell(
        "theme_root: https://scp-wiki.wdfiles.com/local--code/component%3Atheme/1\n",
    );
    assert_eq!(
        chrome.theme_root.expect("escaped theme_root").as_str(),
        "scp-wiki.wikidot.com/local--code/component:theme/1"
    );
}

#[test]
fn parse_shell_reads_landing_with_start_default() {
    // A shell-carried landing: a quoted `cat:name` slug, category included
    // (srddf names `deleted:blog:_start`); or a bare name.
    assert_eq!(
        super::parse_shell("landing: \"deleted:blog:_start\"\n").landing,
        kolorinko_rt::parse_slug("deleted:blog:_start").unwrap()
    );
    assert_eq!(
        super::parse_shell("landing: \"main\"\n").landing,
        kolorinko_rt::parse_slug("main").unwrap()
    );
    // No landing line — and an unparsable one — fall back to `start`.
    assert_eq!(
        super::parse_shell("title: \"T\"\n").landing,
        kolorinko_rt::start_slug()
    );
    assert_eq!(
        super::parse_shell("landing: \"../escape\"\n").landing,
        kolorinko_rt::start_slug()
    );
}

#[test]
fn parse_shell_missing_or_hostless_theme_root_is_none() {
    // No theme_root line.
    let chrome = super::parse_shell("title: \"T\"\n");
    assert_eq!(chrome.title.as_deref(), Some("T"));
    assert!(chrome.subtitle.is_none());
    assert!(chrome.theme_root.is_none());
    // A site-relative CSS path has no host — no mirrored identity.
    let chrome = super::parse_shell("theme_root: /local--code/component:theme\n");
    assert!(chrome.theme_root.is_none());
}

/// Against a real evakuilo publication: the `files` section must map the
/// theme-root tail to a full 64-char sha256 (with a real blob behind it in
/// `files_ca/`), the `shell` must parse (title/subtitle/theme_root), and the
/// theme root must resolve through that index. Skipped when the publication
/// isn't checked out.
#[test]
fn real_publication_indexes_files_and_shell() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../../wikidot-evakuilo/data/kolorinko/out/rpcauthority");
    if !root.join("HEAD").exists() {
        eprintln!("skipping: real publication not present");
        return;
    }
    let meta = block_on(read_repo_meta(&site("rpcauthority"), &root)).expect("publication reads");
    assert!(!meta.pages.is_empty(), "pages indexed");
    assert_eq!(meta.title.as_deref(), Some("RPC Authority"));
    assert!(meta.subtitle.as_ref().is_some_and(|s| !s.is_empty()));
    // The publication shell names rpc's landing explicitly.
    assert_eq!(meta.landing, kolorinko_rt::parse_slug("start").unwrap());
    let theme_path = meta.theme_root.clone().expect("theme_root parsed");
    // The files index resolves the theme path to a full 64-char sha256 —
    // NOT the 60-char sharded leaf (the bug this guards against) — and the
    // reverse half names its recorded entry.
    let (hash, file) =
        resource(&meta, &site("rpcauthority"), &theme_path).expect("theme in files index");
    assert!(!crate::assets::ca_file_ext(file).is_empty());
    assert_eq!(hash.len(), 64);
    assert!(hash.bytes().all(|b| b.is_ascii_hexdigit()));
    // The hash must locate the real blob: the on-disk leaf is `hash[4..]`
    // (the rest), not the full hash.
    let blob = root
        .join("files_ca")
        .join(&hash[..2])
        .join(&hash[2..4])
        .join(&hash[4..]);
    assert!(blob.exists(), "blob {blob:?} should exist");
    // And ca_url embeds the full hash under the matching shards.
    let url = super::ca_url(&site("rpcauthority"), hash, file);
    let prefix = format!("/-/repo/rpcauthority/files/{}/{}", &hash[..2], &hash[2..4]);
    assert!(
        url.starts_with(&prefix),
        "url {url} should start with {prefix}"
    );
}

/// The review regression, against real data: the `shell` names the theme as
/// a raw URL and the `files` section keys it by that original URL — the
/// shell tail must resolve through `resource` to a real blob.
#[test]
fn real_publication_resolves_site_theme_root() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../../wikidot-evakuilo/data/kolorinko/out/castopia.site");
    if !root.join("HEAD").exists() {
        eprintln!("skipping: real publication not present");
        return;
    }
    let site = site("castopia.site");
    let meta = block_on(read_repo_meta(&site, &root)).expect("publication reads");
    let theme = meta.theme_root.clone().expect("theme_root parsed");
    let (_hash, file) = resource(&meta, &site, &theme).expect("theme resolves through the index");
    assert_eq!(crate::assets::ca_file_ext(file), "text.css");
}

/// The reported regression, against real data: a served theme blob whose
/// `url()`s are *relative* (`/local--files/…`, the shape the castopia theme
/// uses) must resolve them against its original URL (the reverse index's
/// path) and localize them to CA URLs — not break against the `/-/repo/…`
/// path it is served from. Skipped when the publication isn't checked out.
#[test]
fn real_publication_localizes_relative_theme_imports() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../../wikidot-evakuilo/data/kolorinko/out/castopia.site");
    if !root.join("HEAD").exists() {
        eprintln!("skipping: real publication not present");
        return;
    }
    let site = site("castopia.site");
    let meta = block_on(read_repo_meta(&site, &root)).expect("publication reads");
    let key = canon_file_key("https://castopia.site/-/static/scp-base.css").unwrap();
    let (hash, file) = resource(&meta, &site, &key).expect("theme row in the index");
    assert_eq!(file.content_type.as_deref(), Some("text/css"));
    // The real blob (raw on disk, sharded rest-leaf), localized exactly the
    // way the `asset` gear serves it.
    let blob = std::fs::read(
        root.join("files_ca")
            .join(&hash[..2])
            .join(&hash[2..4])
            .join(&hash[4..]),
    )
    .expect("theme blob");
    let text = std::str::from_utf8(&blob).expect("utf8 css");
    assert!(
        text.contains("/local--files/"),
        "fixture drifted: no relative ref in the theme"
    );
    let out = super::assets_gear::localize_css(
        text,
        &format!("http://{}", file.path.as_str()),
        &site,
        &meta,
    );
    assert!(
        !out.contains("/local--files/"),
        "a relative ref survived: {out}"
    );
    for line in out.lines().filter(|l| l.contains("url(")) {
        assert!(
            line.contains("/-/repo/castopia.site/files/") || line.contains("https://"),
            "ref not localized: {line}"
        );
    }
}

/// Globals for host-matching tests: the dev config's two sites. `init` is
/// first-write-wins (`OnceLock`), so a second call is a no-op — every test
/// must tolerate this one registry.
fn init_test_globals() {
    let mut sites = indexmap::IndexMap::new();
    sites.insert(
        "obscurative".to_string(),
        crate::globals::SiteCfg {
            domains: vec!["www.obscurative.ru".into()],
        },
    );
    sites.insert(
        "rpcauthority".to_string(),
        crate::globals::SiteCfg {
            domains: vec!["rpc-wiki.net".into()],
        },
    );
    let _ = crate::globals::init(".", 0, &sites);
}

#[test]
fn code_urls_rewrite_to_slug_family_routes() {
    init_test_globals();
    let sp = crate::globals::evakuilo_space_id("rpcauthority");
    let f = |t: &str| super::code_url_for_tail(t);
    // The wikidot.com form, the configured alias domain (the corpus's www
    // variant of a bare config entry), and the wdfiles `local--code` 302
    // target with its percent-encoded page — all one local code route.
    assert_eq!(
        f("rpcauthority.wikidot.com/component:research-style/code/1"),
        Some(format!("/{sp}/component:research-style/code/1"))
    );
    assert_eq!(
        f("www.rpc-wiki.net/component:research-style/code/1"),
        Some(format!("/{sp}/component:research-style/code/1"))
    );
    assert_eq!(
        f("rpc-wiki.net.wdfiles.com/local--code/component%3Aresearch-style/1"),
        Some(format!("/{sp}/component:research-style/code/1"))
    );
    // A bare-name page and a block number other than 1.
    assert_eq!(
        f("rpc-wiki.net/foo/code/2"),
        Some(format!("/{sp}/foo/code/2"))
    );
    // Unregistered site, multi-segment page, bad N, non-code shape: all stay
    // hotlinks.
    assert_eq!(f("rpcsandbox.wikidot.com/foo/code/1"), None);
    assert_eq!(f("rpcauthority.wikidot.com/forum/t-123/code/1"), None);
    assert_eq!(f("rpcauthority.wikidot.com/component:theme/code/x"), None);
    assert_eq!(f("i.imgur.com/x.jpg"), None);
}

#[test]
fn code_endpoint_imports_fall_back_to_local_routes() {
    init_test_globals();
    let sp = crate::globals::evakuilo_space_id("rpcauthority");
    let tail = "www.rpc-wiki.net/component:theme/code/1";
    let content: Content = vec![Node::Stylesheet(
        format!("@import url(http://{tail});").into(),
    )];
    // Nothing is mirrored — `resolve_tails` falls back to the code route.
    let resolved = super::resolve_tails(&[tail.to_string()], |_| None);
    let out = super::substitute_resources(content, &resolved);
    let Node::Stylesheet(rewritten) = &out[0] else {
        panic!("expected stylesheet")
    };
    // `rewrite_with` re-emits a `url()` whose replacement contains `:` as a
    // quoted string (valid CSS; CA URLs never carry one, code routes do).
    assert_eq!(
        rewritten,
        &format!("@import url(\"/{sp}/component:theme/code/1\");")
    );
}

#[test]
fn page_refs_are_collected_and_query_canonicalized() {
    let page = |space: Option<&str>, path: &[&str]| {
        LinkTarget::Page(PageRef {
            space: space.map(str::to_string),
            path: path.iter().map(|s| (*s).to_string()).collect(),
        })
    };
    let content: Content = vec![
        Node::Link {
            target: page(None, &["index"]),
            text: vec![],
            class: None,
            new_tab: false,
        },
        // Nested inside a container — the walk reaches it.
        Node::Container {
            kind: ContainerKind::Div {
                inline: false,
                block: true,
                params: Default::default(),
            },
            content: vec![Node::Link {
                target: page(Some("database"), &["vika-owl"]),
                text: vec![],
                class: None,
                new_tab: false,
            }],
        },
        // Duplicate: deduplicated.
        Node::Link {
            target: page(None, &["index"]),
            text: vec![],
            class: None,
            new_tab: false,
        },
        // Empty ref and a `/`-bearing name: not slugs, never queried.
        Node::Link {
            target: page(None, &[]),
            text: vec![],
            class: None,
            new_tab: false,
        },
        Node::Link {
            target: page(None, &["forum/t-1"]),
            text: vec![],
            class: None,
            new_tab: false,
        },
        // External URL: untouched by this pass.
        Node::Link {
            target: LinkTarget::Url("https://x.example".into()),
            text: vec![],
            class: None,
            new_tab: false,
        },
    ];
    let mut slugs = Vec::new();
    super::collect_page_refs(&content, &mut slugs);
    assert_eq!(
        slugs,
        vec![
            root_slug("index"),
            (Some(site("database")), site("vika-owl")),
        ]
    );
    // The id form is the sorted, deduplicated set…
    let query = super::canonical_query(slugs);
    assert_eq!(
        query.0,
        vec![
            root_slug("index"),
            (Some(site("database")), site("vika-owl")),
        ]
    );
    // …so any reshuffling of the same set is the same id.
    let reshuffled = vec![
        root_slug("index"),
        (Some(site("database")), site("vika-owl")),
        root_slug("index"),
    ];
    assert_eq!(super::canonical_query(reshuffled), query);
}

#[test]
fn link_substitution_rewrites_hits_and_keeps_misses() {
    let page = |space: Option<&str>, name: &str| {
        LinkTarget::Page(PageRef {
            space: space.map(str::to_string),
            path: vec![name.to_string()],
        })
    };
    let content: Content = vec![
        Node::Link {
            target: page(None, "index"),
            text: vec![],
            class: None,
            new_tab: false,
        },
        Node::Link {
            target: page(None, "missing"),
            text: vec![],
            class: None,
            new_tab: false,
        },
        // A site-root ref: unclassifiable, never touched (never a red link).
        Node::Link {
            target: LinkTarget::Page(PageRef {
                space: None,
                path: vec![],
            }),
            text: vec![],
            class: None,
            new_tab: false,
        },
        Node::Container {
            kind: ContainerKind::Div {
                inline: false,
                block: true,
                params: Default::default(),
            },
            content: vec![Node::Link {
                target: page(Some("database"), "vika-owl"),
                text: vec![],
                class: None,
                new_tab: false,
            }],
        },
    ];
    let resolved = HashMap::from([
        (
            root_slug("index"),
            (LocalId::new(986050317), "Index".to_string()),
        ),
        (
            (Some(site("database")), site("vika-owl")),
            (LocalId::new(1305054470), "Вика-Сова".to_string()),
        ),
    ]);
    let out = super::substitute_links(content, &resolved);
    // Hits become canonical refs carrying the rename-stable page id + title.
    let Node::Link {
        target: LinkTarget::Canonical { page_id, title },
        ..
    } = &out[0]
    else {
        panic!("expected a canonical link")
    };
    assert_eq!((page_id.as_str(), title.as_str()), ("986050317", "Index"));
    let Node::Container {
        content: nested, ..
    } = &out[3]
    else {
        panic!("expected the container")
    };
    let Node::Link {
        target: LinkTarget::Canonical { page_id, title },
        ..
    } = &nested[0]
    else {
        panic!("expected the nested hit")
    };
    assert_eq!(
        (page_id.as_str(), title.as_str()),
        ("1305054470", "Вика-Сова")
    );
    // The miss becomes `Missing` — the renderer's red `newpage` link.
    let Node::Link {
        target: LinkTarget::Missing(p),
        ..
    } = &out[1]
    else {
        panic!("expected the miss to become a red link")
    };
    assert_eq!(p.path, vec!["missing".to_string()]);
    // The unclassifiable root ref stays `Page` verbatim.
    let Node::Link {
        target: LinkTarget::Page(p),
        ..
    } = &out[2]
    else {
        panic!("expected the root ref to stay a page ref")
    };
    assert!(p.path.is_empty());
}
