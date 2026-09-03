use super::*;

// =========================================================================
// Configuration
// =========================================================================

/// The evakuilo publication source configuration: the daemon's instance
/// directory (the one holding `out/<site>/` publications) and how often to
/// rescan it. Held `&'static` (process-global via [`crate::globals`], leaked
/// once at initialization) because the values name the directory the
/// publication worker thread reads for the whole process lifetime. No longer
/// part of any [`GearId`](crate::runtime::…) identity — gear ids are purely
/// content addressing since the globals refactor.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct OutMeta {
    pub(super) dir: &'static Path,
    pub(super) interval: u32,
}

impl OutMeta {
    #[must_use]
    pub(crate) const fn new(dir: &'static Path, interval: u32) -> Self {
        Self { dir, interval }
    }

    #[must_use]
    pub(crate) const fn interval(&self) -> u32 {
        self.interval
    }

    #[must_use]
    pub(crate) const fn dir(&self) -> &'static Path {
        self.dir
    }

    /// The published artifact of one site: `<dir>/out/<site>` (see the
    /// `evakuilo` config module's layout).
    #[must_use]
    pub(crate) fn site_dir(&self, site: &str) -> PathBuf {
        self.dir.join("out").join(site)
    }
}
