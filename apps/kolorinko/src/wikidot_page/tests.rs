use super::*;
use git2::{IndexAddOption, Signature};
use std::fs;

/// Write one page (`_meta` + a single `r{rev}.txt` body) into the export
/// layout under `root/<site>/…`, returning the relative paths committed.
fn write_page(
    root: &Path,
    site: &str,
    p1: &str,
    p2: &str,
    id: &str,
    slug: &str,
    rev: u64,
    body: &str,
) {
    let base = root.join(site);
    let meta = format!("slug: \"{slug}\"\ntitle: \"T\"\ntags: []\n{rev}\trid\t1\ta\n");
    let meta_path = base.join("_meta").join(p1).join(p2).join(id);
    fs::create_dir_all(meta_path.parent().unwrap()).unwrap();
    fs::write(&meta_path, meta).unwrap();
    let body_dir = base.join("_pages_by_id").join(p1).join(p2).join(id);
    fs::create_dir_all(&body_dir).unwrap();
    fs::write(body_dir.join(format!("r{rev}.txt")), body).unwrap();
}

/// Stage everything under the worktree and commit (first commit if empty).
fn commit(repo: &Repository, msg: &str, parents: &[&git2::Commit]) -> Oid {
    let sig = Signature::now("t", "t@t").unwrap();
    let mut idx = repo.index().unwrap();
    idx.add_all(["*"], IndexAddOption::DEFAULT, None).unwrap();
    idx.write().unwrap();
    let tree = repo.find_tree(idx.write_tree().unwrap()).unwrap();
    repo.commit(Some("HEAD"), &sig, &sig, msg, &tree, parents)
        .unwrap()
}

fn site(s: &str) -> SafePathComponent {
    SafePathComponent::new(s.into()).unwrap()
}

fn root_slug(name: &str) -> Slug {
    (None, SafePathComponent::new(name.into()).unwrap())
}

/// Regression: `repo()`'s cold start spawns the worker and re-borrows the
/// cache `RefCell` in its `None` arm. A `borrow()` left in the `match`
/// scrutinee used to live through the arms and panic ("RefCell already
/// borrowed"). Pointing at an unreachable source keeps the worker
/// repository-less (empty dataset) while still exercising that path, and
/// proves the unchanged-tip path keeps the same `Rc`.
#[test]
fn repo_cold_start_reborrows_without_panic() {
    use compio::runtime::Runtime;
    let dir = std::env::temp_dir().join(format!("kolorinko_repo_nopath_{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    let path: &'static Path = Box::leak(dir.clone().into_boxed_path());
    let meta = RepoMeta::new("file:///nonexistent/kolorinko-repo", path, 900);
    let mut cache = RepoCache::default();
    let rt = Runtime::new().unwrap();
    // Cold start (non-tick): the `None` arm — the original panic site.
    let first = rt.block_on(repo(&meta, false, &mut cache));
    assert!(find_article(&first.sites, &site("nope"), &root_slug("nope")).is_none());
    // Nothing to pull → worker returns None → the prior `Rc` is kept.
    let second = rt.block_on(repo(&meta, true, &mut cache));
    assert!(Rc::ptr_eq(&first, &second));
}

#[test]
fn build_and_materialise_from_odb() {
    let dir = std::env::temp_dir().join(format!("kolorinko_odb_{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();

    let repo = Repository::init(&dir).unwrap();
    write_page(
        &dir,
        "scp",
        "13",
        "05",
        "054470",
        "foo",
        1,
        "---\nx:1\n---\nFoo body",
    );
    write_page(
        &dir,
        "scp",
        "13",
        "05",
        "054471",
        "bar",
        1,
        "---\nx:1\n---\nBar body",
    );
    let tip = commit(&repo, "c1", &[]);

    let (sites, index) = build_from_tree(&repo, tip, &dir);

    // Two pages indexed; bodies stay as Oids until read on demand.
    assert_eq!(index.len(), 2);
    let foo = find_article(&sites, &site("scp"), &root_slug("foo")).unwrap();
    assert_eq!(
        read_body(&repo, foo.latest_body),
        Some("Foo body".to_string())
    );
    assert_eq!(foo.meta.slug, "foo");
    assert_eq!(foo.meta.page_id, "1305054470");
    let bar = find_article(&sites, &site("scp"), &root_slug("bar")).unwrap();
    assert_eq!(
        read_body(&repo, bar.latest_body),
        Some("Bar body".to_string())
    );
}

#[test]
fn incremental_patch_on_tip_move() {
    let dir = std::env::temp_dir().join(format!("kolorinko_inc_{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();

    let repo = Repository::init(&dir).unwrap();
    write_page(
        &dir,
        "scp",
        "13",
        "05",
        "054470",
        "foo",
        1,
        "---\nx:1\n---\nFoo v1",
    );
    write_page(
        &dir,
        "scp",
        "13",
        "05",
        "054471",
        "bar",
        1,
        "---\nx:1\n---\nBar v1",
    );
    let tip1 = commit(&repo, "c1", &[]);

    let (sites, mut index) = build_from_tree(&repo, tip1, &dir);
    let foo = find_article(&sites, &site("scp"), &root_slug("foo")).unwrap();
    assert_eq!(
        read_body(&repo, foo.latest_body),
        Some("Foo v1".to_string())
    );

    // Edit only `foo` (new revision → moved blob Oid) and advance the tip.
    write_page(
        &dir,
        "scp",
        "13",
        "05",
        "054470",
        "foo",
        2,
        "---\nx:1\n---\nFoo v2",
    );
    let parent = repo.find_commit(tip1).unwrap();
    let tip2 = commit(&repo, "c2", &[&parent]);

    let affected = diff_changes(&repo, tip1, tip2, &dir).unwrap().0;
    let tree2 = repo
        .find_commit(tip2)
        .and_then(|c| repo.find_tree(c.tree_id()))
        .unwrap();
    let next = incremental_update(&repo, &tree2, &dir, &sites, &mut index, affected);

    // `bar` is structurally shared from the old snapshot; `foo` re-read.
    let foo = find_article(&next, &site("scp"), &root_slug("foo")).unwrap();
    assert_eq!(
        read_body(&repo, foo.latest_body),
        Some("Foo v2".to_string())
    );
    let bar = find_article(&next, &site("scp"), &root_slug("bar")).unwrap();
    assert_eq!(
        read_body(&repo, bar.latest_body),
        Some("Bar v1".to_string())
    );
}

fn key(cat: Option<&str>, name: &str) -> Key {
    (
        site("scp"),
        cat.map(|c| SafePathComponent::new(c.into()).unwrap()),
        SafePathComponent::new(name.into()).unwrap(),
    )
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

fn plain(s: &str) -> Content {
    vec![Node::Text(TextObj::Plain(s.to_string()))]
}

fn ivar(name: &str) -> Node {
    Node::Text(TextObj::IncludeVar {
        name: name.to_string(),
        default: None,
    })
}

#[test]
fn include_var_resolves_to_value() {
    let vars = vec![("align".to_string(), plain("right"))];
    let out = apply_include_vars(vec![ivar("align")], &vars);
    assert_eq!(out, plain("right"));
}

#[test]
fn include_var_fallback_idiom_prefers_passed_value() {
    // `k={$k}|k=default`: a passed value shadows the literal default.
    let vars = vec![
        ("name".to_string(), plain("conspiracy.png")),
        ("name".to_string(), plain("unknown.png")),
    ];
    let out = apply_include_vars(vec![ivar("name")], &vars);
    assert_eq!(out, plain("conspiracy.png"));
}

#[test]
fn include_var_fallback_idiom_uses_default_when_passthrough_empty() {
    // An empty passthrough (an unset `{$k}`) is skipped, so the literal
    // default is used — the fallback half of the idiom.
    let vars = vec![
        ("name".to_string(), vec![]),
        ("name".to_string(), plain("unknown.png")),
    ];
    let out = apply_include_vars(vec![ivar("name")], &vars);
    assert_eq!(out, plain("unknown.png"));
}

#[test]
fn unresolved_include_var_uses_default() {
    let node = Node::Text(TextObj::IncludeVar {
        name: "x".to_string(),
        default: Some(plain("fallback")),
    });
    let out = apply_include_vars(vec![node], &[]);
    assert_eq!(out, plain("fallback"));
}

#[test]
fn unresolved_include_var_without_default_vanishes() {
    let out = apply_include_vars(vec![ivar("x")], &[]);
    assert!(out.is_empty());
}

#[test]
fn include_var_in_div_param_flattens_to_text() {
    let div = Node::Container {
        kind: ContainerKind::Div {
            inline: false,
            block: true,
            params: HashMap::from([(
                "style".to_string(),
                vec![
                    TextObj::Plain("text-align: ".to_string()),
                    TextObj::IncludeVar {
                        name: "align".to_string(),
                        default: None,
                    },
                ],
            )]),
        },
        content: vec![],
    };
    let vars = vec![("align".to_string(), plain("right"))];
    let out = apply_include_vars(vec![div], &vars);
    let Node::Container {
        kind: ContainerKind::Div { params, .. },
        ..
    } = &out[0]
    else {
        panic!("expected a div")
    };
    assert_eq!(
        params.get("style"),
        Some(&vec![TextObj::Plain("text-align: right".to_string())])
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
    let out = super::substitute_resources(content, &site, &resolved);
    // Image source → CA url.
    let Node::Image { source, .. } = &out[0] else {
        panic!("expected image")
    };
    assert_eq!(
            source,
            &vec![TextObj::Plain(
                "/repo/scp/files/d8/4a/d84a29109fe0e70c7a5c22c39bda120fdbc56bd192f5927af95b9af8d0f87c27.png".into()
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
    assert!(css.contains("/repo/scp/files/d8/4a/"));
    assert!(!css.contains("https://scp.wikidot.com"));
}

#[test]
fn parse_shell_reads_title_subtitle_and_theme_root() {
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
}

#[test]
fn parse_shell_missing_or_non_files_theme_root_is_none() {
    // No theme_root line.
    let (t, s, r) = super::parse_shell("title: \"T\"\n");
    assert_eq!(t.as_deref(), Some("T"));
    assert!(s.is_none());
    assert!(r.is_none());
    // theme_root outside `files/` (a raw URL) is rejected.
    let (_, _, r) = super::parse_shell("theme_root: https://example.com/style.css\n");
    assert!(r.is_none());
}

/// Against the real export repo: a `files/` symlink must index to the
/// full 64-char sha256 (reconstructed from the sharded `_files/d1/d2/rest`
/// target), the `<site>/shell` manifest must parse (title/subtitle/theme_root),
/// and the theme root must resolve to a real blob via that index. Skipped
/// when the real repo isn't checked out.
#[test]
fn real_repo_indexes_sharded_files_and_shell() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.kolorinko/repo");
    if !root.join("rpcauthority/shell").exists() {
        eprintln!("skipping: real repo not present");
        return;
    }
    let repo = Repository::open(&root).expect("open repo");
    let tip = repo.head().unwrap().peel_to_commit().unwrap().id();
    let (sites, _index) = super::build_from_tree(&repo, tip, &root);
    let w = sites.get(&site("rpcauthority")).expect("site indexed");
    // The shell manifest round-trips.
    assert_eq!(w.title.as_deref(), Some("RPC Authority"));
    assert!(w.subtitle.as_ref().is_some_and(|s| !s.is_empty()));
    let theme_path = w.theme_root.clone().expect("theme_root parsed");
    // The files index resolves the theme path to a full 64-char sha256 —
    // NOT the 60-char sharded leaf (the bug this guards against).
    let ca = w.files.get(&theme_path).expect("theme in files index");
    assert_eq!(ca.ext, "css");
    assert_eq!(ca.hash.len(), 64);
    assert!(ca.hash.bytes().all(|b| b.is_ascii_hexdigit()));
    // The reconstructed hash must locate the real blob: the on-disk leaf is
    // `hash[4..]` (the rest), not the full hash.
    let blob = root
        .join("rpcauthority/_files")
        .join(&ca.hash[..2])
        .join(&ca.hash[2..4])
        .join(&ca.hash[4..]);
    assert!(blob.exists(), "blob {blob:?} should exist");
    // And ca_url embeds the full hash under the matching shards.
    let url = super::ca_url(&site("rpcauthority"), ca);
    let prefix = format!(
        "/repo/rpcauthority/files/{}/{}",
        &ca.hash[..2],
        &ca.hash[2..4]
    );
    assert!(
        url.starts_with(&prefix),
        "url {url} should start with {prefix}"
    );
    assert!(url.ends_with(".css"));
}
