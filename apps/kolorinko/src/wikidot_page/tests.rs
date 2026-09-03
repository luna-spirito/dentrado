use super::*;
use std::fs;

/// Write one page's tar+zst archive into the publication layout under
/// `<root>/out/<site>/pages_by_id/…`, mirroring the publisher's deterministic
/// pack (v1 frontmatter per `rNNN.txt` entry). `revs` is
/// `(rev_no, rev_id, timestamp, body)`.
fn write_page_archive(
    root: &Path,
    site: &str,
    id: &str,
    slug: &str,
    revs: &[(u64, &str, i64, &str)],
) {
    let dest = root.join("out").join(site).join(archive_rel(id));
    fs::create_dir_all(dest.parent().unwrap()).unwrap();
    let mut tarb = tar::Builder::new(Vec::new());
    for (no, rid, ts, body) in revs {
        let text = format!(
            "---\n\
             title: \"T {slug}\"\n\
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
    let tar_bytes = tarb.into_inner().unwrap();
    fs::write(&dest, zstd::stream::encode_all(&tar_bytes[..], 3).unwrap()).unwrap();
}

/// (Re)write the site's `pages.json` from `(id, slug, stored, max_rev)` rows.
fn write_manifest(root: &Path, site: &str, rows: &[(&str, &str, i64, i64)]) {
    let site_out = root.join("out").join(site);
    fs::create_dir_all(&site_out).unwrap();
    let pages: Vec<_> = rows
        .iter()
        .map(|(id, slug, stored, max_rev)| {
            serde_json::json!({
                "id": id,
                "slug": slug,
                "title": format!("T {slug}"),
                "tags": [],
                "revisions_stored": stored,
                "revisions_known": stored,
                "max_rev": max_rev,
                "archive": archive_rel(id),
            })
        })
        .collect();
    let doc = serde_json::json!({ "site": site, "pages": pages });
    fs::write(
        site_out.join("pages.json"),
        serde_json::to_vec_pretty(&doc).unwrap(),
    )
    .unwrap();
}

/// Write the site's `files.json` from `(url, sha256, status)` rows and lay
/// down each `saved` blob's bytes under `files_ca/`.
fn write_files(root: &Path, site: &str, rows: &[(&str, &str, &str, &[u8])]) {
    let site_out = root.join("out").join(site);
    fs::create_dir_all(&site_out).unwrap();
    let files: Vec<_> = rows
        .iter()
        .map(|(url, sha, status, _)| {
            serde_json::json!({
                "path": url,
                "sha256": sha,
                "size": 1,
                "content_type": "text/css",
                "status": status,
                "blob": format!("files_ca/{}/{}/{}", &sha[..2], &sha[2..4], &sha[4..]),
            })
        })
        .collect();
    let doc = serde_json::json!({ "site": site, "files": files });
    fs::write(
        site_out.join("files.json"),
        serde_json::to_vec_pretty(&doc).unwrap(),
    )
    .unwrap();
    for (url, sha, status, bytes) in rows {
        if *status != "saved" {
            continue;
        }
        let blob = site_out
            .join("files_ca")
            .join(&sha[..2])
            .join(&sha[2..4])
            .join(&sha[4..]);
        let _ = url;
        fs::create_dir_all(blob.parent().unwrap()).unwrap();
        fs::write(&blob, bytes).unwrap();
    }
}

fn site(s: &str) -> SafePathComponent {
    SafePathComponent::new(s.into()).unwrap()
}

fn root_slug(name: &str) -> Slug {
    (None, SafePathComponent::new(name.into()).unwrap())
}

fn site_map(w: WDWebsite) -> ImHashMap<SafePathComponent, WDWebsite> {
    let mut sites: ImHashMap<SafePathComponent, WDWebsite> = ImHashMap::new();
    sites.insert(site("scp"), w);
    sites
}

fn site_map_at(site: SafePathComponent, w: WDWebsite) -> ImHashMap<SafePathComponent, WDWebsite> {
    let mut sites: ImHashMap<SafePathComponent, WDWebsite> = ImHashMap::new();
    sites.insert(site, w);
    sites
}

/// A 64-hex sha256 stand-in (of the literal bytes "css").
const HASH: &str = "d1f69a9854765a4f1e7c8b1e8a9e5c9bd1e0a2f3c4b5a6978899aabbccddeeff";

/// Regression: `repo()`'s cold start spawns the worker and re-borrows the
/// cache `RefCell` in its `None` arm. A `borrow()` left in the `match`
/// scrutinee used to live through the arms and panic ("RefCell already
/// borrowed"). Pointing at an unreachable source keeps the worker
/// publication-less (empty dataset) while still exercising that path, and
/// proves the unchanged-publication path keeps the same `Rc`.
#[test]
fn repo_cold_start_reborrows_without_panic() {
    use compio::runtime::Runtime;
    let dir = std::env::temp_dir().join(format!("kolorinko_repo_nopath_{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    let path: &'static Path = Box::leak(dir.clone().into_boxed_path());
    let meta = OutMeta::new(path, 900);
    let mut cache = RepoCache::default();
    let rt = Runtime::new().unwrap();
    // Cold start (non-tick): the `None` arm — the original panic site.
    let first = rt.block_on(repo(&meta, false, &mut cache));
    assert!(find_article(&first.sites, &site("nope"), &root_slug("nope")).is_none());
    // Nothing on disk → worker returns None → the prior `Rc` is kept.
    let second = rt.block_on(repo(&meta, true, &mut cache));
    assert!(Rc::ptr_eq(&first, &second));
}

/// A one-page publication built the way the worker's first scan builds it.
fn one_page_publication(dir: &Path) {
    write_page_archive(
        dir,
        "scp",
        "1305054470",
        "foo",
        &[(1, "rid-1", 100, "Foo body")],
    );
    write_manifest(dir, "scp", &[("1305054470", "foo", 1, 1)]);
}

#[test]
fn build_reads_publication_and_materialises_bodies() {
    let dir = std::env::temp_dir().join(format!("kolorinko_out_{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();

    write_page_archive(
        &dir,
        "scp",
        "1305054470",
        "foo",
        &[(1, "rid-1", 100, "Foo body")],
    );
    write_page_archive(
        &dir,
        "scp",
        "1305054471",
        "bar",
        &[(1, "rid-2", 101, "Bar body"), (2, "rid-3", 102, "Bar v2")],
    );
    write_manifest(
        &dir,
        "scp",
        &[("1305054470", "foo", 1, 1), ("1305054471", "bar", 2, 2)],
    );
    write_files(
        &dir,
        "scp",
        &[(
            "https://scp.wikidot.com/local--files/foo/a.css",
            HASH,
            "saved",
            b"css",
        )],
    );

    let mut bodies = ImHashMap::new();
    let (rows, w) = build_site(&dir.join("out").join("scp"), &mut bodies).unwrap();
    let sites = site_map(w);

    // Both pages indexed, both latest bodies materialised into the
    // snapshot's store (frontmatter stripped, highest revision wins).
    assert_eq!(rows.len(), 2);
    assert_eq!(bodies.len(), 2);
    let foo = find_article(&sites, &site("scp"), &root_slug("foo")).unwrap();
    assert_eq!(&**bodies.get(&foo.latest_body).unwrap(), "Foo body\n");
    assert_eq!(foo.meta.slug, "foo");
    assert_eq!(foo.meta.page_id, "1305054470");
    assert_eq!(foo.meta.title, "T foo");
    // The revision table comes from the archive entries' frontmatter.
    assert_eq!(foo.revisions.len(), 1);
    assert_eq!(foo.revisions[0].revision, 1);
    assert_eq!(foo.revisions[0].revision_id, "rid-1");
    assert_eq!(foo.revisions[0].timestamp, 100);
    assert_eq!(foo.revisions[0].author, "7");
    let bar = find_article(&sites, &site("scp"), &root_slug("bar")).unwrap();
    assert_eq!(&**bodies.get(&bar.latest_body).unwrap(), "Bar v2\n");
    assert_eq!(bar.revisions.len(), 2);
    // Canonical addressing: page id → current slug.
    assert_eq!(
        sites.get(&site("scp")).unwrap().by_page_id.get(&1305054470),
        Some(&(None, SafePathComponent::new("foo".into()).unwrap()))
    );
    // The files index maps the percent-decoded host/path tail to the CA ref.
    let w = sites.get(&site("scp")).unwrap();
    let ca = w
        .files
        .get(&RepoAssetPath::new("scp.wikidot.com/local--files/foo/a.css".into()).unwrap())
        .expect("file indexed");
    assert_eq!(ca.hash, HASH);
    assert_eq!(ca.ext, "css");
}

/// On-site files are keyed site-relative in `files.json` (the DB's path
/// form), while lookups name `host/path` tails: `resource` must retry the
/// bare relative key for the site's own hosts — and only those.
#[test]
fn resource_retries_site_relative_rows_for_own_hosts() {
    init_test_globals();
    let dir = std::env::temp_dir().join(format!("kolorinko_rel_{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();

    write_page_archive(
        &dir,
        "obscurative",
        "1305054470",
        "foo",
        &[(1, "rid-1", 100, "Foo body")],
    );
    write_manifest(&dir, "obscurative", &[("1305054470", "foo", 1, 1)]);
    write_files(
        &dir,
        "obscurative",
        &[("local--theme/t/style.css", HASH, "saved", b"css")],
    );

    let mut bodies = ImHashMap::new();
    let (_, w) = build_site(&dir.join("out").join("obscurative"), &mut bodies).unwrap();
    let snap = RepoSnapshot {
        sites: site_map_at(site("obscurative"), w),
        bodies,
    };
    // The relative row is indexed verbatim, under no host at all.
    let rel = RepoAssetPath::new("local--theme/t/style.css".into()).unwrap();
    assert!(
        snap.sites
            .get(&site("obscurative"))
            .unwrap()
            .files
            .contains_key(&rel)
    );
    // Every own-host form resolves through the relative retry.
    for host in [
        "obscurative.wikidot.com",
        "obscurative.wdfiles.com",
        "WWW.OBSCURATIVE.RU",
        "files.www.obscurative.ru",
    ] {
        let tail = RepoAssetPath::new(format!("{host}/local--theme/t/style.css")).unwrap();
        let ca = resource(&snap, &site("obscurative"), &tail);
        assert_eq!(ca.map(|c| c.hash).as_deref(), Some(HASH), "{host}");
    }
    // A foreign host with the same path stays a hotlink.
    let foreign = RepoAssetPath::new("i.imgur.com/local--theme/t/style.css".into()).unwrap();
    assert!(resource(&snap, &site("obscurative"), &foreign).is_none());
}

#[test]
fn incremental_patch_on_manifest_drift() {
    let dir = std::env::temp_dir().join(format!("kolorinko_inc_{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let site_dir = || dir.join("out").join("scp");

    write_page_archive(
        &dir,
        "scp",
        "1305054470",
        "foo",
        &[(1, "rid-1", 100, "Foo v1")],
    );
    write_page_archive(
        &dir,
        "scp",
        "1305054471",
        "bar",
        &[(1, "rid-2", 101, "Bar v1")],
    );
    write_manifest(
        &dir,
        "scp",
        &[("1305054470", "foo", 1, 1), ("1305054471", "bar", 1, 1)],
    );

    let mut bodies = ImHashMap::new();
    let (old_rows, w) = build_site(&site_dir(), &mut bodies).unwrap();
    let sites = site_map(w);
    let bar_body = Rc::new(
        find_article(&sites, &site("scp"), &root_slug("bar"))
            .unwrap()
            .clone(),
    );
    let bar_arc = bodies.get(&bar_body.latest_body).unwrap().clone();

    // Edit only `foo` (new revision → new archive bytes + drifted row) and
    // republish the manifest.
    write_page_archive(
        &dir,
        "scp",
        "1305054470",
        "foo",
        &[(1, "rid-1", 100, "Foo v1"), (2, "rid-3", 103, "Foo v2")],
    );
    write_manifest(
        &dir,
        "scp",
        &[("1305054470", "foo", 2, 2), ("1305054471", "bar", 1, 1)],
    );
    let fresh = read_pages_manifest(&site_dir()).unwrap();
    let mut w = sites.get(&site("scp")).cloned().unwrap();
    patch_pages(&mut w, &site_dir(), &old_rows, &fresh, &mut bodies);
    let next = site_map(w);

    // `bar` is structurally shared from the old snapshot (same body `Arc`);
    // `foo` re-read with its new latest body.
    let foo = find_article(&next, &site("scp"), &root_slug("foo")).unwrap();
    assert_eq!(&**bodies.get(&foo.latest_body).unwrap(), "Foo v2\n");
    assert_eq!(foo.revisions.len(), 2);
    let bar = find_article(&next, &site("scp"), &root_slug("bar")).unwrap();
    assert_eq!(bar.latest_body, bar_body.latest_body);
    assert!(Arc::ptr_eq(bodies.get(&bar.latest_body).unwrap(), &bar_arc));
}

/// The worker's stamp gate end-to-end: a first tick builds, an unchanged
/// publication yields `None`, a republished page re-reads, and a rename
/// moves the page (and its `by_page_id` entry) to the new slug.
#[test]
fn worker_tick_rebuilds_only_on_drift() {
    let dir = std::env::temp_dir().join(format!("kolorinko_tick_{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    // The fixtures live under `<dir>/out/`; the worker takes the publication
    // root itself, as the config's `evakuilo.dir` names it.
    let path: &'static Path = Box::leak(dir.join("out").into_boxed_path());

    one_page_publication(&dir);
    let mut worker = OutWorker::new(path);
    let first = worker.tick().expect("first tick builds");
    let foo = find_article(&first.sites, &site("scp"), &root_slug("foo")).unwrap();
    assert_eq!(&**first.bodies.get(&foo.latest_body).unwrap(), "Foo body\n");

    // Unchanged publication → None (the caller keeps its prior `Rc`).
    assert!(worker.tick().is_none());

    // A drifted archive + manifest row → re-read, and the stale body is
    // pruned from the store.
    write_page_archive(
        &dir,
        "scp",
        "1305054470",
        "foo",
        &[(1, "rid-1", 100, "Foo body"), (2, "rid-9", 109, "Foo v9")],
    );
    write_manifest(&dir, "scp", &[("1305054470", "foo", 2, 2)]);
    let second = worker.tick().expect("drift rebuilds");
    let foo = find_article(&second.sites, &site("scp"), &root_slug("foo")).unwrap();
    assert_eq!(&**second.bodies.get(&foo.latest_body).unwrap(), "Foo v9\n");
    assert_eq!(second.bodies.len(), 1, "stale body pruned");

    // A rename: same id, new slug — the old slug entry and `by_page_id`
    // mapping move together.
    write_page_archive(
        &dir,
        "scp",
        "1305054470",
        "renamed",
        &[(1, "rid-1", 100, "Foo body"), (2, "rid-9", 109, "Foo v9")],
    );
    write_manifest(&dir, "scp", &[("1305054470", "renamed", 2, 2)]);
    let third = worker.tick().expect("rename rebuilds");
    assert!(
        find_article(&third.sites, &site("scp"), &root_slug("foo")).is_none(),
        "old slug vacated"
    );
    let renamed =
        find_article(&third.sites, &site("scp"), &root_slug("renamed")).expect("new slug indexed");
    assert_eq!(renamed.meta.page_id, "1305054470");
    assert_eq!(
        third
            .sites
            .get(&site("scp"))
            .unwrap()
            .by_page_id
            .get(&1305054470),
        Some(&(None, SafePathComponent::new("renamed".into()).unwrap()))
    );
}

fn key(cat: Option<&str>, name: &str) -> Key {
    (
        site("scp"),
        cat.map(|c| SafePathComponent::new(c.into()).unwrap()),
        SafePathComponent::new(name.into()).unwrap(),
    )
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
    let site = SafePathComponent::new("scp".into()).unwrap();
    let img_url = "https://scp.wikidot.com/local--files/foo/a.png";
    let hot = "https://i.imgur.com/x.jpg";
    let css = "a{background:url(https://scp.wikidot.com/local--files/foo/bg.png)}";
    let content: Content = vec![
        Node::Image {
            align: None,
            source: vec![TextObj::Plain(img_url.into())],
            params: HashMap::new(),
        },
        Node::Link {
            target: LinkTarget::Url(hot.into()),
            text: vec![],
            class: None,
            new_tab: false,
        },
        Node::Stylesheet(css.into()),
    ];
    // All three external refs are collected (the hotlink too); resolution
    // later keeps only the mirrored ones.
    let mut tails = Vec::new();
    super::collect_external_refs(&content, &mut tails);
    assert_eq!(
        tails,
        vec![
            "scp.wikidot.com/local--files/foo/a.png".to_string(),
            "i.imgur.com/x.jpg".to_string(),
            "scp.wikidot.com/local--files/foo/bg.png".to_string(),
        ]
    );
    // Only the mirrored scp tails resolve to CA refs; the hotlink doesn't.
    let ca = CaRef {
        hash: "d84a29109fe0e70c7a5c22c39bda120fdbc56bd192f5927af95b9af8d0f87c27".into(),
        ext: "png".into(),
    };
    let resolved: HashMap<String, CaRef> = [
        "scp.wikidot.com/local--files/foo/a.png",
        "scp.wikidot.com/local--files/foo/bg.png",
    ]
    .iter()
    .map(|t| (t.to_string(), ca.clone()))
    .collect();
    let out = super::substitute_resources(content, &site, &resolved, &HashMap::new());
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
}

#[test]
fn parse_shell_reads_title_subtitle_and_theme_root() {
    // The git export's `files/`-prefixed tail shape.
    let text = "\
title: \"RPC Authority\"
subtitle: \"Research, Protection, Containment\"
theme_root: files/cdn.jsdelivr.net/gh/x/y@main/style.css
";
    let (title, subtitle, theme_root) = super::parse_shell(text);
    assert_eq!(title.as_deref(), Some("RPC Authority"));
    assert_eq!(
        subtitle.as_deref(),
        Some("Research, Protection, Containment")
    );
    let theme_root = theme_root.expect("theme_root");
    assert_eq!(
        theme_root.as_str(),
        "cdn.jsdelivr.net/gh/x/y@main/style.css"
    );
    // The out/ publication's raw-URL shape, percent-escapes included.
    let (_, _, theme_root) = super::parse_shell(
        "theme_root: https://scp-wiki.wdfiles.com/local--code/component%3Atheme/1\n",
    );
    let theme_root = theme_root.expect("url theme_root");
    assert_eq!(
        theme_root.as_str(),
        "scp-wiki.wdfiles.com/local--code/component:theme/1"
    );
}

#[test]
fn parse_shell_missing_or_hostless_theme_root_is_none() {
    // No theme_root line.
    let (t, s, r) = super::parse_shell("title: \"T\"\n");
    assert_eq!(t.as_deref(), Some("T"));
    assert!(s.is_none());
    assert!(r.is_none());
    // A site-relative CSS path has no host — no mirrored identity.
    let (_, _, r) = super::parse_shell("theme_root: /local--code/component:theme\n");
    assert!(r.is_none());
}

/// Against a real evakuilo publication: the `files.json` index must map the
/// theme-root tail to a full 64-char sha256 (with a real blob behind it in
/// `files_ca/`), the `shell` must parse (title/subtitle/theme_root), and the
/// theme root must resolve through that index. Skipped when the publication
/// isn't checked out.
#[test]
fn real_publication_indexes_files_and_shell() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../../wikidot-evakuilo/data/kolorinko/out/rpcauthority");
    if !root.join("pages.json").exists() {
        eprintln!("skipping: real publication not present");
        return;
    }
    let (rows, w) = build_site(&root, &mut ImHashMap::new()).expect("site builds");
    assert!(!rows.is_empty(), "manifest indexed");
    assert!(w.title.as_deref() == Some("RPC Authority"));
    assert!(w.subtitle.as_ref().is_some_and(|s| !s.is_empty()));
    let theme_path = w.theme_root.clone().expect("theme_root parsed");
    // The files index resolves the theme path to a full 64-char sha256 —
    // NOT the 60-char sharded leaf (the bug this guards against).
    let ca = w.files.get(&theme_path).expect("theme in files index");
    assert!(ca.ext == "css" || !ca.ext.is_empty());
    assert_eq!(ca.hash.len(), 64);
    assert!(ca.hash.bytes().all(|b| b.is_ascii_hexdigit()));
    // The hash must locate the real blob: the on-disk leaf is `hash[4..]`
    // (the rest), not the full hash.
    let blob = root
        .join("files_ca")
        .join(&ca.hash[..2])
        .join(&ca.hash[2..4])
        .join(&ca.hash[4..]);
    assert!(blob.exists(), "blob {blob:?} should exist");
    // And ca_url embeds the full hash under the matching shards.
    let url = super::ca_url(&site("rpcauthority"), ca);
    let prefix = format!(
        "/-/repo/rpcauthority/files/{}/{}",
        &ca.hash[..2],
        &ca.hash[2..4]
    );
    assert!(
        url.starts_with(&prefix),
        "url {url} should start with {prefix}"
    );
}

/// The review regression, against real data: the out/ `shell` names the
/// theme as a raw URL, and an on-site theme is keyed site-relative in
/// `files.json` — the shell tail must resolve through `resource`'s
/// own-host retry to a real blob.
#[test]
fn real_publication_resolves_site_theme_root() {
    init_test_globals();
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../../wikidot-evakuilo/data/kolorinko/out/obscurative");
    if !root.join("pages.json").exists() {
        eprintln!("skipping: real publication not present");
        return;
    }
    let (_, w) = build_site(&root, &mut ImHashMap::new()).expect("site builds");
    let snap = RepoSnapshot {
        sites: site_map_at(site("obscurative"), w),
        bodies: ImHashMap::new(),
    };
    let theme = snap
        .sites
        .get(&site("obscurative"))
        .unwrap()
        .theme_root
        .clone()
        .expect("theme_root parsed from the raw URL");
    let ca =
        resource(&snap, &site("obscurative"), &theme).expect("theme resolves through the index");
    assert_eq!(ca.ext, "css");
}

/// Globals for host-matching tests: the dev config's two sites. `init` is
/// first-write-wins (`OnceLock`), so a second call is a no-op — every test
/// must tolerate this one registry.
fn init_test_globals() {
    let mut sites = indexmap::IndexMap::new();
    sites.insert(
        "obscurative".to_string(),
        crate::globals::SiteCfg {
            landing: "main".into(),
            domains: vec!["www.obscurative.ru".into()],
        },
    );
    sites.insert(
        "rpcauthority".to_string(),
        crate::globals::SiteCfg {
            landing: kolorinko_rt::START_PAGE.into(),
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
    let site = SafePathComponent::new("rpcauthority".into()).unwrap();
    let tail = "www.rpc-wiki.net/component:theme/code/1";
    let content: Content = vec![Node::Stylesheet(
        format!("@import url(http://{tail});").into(),
    )];
    let code = HashMap::from([(
        tail.to_string(),
        super::code_url_for_tail(tail).expect("code url"),
    )]);
    let out = super::substitute_resources(content, &site, &HashMap::new(), &code);
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
                params: HashMap::new(),
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
                params: HashMap::new(),
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
