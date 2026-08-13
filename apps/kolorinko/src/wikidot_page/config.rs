use super::*;

// =========================================================================
// Configuration
// =========================================================================

/// Configuration for the [`repo`] oracle gear: where to clone and how often to
/// re-pull. Holds `&'static` fields because it is part of a [`GearId`](crate::runtime::...)
/// identity (which is `'static`); a runtime path from a config file is leaked
/// once at startup with `Box::leak`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, dentrado::types::Localizable)]
pub(crate) struct RepoMeta {
    pub(super) url: &'static str,
    pub(super) path: &'static Path,
    pub(super) interval: u32,
}

impl RepoMeta {
    #[must_use]
    pub(crate) const fn new(url: &'static str, path: &'static Path, interval: u32) -> Self {
        Self {
            url,
            path,
            interval,
        }
    }

    #[must_use]
    pub(crate) const fn interval(&self) -> u32 {
        self.interval
    }

    #[must_use]
    pub(crate) const fn path(&self) -> &'static Path {
        self.path
    }

    #[must_use]
    pub(crate) const fn url(&self) -> &'static str {
        self.url
    }
}
