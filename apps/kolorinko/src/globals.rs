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
//! key names a Wikidot-export site, and its canonical space id is
//! `SHA-256("wikidot-evakuilo/v1/<site>")[0..16]`, marker-prefixed
//! (see [`kolorinko_rt::SpaceId`]). Independent operators mirroring the same
//! export therefore converge on the same address with zero coordination —
//! fork navigation is a URL segment swap.
//!
//! The next step (the agreed `SiteRegistry` design) replaces the static with
//! an immutable snapshot + `SiteEvent` stream — same read shape (`site_of` /
//! `space_of` lookups), so call sites built against this module survive the
//! swap.

use std::{
    collections::{HashMap, HashSet},
    num::NonZero,
    sync::OnceLock,
};

use indexmap::IndexMap;
use kolorinko_rt::{SafePathComponent, SpaceId};
use ring::digest::{SHA256, digest};
use serde::Deserialize;

use crate::wikidot_page::RepoMeta;

/// One `ensure-evakuilo-sites` table entry: the site's landing page (`start`,
/// Wikidot's default, unless named otherwise — e.g. obscurative's `main`) and
/// its alias domains.
#[derive(Debug, Deserialize)]
pub(crate) struct SiteCfg {
    #[serde(default = "default_landing")]
    pub landing: String,
    /// The source site's custom domains (`www.obscurative.ru`, …). See
    /// [`SpaceReg::domains`].
    #[serde(default)]
    pub domains: Vec<String>,
}

fn default_landing() -> String {
    kolorinko_rt::START_PAGE.to_owned()
}

/// One registered content space: the export dataset site that serves it, the
/// slug its bare `/SPACE` (and `/`) resolve to (the site's landing page —
/// Wikidot's default is `start`, but many wikis name it `main`), and the
/// source site's alias domains.
pub(crate) struct SpaceReg {
    pub site: SafePathComponent,
    pub landing: SafePathComponent,
    /// The source site's custom domains: URLs on these hosts (CSS `url()`,
    /// images, links) resolve to the site's mirrored attachments exactly like
    /// `<site>.wikidot.com` ones do, and — later — `Host: <domain>` requests
    /// will address the space without a `SPACE_ID` segment.
    pub domains: Box<[String]>,
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

/// Initialize once from the parsed config. Fails (before anything starts) on
/// an invalid site/landing/domain name, a domain claimed by two sites, or a
/// second initialization. A TOML table cannot repeat a key, so duplicate
/// sites and duplicate space ids are unrepresentable by construction.
pub(crate) fn init(
    repo_url: &str,
    repo_dir: &str,
    interval: u32,
    sites: &IndexMap<String, SiteCfg>,
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
    let spc = |s: &str, what: &str| {
        SafePathComponent::new(s.to_owned())
            .ok_or_else(|| anyhow::anyhow!("invalid {what} name {s:?}"))
    };
    let mut spaces = Vec::with_capacity(sites.len());
    let mut by_site = HashMap::with_capacity(sites.len());
    let mut seen_domains: HashSet<String> = HashSet::new();
    for (site_name, cfg) in sites {
        let site = spc(site_name, "space site")?;
        let landing = spc(&cfg.landing, "landing page")?;
        for d in &cfg.domains {
            if d.is_empty()
                || !d
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
            {
                anyhow::bail!("invalid alias domain {d:?} of site {site_name:?}");
            }
            if !seen_domains.insert(d.to_ascii_lowercase()) {
                anyhow::bail!("alias domain {d:?} claimed by more than one site");
            }
        }
        let space = evakuilo_space_id(site_name);
        by_site.insert(site.clone(), space);
        spaces.push((
            space,
            SpaceReg {
                site,
                landing,
                domains: cfg.domains.clone().into_boxed_slice(),
            },
        ));
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

/// The configured alias domains of a registered space (`None`: unregistered
/// id). The resource resolver retries a missed lookup under the canonical
/// `<site>.wikidot.com` host for each of them
/// ([`crate::wikidot_page::repo_resource`]).
pub(crate) fn domains_of(space: &SpaceId) -> Option<&'static [String]> {
    reg_of(space).map(|r| &r.domains[..])
}

/// The first registered space and its registry entry — whose landing page the
/// bare `/` serves. `None` when no space is configured at all.
pub(crate) fn first_space() -> Option<(SpaceId, &'static SpaceReg)> {
    g().spaces.first().map(|(s, r)| (*s, r))
}
