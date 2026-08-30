use super::*;

// =========================================================================
// Tree walk → RepoData (blob Oids only; no body text materialised)
// =========================================================================

/// Walk the commit's tree at `tip` and build the sites map: for each site,
/// every `_meta/<p1>/<p2>/<pageid>` blob yields one [`Article`] (metadata parsed
/// from the blob, body blob Oids recorded from the sibling `_pages_by_id`
/// subtree), and each latest body is materialised once into `bodies` — the
/// persistent half of every [`RepoSnapshot`], so a rebuild re-inflates only
/// genuinely new blobs (content addressing: a changed body is a new oid).
/// Returns the bare sites map + reverse [`Index`]; the worker pairs them
/// with `bodies` to form the snapshot.
pub(super) fn build_from_tree(
    repo: &Repository,
    tip: Oid,
    root: &Path,
    bodies: &mut ImHashMap<BlobId, Arc<str>>,
) -> (ImHashMap<SafePathComponent, WDWebsite>, Index) {
    let mut sites: ImHashMap<SafePathComponent, WDWebsite> = ImHashMap::new();
    let mut index: Index = HashMap::new();
    let root_tree = match repo
        .find_commit(tip)
        .and_then(|c| repo.find_tree(c.tree_id()))
    {
        Ok(t) => t,
        Err(_) => return (sites, index),
    };
    // `(site, p1, p2, id)` keyed: the `_meta` blob Oid + per-revision body Oids.
    let mut metas: HashMap<(String, String, String, String), Oid> = HashMap::new();
    let mut rev_bodies: HashMap<(String, String, String, String), ImHashMap<u64, Oid>> =
        HashMap::new();
    let mut shells: HashMap<String, Oid> = HashMap::new();
    // `<site>/files/<host>/<path>` symlink → [`CaRef`], keyed by the
    // percent-decoded `<host>/<path>` tail (matching the form `http_tail`
    // yields from an in-article URL).
    let mut files: HashMap<String, HashMap<RepoAssetPath, CaRef>> = HashMap::new();
    root_tree
        .walk(TreeWalkMode::PreOrder, |dir, entry| {
            if entry.kind() != Some(ObjectType::Blob) {
                return 0;
            }
            let Ok(name) = entry.name() else {
                return 0;
            };
            let path = format!("{dir}{name}");
            let comps: Vec<&str> = path.split('/').collect();
            match comps.as_slice() {
                [site, "_meta", p1, p2, id] => {
                    metas.insert(
                        ((*site).into(), (*p1).into(), (*p2).into(), (*id).into()),
                        entry.id(),
                    );
                }
                [site, "_pages_by_id", p1, p2, id, rfile] => {
                    if let Some(n) = rev_number(rfile) {
                        rev_bodies
                            .entry(((*site).into(), (*p1).into(), (*p2).into(), (*id).into()))
                            .or_default()
                            .insert(n, entry.id());
                    }
                }
                [site, "shell"] => {
                    shells.insert((*site).into(), entry.id());
                }
                // A `files/` attachment: a symlink (mode `0o120000`) whose blob
                // content points into the `_files/` store, sharded
                // `<d1>/<d2>/<rest>` (d1=key[0:2], d2=key[2:4], rest=key[4:]).
                // The leaf is only the 60-char tail, so the full 64-char sha256
                // is the concatenation of all components after `_files/`.
                // Record the CA ref so `repo_resource` can resolve in-article URLs.
                [site, "files", tail @ ..] if entry.filemode() == 0o120000 => {
                    let Some(target) = blob_str(repo, entry.id()) else {
                        return 0;
                    };
                    let Some((_, sharded)) = target.rsplit_once("_files/") else {
                        return 0;
                    };
                    let hash: String = sharded.split('/').collect();
                    if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
                        return 0;
                    }
                    let tail = tail.join("/");
                    let Some(path) = RepoAssetPath::new(percent_decode(&tail)) else {
                        return 0;
                    };
                    let ext = Path::new(name)
                        .extension()
                        .and_then(|e| e.to_str())
                        .unwrap_or("")
                        .to_string();
                    files.entry((*site).to_string()).or_default().insert(
                        path,
                        CaRef {
                            hash: hash.to_string(),
                            ext,
                        },
                    );
                }
                _ => {}
            }
            0
        })
        .ok();
    for (key, meta_oid) in metas {
        let (site, p1, p2, id) = &key;
        let Some(site_c) = SafePathComponent::new(site.clone()) else {
            continue;
        };
        let Some(meta_text) = blob_str(repo, meta_oid) else {
            continue;
        };
        let pm = parse_meta(&meta_text);
        let body_map = rev_bodies.remove(&key).unwrap_or_default();
        let Some(latest) = body_map.keys().max().copied() else {
            continue;
        };
        let Some(&latest_body) = body_map.get(&latest) else {
            continue;
        };
        let Some((cat, name)) = slug_to_key(&pm.slug) else {
            continue;
        };
        let article = Article {
            meta: ArticleMeta {
                title: pm.title,
                tags: pm.tags,
                slug: pm.slug,
                page_id: format!("{p1}{p2}{id}"),
            },
            latest_body: blob_id(latest_body),
            revisions: pm.revisions,
            bodies: body_map.iter().map(|(n, o)| (*n, blob_id(*o))).collect(),
        };
        let body_id = blob_id(latest_body);
        if let Some(body) = materialize_body(repo, body_id) {
            bodies.insert(body_id, body);
        }
        let meta_path = root.join(site).join("_meta").join(p1).join(p2).join(id);
        index.insert(meta_path, (site_c.clone(), cat.clone(), name.clone()));
        insert_page(&mut sites, site_c, cat, name, article);
    }
    for (site, oid) in shells {
        let Some(site_c) = SafePathComponent::new(site.clone()) else {
            continue;
        };
        let Some(text) = blob_str(repo, oid) else {
            continue;
        };
        let (title, subtitle, theme_root) = parse_shell(&text);
        if let Some(mut w) = sites.get(&site_c).cloned() {
            w.title = title;
            w.subtitle = subtitle;
            w.theme_root = theme_root;
            sites.insert(site_c, w);
        }
    }
    for (site, map) in files {
        let Some(site_c) = SafePathComponent::new(site) else {
            continue;
        };
        let mut w = sites.get(&site_c).cloned().unwrap_or_default();
        w.files = map.into_iter().collect();
        sites.insert(site_c, w);
    }
    (sites, index)
}

/// Read one [`Article`] out of `tree` at `(site, p1, p2, id)`: parse the `_meta`
/// blob and record every `r{N}.txt` body blob Oid from the matching
/// `_pages_by_id` subtree. Used by the incremental path to re-read only the
/// pages a git diff touched.
pub(super) fn read_page(
    repo: &Repository,
    tree: &Tree,
    site: &str,
    p1: &str,
    p2: &str,
    id: &str,
) -> Option<Article> {
    let meta_rel = format!("{site}/_meta/{p1}/{p2}/{id}");
    let meta_oid = tree.get_path(Path::new(&meta_rel)).ok()?.id();
    let pm = parse_meta(&blob_str(repo, meta_oid)?);
    let body_map = enumerate_bodies(repo, tree, &format!("{site}/_pages_by_id/{p1}/{p2}/{id}"));
    let latest = body_map.keys().max().copied()?;
    let latest_body = *body_map.get(&latest)?;
    Some(Article {
        meta: ArticleMeta {
            title: pm.title,
            tags: pm.tags,
            slug: pm.slug,
            page_id: format!("{p1}{p2}{id}"),
        },
        latest_body: blob_id(latest_body),
        revisions: pm.revisions,
        bodies: body_map.iter().map(|(n, o)| (*n, blob_id(*o))).collect(),
    })
}

/// Every `r{N}.txt` blob Oid directly under `dir_rel` in `tree`, as `{N → oid}`.
pub(super) fn enumerate_bodies(
    repo: &Repository,
    tree: &Tree,
    dir_rel: &str,
) -> ImHashMap<u64, Oid> {
    let mut map = ImHashMap::new();
    let Ok(entry) = tree.get_path(Path::new(dir_rel)) else {
        return map;
    };
    let Ok(obj) = entry.to_object(repo) else {
        return map;
    };
    let Some(dir_tree) = obj.as_tree() else {
        return map;
    };
    for e in dir_tree.iter() {
        if e.kind() != Some(ObjectType::Blob) {
            continue;
        }
        let Ok(name) = e.name() else {
            continue;
        };
        if let Some(n) = rev_number(name) {
            map.insert(n, e.id());
        }
    }
    map
}

/// Parse the revision number out of a body file name: `r12.txt` → `12`.
pub(super) fn rev_number(name: &str) -> Option<u64> {
    name.strip_prefix('r')
        .and_then(|s| s.strip_suffix(".txt"))
        .and_then(|s| s.parse().ok())
}

/// Read a blob by Oid straight from the odb as an owned `String` (uncached —
/// used for the small `_meta` blobs at build time).
pub(super) fn blob_str(repo: &Repository, oid: Oid) -> Option<String> {
    let blob = repo.find_blob(oid).ok()?;
    std::str::from_utf8(blob.content()).ok().map(String::from)
}

/// Percent-decode `%XX` escapes (e.g. the exporter's `component%3Atheme` →
/// `component:theme`). Used to normalise `files/` index keys to the same form
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

/// Read a body blob by Oid, stripping its frontmatter (uncached; used by
/// [`materialize_body`] and by tests against a live `Repository`).
pub(super) fn read_body(repo: &Repository, oid: Oid) -> Option<String> {
    let raw = blob_str(repo, oid)?;
    Some(revision_body(&raw).to_string())
}

/// Materialise one body blob as its [`RepoSnapshot`] entry — frontmatter
/// stripped, and every NBSP (U+00A0) rewritten to a plain space. The
/// rewrite is the dataset-boundary normalisation the old lens layer
/// performed per delivery: Wikidot exports lean on NBSP for list
/// indentation and trailing whitespace, and one substitution here retires
/// every whitespace-structural consumer's `&nbsp;` workaround (list
/// structure survives — an NBSP and a space each count as one indent unit).
/// `None` if the blob is missing or not UTF-8 (the page then reads as
/// blank — the old failed-RPC convention).
/// git2 oid → snapshot body-store key (the git-side [`BlobId`] boundary
/// conversion — the raw 20 bytes are the same on both sides).
pub(super) fn blob_id(oid: Oid) -> BlobId {
    BlobId(oid.as_bytes().try_into().expect("git2 oid is 20 bytes"))
}

pub(super) fn materialize_body(repo: &Repository, id: BlobId) -> Option<Arc<str>> {
    let oid = git2::Oid::from_bytes(&id.bytes()).ok()?;
    read_body(repo, oid).map(|b| Arc::from(b.replace('\u{a0}', " ")))
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

pub(super) struct ParsedMeta {
    pub(super) slug: String,
    pub(super) title: String,
    pub(super) tags: Vec<String>,
    pub(super) revisions: Vec<RevMeta>,
}

/// Parse a `_meta` file: `slug`/`title`/`tags` header lines plus
/// TAB-separated `revision  revision_id  timestamp  author` rows. Header and
/// revision lines may appear in any order (a line with a TAB is a revision
/// row; a `key: value` line is a header).
pub(super) fn parse_meta(text: &str) -> ParsedMeta {
    let mut slug = String::new();
    let mut title = String::new();
    let mut tags = Vec::new();
    let mut revisions = Vec::new();
    for line in text.lines() {
        if line.contains('\t') {
            let mut f = line.split('\t');
            if let (Some(r), Some(rid), Some(ts), Some(a)) =
                (f.next(), f.next(), f.next(), f.next())
                && let (Ok(rev), Ok(timestamp)) =
                    (r.trim().parse::<u64>(), ts.trim().parse::<i64>())
            {
                revisions.push(RevMeta {
                    revision: rev,
                    revision_id: rid.trim().to_string(),
                    timestamp,
                    author: a.trim().to_string(),
                });
            }
        } else if let Some((k, v)) = line.split_once(':') {
            match k.trim() {
                "slug" => slug = strip_quotes(v.trim()).to_string(),
                "title" => title = strip_quotes(v.trim()).to_string(),
                "tags" => tags = serde_json::from_str(v.trim()).unwrap_or_default(),
                _ => {}
            }
        }
    }
    ParsedMeta {
        slug,
        title,
        tags,
        revisions,
    }
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
/// either segment is unsafe (the page is dropped, as in [`build_from_tree`]).
pub(super) fn slug_to_key(slug: &str) -> Option<(Option<SafePathComponent>, SafePathComponent)> {
    let (cat, name) = slug_parts(slug);
    let name = SafePathComponent::new(name)?;
    let cat = match cat {
        None => None,
        Some(c) => Some(SafePathComponent::new(c)?),
    };
    Some((cat, name))
}
