use super::*;

// =========================================================================
// Configuration
// =========================================================================

/// The export-repo source configuration: where to clone and how often to
/// re-pull. Held `&'static` (process-global via [`crate::globals`], leaked
/// once at initialization) because the values name the repository the git
/// worker thread owns for the whole process lifetime. No longer part of any
/// [`GearId`](crate::runtime::…) identity — gear ids are purely content
/// addressing since the globals refactor.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
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
