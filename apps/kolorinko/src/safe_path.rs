use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct SafePathComponent(String);

// TODO: Validate
impl SafePathComponent {
    pub(crate) fn new(input: String) -> Option<Self> {
        let mut components = Path::new(&input).components();
        match (components.next(), components.next()) {
            (Some(Component::Normal(_)), None) => Some(SafePathComponent(input)),
            _ => None,
        }
    }

    /// Adds `_` at the end of the component.
    pub(crate) fn with_underline_suffix(mut self) -> Self {
        self.0.push('_');
        self
    }
}

impl AsRef<Path> for SafePathComponent {
    fn as_ref(&self) -> &Path {
        self.0.as_ref()
    }
}
