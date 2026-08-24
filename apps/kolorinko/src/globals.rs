//! Process-global configuration: the one place runtime-tuned values (the
//! export repo, the space registry) live, instead of being threaded through
//! every server/gear signature.
//!
//! This is a deliberate transitional shape. `RepoMeta` used to be a gear-id
//! field (`GearId::Repo(repo_meta)`), which forced the `&'static` strings
//! (`Box::leak`), the `wire_skip` injections, and a `RepoMeta` parameter on
//! every server entry point. With the repo singleton demoted to a process
//! global, gear identity is purely content addressing (`space`, `local`,
//! `site`, `slug`) and the servers take no config at all.
//!
//! Spaces are *derived*, never hand-picked: each `ensure-evakuilo-sites`
//! entry names a Wikidot-export site, and its canonical space id is
//! `SHA-256("wikidot-evakuilo/v1/<site>")[0..16]`, marker-prefixed
//! (see [`kolorinko_rt::SpaceId`]). Independent operators mirroring the same
//! export therefore converge on the same address with zero coordination —
//! fork navigation is a URL segment swap.
//!
//! The next step (the agreed `SiteRegistry` design) replaces the static with
//! an immutable snapshot + `SiteEvent` stream — same read shape (`site_of` /
//! `space_of` lookups), so call sites built against this module survive the
//! swap.

use std::{collections::HashMap, num::NonZero, sync::OnceLock};

use kolorinko_rt::{SafePathComponent, SpaceId};
use ring::digest::{SHA256, digest};

use crate::wikidot_page::RepoMeta;

/// One registered content space: the export dataset site that serves it and
/// the slug its bare `/SPACE` (and `/`) resolve to (the site's landing page —
/// Wikidot's default is `start`, but many wikis name it `main`).
pub(crate) struct SpaceReg {
    pub site: SafePathComponent,
    pub landing: SafePathComponent,
}

/// Everything configured once at startup. Leaked into a `OnceLock` — the
/// process runs until it doesn't, and `&'static` access keeps every reader
/// lock-free.
pub(crate) struct Globals {
    repo: RepoMeta,
    /// Registered content spaces in config order: canonical id → registry
    /// entry. The first entry is whose landing page `/` serves.
    spaces: Vec<(SpaceId, SpaceReg)>,
    /// Site name → space id (the render CLI addresses sites by name).
    by_site: HashMap<SafePathComponent, SpaceId>,
}

static GLOBALS: OnceLock<Globals> = OnceLock::new();

/// Fallback pull interval when the config's is zero ([`RepoCfg::interval`]).
const DEFAULT_INTERVAL: u32 = 900;

/// The derivation domain for Wikidot-export.
/// Part of the address contract: changing it changes every space id.
const EVAKUILO_DOMAIN: &str = "wikidot-evakuilo/v1";

/// The canonical space id for one Wikidot-export site:
/// `SHA-256("<domain>/<site>")[0..16]`, wrapped raw.
pub(crate) fn evakuilo_space_id(site: &str) -> SpaceId {
    let sum = digest(&SHA256, format!("{EVAKUILO_DOMAIN}/{site}").as_bytes());
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&sum.as_ref()[..16]);
    SpaceId::from_bytes(bytes)
}

/// Parse one `ensure-evakuilo-sites` entry: `"<site>"` (landing defaults to
/// [`kolorinko_rt::START_PAGE`]) or `"<site>:<landing>"`. The colon cannot
/// occur in a site name, so the two forms are unambiguous.
fn parse_entry(entry: &str) -> anyhow::Result<(SafePathComponent, SafePathComponent)> {
    let spc = |s: &str| {
        SafePathComponent::new(s.to_owned())
            .ok_or_else(|| anyhow::anyhow!("invalid space site name {s:?}"))
    };
    match entry.split_once(':') {
        Some((site, landing)) => Ok((spc(site)?, spc(landing)?)),
        None => Ok((
            spc(entry)?,
            SafePathComponent::new(kolorinko_rt::START_PAGE.to_owned())
                .expect("start is a safe name"),
        )),
    }
}

/// Initialize once from the parsed config. Fails (before anything starts) on
/// an invalid site/landing name, a duplicate space id, or a second
/// initialization.
pub(crate) fn init(
    repo_url: &str,
    repo_dir: &str,
    interval: u32,
    evakuilo_sites: &[String],
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
    let mut spaces = Vec::new();
    let mut by_site = HashMap::new();
    for entry in evakuilo_sites {
        let (site, landing) = parse_entry(entry)?;
        let space = evakuilo_space_id(&site);
        if !by_site.contains_key(&site) {
            by_site.insert(site.clone(), space);
        }
        if spaces.iter().any(|(s, _)| *s == space) {
            anyhow::bail!("duplicate space id for {entry:?}");
        }
        spaces.push((space, SpaceReg { site, landing }));
    }
    GLOBALS
        .set(Globals {
            repo,
            spaces,
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

/// The registry entry for a registered space (`None`: unregistered id).
pub(crate) fn reg_of(space: &SpaceId) -> Option<&'static SpaceReg> {
    g().spaces.iter().find(|(s, _)| s == space).map(|(_, r)| r)
}

/// Which dataset site serves this space (`None`: unregistered id).
pub(crate) fn site_of(space: &SpaceId) -> Option<&'static SafePathComponent> {
    reg_of(space).map(|r| &r.site)
}

/// Which registered space a dataset site is served under (`None`: the site
/// has no canonical addressing — only the render CLI names sites directly).
pub(crate) fn space_of(site: &SafePathComponent) -> Option<SpaceId> {
    g().by_site.get(site).copied()
}

/// The first registered space and its registry entry — whose landing page the
/// bare `/` serves. `None` when no space is configured at all.
pub(crate) fn first_space() -> Option<(SpaceId, &'static SpaceReg)> {
    g().spaces.first().map(|(s, r)| (*s, r))
}
