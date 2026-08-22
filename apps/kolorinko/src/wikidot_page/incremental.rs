use super::*;

// =========================================================================
// Incremental update
// =========================================================================

/// Patch [`old`] for exactly the pages in [`affected`] (each an absolute
/// `_meta` path). For each: drop the old nested-map entry (if any, via the
/// [`Index`]), then re-read the page (as blob Oids) from the new tip's `tree`
/// and re-insert under its current slug. Unaffected pages are structurally
/// shared from [`old`] (`imbl::HashMap`), so only the touched pages are re-read.
pub(super) fn incremental_update(
    repo: &Repository,
    tree: &Tree,
    root: &Path,
    old: &ImHashMap<SafePathComponent, WDWebsite>,
    index: &mut Index,
    affected: HashSet<PathBuf>,
) -> ImHashMap<SafePathComponent, WDWebsite> {
    let mut sites = old.clone();
    for meta_path in affected {
        if let Some(old_key) = index.remove(&meta_path) {
            remove_page(&mut sites, &old_key);
        }
        let Some((site, p1, p2, id)) = meta_page_parts(&meta_path, root) else {
            continue;
        };
        let Some(article) = read_page(repo, tree, &site, &p1, &p2, &id) else {
            continue;
        };
        let Some(site_c) = SafePathComponent::new(site) else {
            continue;
        };
        let Some((cat, name)) = slug_to_key(&article.meta.slug) else {
            continue;
        };
        index.insert(meta_path, (site_c.clone(), cat.clone(), name.clone()));
        insert_page(&mut sites, site_c, cat, name, article);
    }
    sites
}

/// The set of `_meta` paths changed between two tips, plus whether any
/// `files/` attachment was touched (which forces a full [`build_from_tree`] —
/// the `files/` symlink→hash index lives there, so an incremental page patch
/// would leave it stale). Each git-diff delta path (old and new side) is
/// normalized via [`normalize_meta_path`]; non-page paths are dropped from the
/// affected set. `None` if either tree is unreachable (force-push GC of the old
/// tip) — the caller falls back to [`build_from_tree`].
/// TODO: proper incremental design
pub(super) fn diff_changes(
    repo: &Repository,
    old_tip: Oid,
    new_tip: Oid,
    root: &Path,
) -> Option<(HashSet<PathBuf>, bool)> {
    let old_tree = repo.find_commit(old_tip).ok()?.tree().ok()?;
    let new_tree = repo.find_commit(new_tip).ok()?.tree().ok()?;
    let diff = repo
        .diff_tree_to_tree(Some(&old_tree), Some(&new_tree), None)
        .ok()?;
    let mut affected = HashSet::new();
    let mut files_touched = false;
    for delta in diff.deltas() {
        for rel in delta
            .old_file()
            .path()
            .into_iter()
            .chain(delta.new_file().path())
        {
            if is_files_path(rel) {
                files_touched = true;
            } else if let Some(mp) = normalize_meta_path(rel, root) {
                affected.insert(mp);
            }
        }
    }
    Some((affected, files_touched))
}

/// `true` if `rel` is a repo-relative path under `<site>/files/` (an attachment
/// whose content-addressed symlink may have moved). Only the second component is
/// inspected, so any depth under `files/` is recognised.
pub(super) fn is_files_path(rel: &Path) -> bool {
    rel.components()
        .nth(1)
        .and_then(|c| c.as_os_str().to_str())
        .is_some_and(|s| s == "files")
}

/// Collapse a repo-relative delta path to its absolute `_meta` file path:
/// `<site>/_meta/<p1>/<p2>/<id>` stays as-is; `<site>/_pages_by_id/<p1>/<p2>/<id>/rN.txt`
/// swaps `_pages_by_id` for `_meta` and drops the `rN.txt` leaf. The first three
/// components after the kind are the `p1/p2/<id>` shard — `take(3)` handles
/// both shapes uniformly. `None` for anything that isn't a page file.
pub(super) fn normalize_meta_path(rel: &Path, root: &Path) -> Option<PathBuf> {
    let mut comps = rel.components();
    let site = comps.next()?.as_os_str().to_str()?.to_string();
    let kind = comps.next()?.as_os_str().to_str()?;
    if kind != "_meta" && kind != "_pages_by_id" {
        return None;
    }
    let parts: Vec<std::ffi::OsString> = (0..3)
        .filter_map(|_| comps.next().map(|c| c.as_os_str().to_owned()))
        .collect();
    if parts.len() != 3 {
        return None;
    }
    let mut meta = root.join(site).join("_meta");
    for p in parts {
        meta.push(p);
    }
    Some(meta)
}

/// `<root>/<site>/_meta/<p1>/<p2>/<id>` → `(site, p1, p2, id)`, stripping the
/// root and the `_meta` kind segment. Used to re-read one page from the tree.
pub(super) fn meta_page_parts(
    meta_path: &Path,
    root: &Path,
) -> Option<(String, String, String, String)> {
    let rel = meta_path.strip_prefix(root).ok()?;
    let mut c = rel.components();
    let site = c.next()?.as_os_str().to_str()?.to_string();
    let _kind = c.next()?.as_os_str().to_str()?; // "_meta"
    let p1 = c.next()?.as_os_str().to_str()?.to_string();
    let p2 = c.next()?.as_os_str().to_str()?.to_string();
    let id = c.next()?.as_os_str().to_str()?.to_string();
    Some((site, p1, p2, id))
}

/// Remove `(site, cat, name)` from the nested map (and its `by_page_id`
/// entry — the page id is read off the article before it is dropped), pruning
/// a now-empty category or site. Each level is cloned once (`imbl::HashMap` is
/// O(1)), so this is O(log n) and shares the rest of the structure.
pub(super) fn remove_page(
    sites: &mut ImHashMap<SafePathComponent, WDWebsite>,
    (site, cat, name): &Key,
) {
    let Some(mut website) = sites.get(site).cloned() else {
        return;
    };
    // The old article's page id (if the page exists at all) — needed to drop
    // the canonical-addressing entry together with the slug-keyed one.
    let old_page_id = website
        .articles
        .get(cat)
        .and_then(|m| m.get(name))
        .and_then(|a| a.meta.page_id.parse::<u64>().ok());
    if let Some(mut cat_map) = website.articles.get(cat).cloned() {
        cat_map.remove(name);
        if cat_map.is_empty() {
            website.articles.remove(cat);
        } else {
            website.articles.insert(cat.clone(), cat_map);
        }
    }
    if let Some(id) = old_page_id {
        website.by_page_id.remove(&id);
    }
    if website.articles.is_empty() {
        sites.remove(site);
    } else {
        sites.insert(site.clone(), website);
    }
}

/// Insert an [`Article`] under `(site, cat, name)` (and under its page id in
/// the canonical-addressing index), creating the site/category levels as
/// needed.
pub(super) fn insert_page(
    sites: &mut ImHashMap<SafePathComponent, WDWebsite>,
    site: SafePathComponent,
    cat: Option<SafePathComponent>,
    name: SafePathComponent,
    article: Article,
) {
    let mut website = sites.get(&site).cloned().unwrap_or_default();
    let mut cat_map = website.articles.get(&cat).cloned().unwrap_or_default();
    if let Ok(id) = article.meta.page_id.parse::<u64>() {
        website.by_page_id.insert(id, (cat.clone(), name.clone()));
    }
    cat_map.insert(name, article);
    website.articles.insert(cat, cat_map);
    sites.insert(site, website);
}

/// Strip one layer of surrounding double quotes, if present.
pub(super) fn strip_quotes(s: &str) -> &str {
    if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

/// Parse a `<site>/shell` file: `title`, `subtitle` (both quoted) and
/// `theme_root` (a bare `files/<host>/<path>` path into the blob store).
/// Returns the title/subtitle verbatim and the theme root as the validated
/// `<host>/<path>` tail (`files/` prefix stripped) for resolution against the
/// `files/` index. Unknown keys are ignored; a missing `theme_root` or one
/// outside `files/` yields `None`.
pub(super) fn parse_shell(text: &str) -> (Option<String>, Option<String>, Option<RepoAssetPath>) {
    let mut title = None;
    let mut subtitle = None;
    let mut theme_root = None;
    for line in text.lines() {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let v = v.trim();
        match k.trim() {
            "title" => title = Some(strip_quotes(v).to_string()),
            "subtitle" => subtitle = Some(strip_quotes(v).to_string()),
            "theme_root" => {
                if let Some(tail) = v.strip_prefix("files/") {
                    theme_root = RepoAssetPath::new(tail.to_string());
                }
            }
            _ => {}
        }
    }
    (title, subtitle, theme_root)
}
