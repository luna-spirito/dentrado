use super::*;

// =========================================================================
// Incremental update (manifest diff)
// =========================================================================

/// Patch one site for exactly the manifest rows that drifted since the last
/// adoption. Two passes, removals strictly before inserts: a slug swap
/// between two pages (`x→b` while `y: b→a`) would otherwise insert one page
/// under a key the other is about to vacate and then remove it again.
/// Drifted pages are re-read from their archive (frontmatter → revision
/// table, latest body materialised — content addressing means a re-read page
/// whose body didn't change re-inserts the *same* [`BlobId`], sharing the
/// old `Arc`); unchanged rows are structurally shared from `w` untouched.
pub(super) fn patch_pages(
    w: &mut WDWebsite,
    site_dir: &Path,
    old_rows: &HashMap<u64, PageRow>,
    new_rows: &HashMap<u64, PageRow>,
    bodies: &mut ImHashMap<BlobId, Arc<str>>,
) {
    for (id, old) in old_rows {
        if new_rows.get(id) == Some(old) {
            continue;
        }
        if let Some((cat, name)) = slug_to_key(&old.slug) {
            remove_page(w, cat, &name, *id);
        }
    }
    for (id, row) in new_rows {
        if old_rows.get(id) == Some(row) {
            continue;
        }
        let Some(article) = read_stored_page(site_dir, id, row, bodies) else {
            continue;
        };
        let Some((cat, name)) = slug_to_key(&article.meta.slug) else {
            continue;
        };
        w.by_page_id.insert(*id, (cat.clone(), name.clone()));
        w.articles.entry(cat).or_default().insert(name, article);
    }
}

/// Remove `(category, name)` from the nested map and drop the page's
/// canonical-addressing entry, pruning a now-empty category. The site entry
/// itself is kept (the `files/` index and shell live there even for a site
/// that lost its last page). Each level is cloned once (`imbl::HashMap` is
/// O(1)), so this shares the rest of the structure.
pub(super) fn remove_page(
    w: &mut WDWebsite,
    cat: Option<SafePathComponent>,
    name: &SafePathComponent,
    page_id: u64,
) {
    if let Some(mut cat_map) = w.articles.get(&cat).cloned() {
        cat_map.remove(name);
        if cat_map.is_empty() {
            w.articles.remove(&cat);
        } else {
            w.articles.insert(cat, cat_map);
        }
    }
    w.by_page_id.remove(&page_id);
}

/// Parse a `shell` file: `title`, `subtitle` (both quoted) and `theme_root`.
/// Returns the title/subtitle verbatim and the theme root as the validated,
/// percent-decoded `<host>/<path>` tail for resolution against the `files/`
/// index. Two source shapes normalise to that tail: the raw URL the `out/`
/// publication writes (`https://host/path`), and the `files/`-prefixed tail
/// the older git export wrote. Unknown keys are ignored; a missing
/// `theme_root` — or one with no mirrored identity (e.g. a site-relative CSS
/// path, no host) — yields `None`.
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
                let tail = match v.strip_prefix("files/") {
                    Some(t) => Some(t.to_string()),
                    None => url_tail(v),
                };
                if let Some(tail) = tail {
                    theme_root = RepoAssetPath::new(percent_decode(&tail));
                }
            }
            _ => {}
        }
    }
    (title, subtitle, theme_root)
}
