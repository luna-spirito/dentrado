use crate::{
    Timestamp, impure_now,
    safe_path::SafePathComponent,
    wikidot_parser::{parse, types::Content},
};
use dentrado::{core::core_ctx::Core, types::LocGroupId};
use git2::Repository;
use log::error;
use std::{
    any::Any,
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

use crate::runtime::{GearId, GearOut, KolorinkoRT};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct RepoPath(&'static Path);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct RepoMeta {
    url: &'static str,
    path: RepoPath,
    interval: u32,
}

#[derive(Default)]
pub(crate) struct RepoCache(Option<(Repository, Timestamp)>);

pub(crate) fn repo(meta: &RepoMeta, group: Option<LocGroupId>, cache: &mut RepoCache) -> RepoPath {
    // Short-circuit if None: if None, we're on the wrong core.
    if group.is_none() {
        return meta.path;
    }

    let now = impure_now();
    let (repo, last_fetch) = match &mut cache.0 {
        Some(x) => x,
        None => match open_or_clone(meta.url, meta.path.0) {
            Some(r) => cache.0.insert((r, now)),
            None => return meta.path,
        },
    };
    if (now.0 % meta.interval) != (last_fetch.0 % meta.interval) {
        pull(repo);
        *last_fetch = now;
    }
    meta.path
}

/// Open the repository at `path`, cloning it from `url` if it has not been cloned yet.
///
/// `Repository::clone` fails when the directory already exists, so on a restart we open
/// the existing clone instead of re-cloning.
fn open_or_clone(url: &str, path: &Path) -> Option<Repository> {
    match Repository::open(path) {
        Ok(r) => Some(r),
        Err(_) => match Repository::clone(url, path) {
            Ok(r) => Some(r),
            Err(e) => {
                error!("Failed to clone {url}: {e}");
                None
            }
        },
    }
}

/// Forcibly pull every change: fetch from `origin` and hard reset to the fetched tip
/// (equivalent to `git fetch && git reset --hard`). libgit2 has no built-in pull.
fn pull(repo: &Repository) {
    if let Err(e) = try_pull(repo) {
        error!("Failed to pull the repository: {e}");
    }
}

fn try_pull(repo: &Repository) -> Result<(), git2::Error> {
    let mut remote = repo.find_remote("origin")?;
    // Force-update all local branches from the remote and record the tip in FETCH_HEAD.
    remote.fetch(&["+refs/heads/*:refs/heads/*"], None, None)?;
    let fetched = repo.revparse_single("FETCH_HEAD")?;
    repo.reset(&fetched, git2::ResetType::Hard, None)?;
    Ok(())
}

/// A key from a revision file's YAML-like frontmatter header.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum HeaderKey {
    Title,
    Tags,
    PageId,
    Site,
    Slug,
    Revision,
    RevisionId,
    Author,
    Timestamp,
}

impl HeaderKey {
    /// Map a raw header key string to a [`HeaderKey`], if recognised.
    fn from_key(key: &str) -> Option<Self> {
        Some(match key {
            "title" => Self::Title,
            "tags" => Self::Tags,
            "page_id" => Self::PageId,
            "site" => Self::Site,
            "slug" => Self::Slug,
            "revision" => Self::Revision,
            "revision_id" => Self::RevisionId,
            "author" => Self::Author,
            "timestamp" => Self::Timestamp,
            _ => return None,
        })
    }
}

/// Return the path to the highest-numbered `r{N}.txt` revision in `dir`.
fn latest_revision(dir: &Path) -> Option<(u64, PathBuf)> {
    fs::read_dir(dir)
        .ok()?
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let name = entry.file_name();
            let name = name.to_str()?;
            let number = name.strip_prefix('r')?.strip_suffix(".txt")?;
            let number: u64 = number.parse().ok()?;
            Some((number, entry.path()))
        })
        .max_by_key(|(number, _)| *number)
}

/// Split a revision file into its frontmatter header and body.
///
/// The header is a flat list of `key: value` lines wrapped between two `---`
/// delimiter lines. String values may be double-quoted; the surrounding quotes
/// are stripped. Unrecognised keys are ignored.
fn parse_revision(text: &str) -> (HashMap<HeaderKey, String>, &str) {
    let mut header = HashMap::new();
    let rest = match text.strip_prefix("---\n") {
        Some(rest) => rest,
        None => return (header, text),
    };
    let (header_text, body) = match rest.find("\n---\n") {
        Some(end) => (&rest[..end], &rest[end + "\n---\n".len()..]),
        None => return (header, text),
    };
    for line in header_text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        if let Some(key) = HeaderKey::from_key(key.trim()) {
            header.insert(key, strip_quotes(value.trim()).to_string());
        }
    }
    (header, body)
}

/// Strip a single layer of surrounding double quotes from `s`, if present.
fn strip_quotes(s: &str) -> &str {
    if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

#[derive(Default)]
pub(crate) struct LoadCache(Option<(u64, Arc<Content>)>);

pub(crate) fn load_page(
    meta: &RepoMeta,
    site: &SafePathComponent,
    (category_, name): &(Option<SafePathComponent>, SafePathComponent),
    core: &Core<KolorinkoRT>,
    cache: &mut LoadCache,
) -> Arc<Content> {
    let GearOut::RepoOut(repo) = core.secondary_get(GearId::Repo(meta.clone())) else {
        unreachable!()
    };

    let mut dir = repo.0.join(site).join("pages");
    if let Some(category_) = category_ {
        dir = dir.join(category_);
    }
    dir = dir.join(name);
    let latest = match latest_revision(&dir) {
        Some(p) => p,
        None => {
            error!("No revision found in {}", dir.display());
            return Arc::new(Content::new());
        }
    };
    if let Some(cache) = &mut cache.0
        && cache.0 == latest.0
    {
        cache.1.clone()
    } else {
        let text = match fs::read_to_string(&latest.1) {
            Ok(t) => t,
            Err(e) => {
                error!("Failed to read {}: {e}", latest.1.display());
                return Arc::new(Content::new());
            }
        };

        // TODO: Header
        let (_header, body) = parse_revision(&text);

        let data = Arc::new(parse(body));
        cache.0 = Some((latest.0, data.clone()));
        data
    }
}
