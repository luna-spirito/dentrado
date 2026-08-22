//! Process-global configuration: the one place runtime-tuned values (the
//! export repo, the space registry) live, instead of being threaded through
//! every server/gear signature.
//!
//! This is a deliberate transitional shape. `RepoMeta` used to be a gear-id
//! field (`GearId::Repo(repo_meta)`), which forced the `&'static` strings
//! (`Box::leak`), the `wire_skip` injections, and a `RepoMeta` parameter on
//! every server entry point. With the repo singleton demoted to a process
//! global, gear identity is purely content addressing (`site`, `slug`,
//! `space`, `local`) and the servers take no config at all.
//!
//! The next step (the agreed `SiteRegistry` design) replaces the static with
//! an immutable snapshot + `SiteEvent` stream — same read shape (`site_of` /
//! `space_of` lookups), so call sites built against this module survive the
//! swap.

use std::{collections::HashMap, num::NonZero, sync::OnceLock};

use kolorinko_rt::{SafePathComponent, SpaceId};

use crate::wikidot_page::RepoMeta;

/// Everything configured once at startup. Leaked into a `OnceLock` — the
/// process runs until it doesn't, and `&'static` access keeps every reader
/// lock-free.
pub(crate) struct Globals {
    repo: RepoMeta,
    /// Registered content spaces: canonical id → the export dataset site that
    /// serves it (the `[[space]]` config table).
    spaces: HashMap<SpaceId, SafePathComponent>,
    /// The reverse registry (site → space): how legacy `/site/…` paths find
    /// their canonical `/space/…` target for 301s.
    by_site: HashMap<SafePathComponent, SpaceId>,
}

static GLOBALS: OnceLock<Globals> = OnceLock::new();

/// Fallback pull interval when the config's is zero ([`RepoCfg::interval`]).
const DEFAULT_INTERVAL: u32 = 900;

/// Initialize once from the parsed config. Fails (before anything starts)
/// on an invalid space id or site name, or a second initialization.
pub(crate) fn init(
    repo_url: &str,
    repo_dir: &str,
    interval: u32,
    spaces: &[(String, String)],
) -> anyhow::Result<()> {
    // `RepoMeta` fields are `&'static` by design (they name the repo the
    // worker thread owns for the process lifetime); the config's strings
    // are leaked here — the single leak point in the crate.
    let repo = RepoMeta::new(
        Box::leak(repo_url.to_owned().into_boxed_str()),
        Box::leak(std::path::PathBuf::from(repo_dir).into_boxed_path()),
        if interval == 0 {
            DEFAULT_INTERVAL
        } else {
            interval
        },
    );
    let mut map = HashMap::new();
    let mut by_site = HashMap::new();
    for (id, site) in spaces {
        let space = SpaceId::parse(id).ok_or_else(|| {
            anyhow::anyhow!("invalid space id {id:?}: want 22-char canonical base64url")
        })?;
        let site = SafePathComponent::new(site.clone())
            .ok_or_else(|| anyhow::anyhow!("invalid space site name {site:?}"))?;
        by_site.insert(site.clone(), space);
        map.insert(space, site);
    }
    GLOBALS
        .set(Globals {
            repo,
            spaces: map,
            by_site,
        })
        .map_err(|_| anyhow::anyhow!("global config initialized twice"))
}

fn g() -> &'static Globals {
    GLOBALS.get().expect("global config not initialized")
}

/// The export-repo configuration (url/path/interval bundle).
pub(crate) fn repo() -> &'static RepoMeta {
    &g().repo
}

/// The pull interval for the `repo` oracle's timer (never zero).
pub(crate) fn interval() -> NonZero<u64> {
    NonZero::new(u64::from(g().repo.interval()))
        .unwrap_or_else(|| NonZero::new(u64::from(DEFAULT_INTERVAL)).expect("900 != 0"))
}

/// Which dataset site serves this space (`None`: unregistered id).
pub(crate) fn site_of(space: &SpaceId) -> Option<&'static SafePathComponent> {
    g().spaces.get(space)
}

/// Which registered space a dataset site is served under (`None`: the
/// site has no canonical addressing — legacy paths render in place).
pub(crate) fn space_of(site: &SafePathComponent) -> Option<SpaceId> {
    g().by_site.get(site).copied()
}
