//! Shared fixtures for kolorinko's integration-test binaries.

use kolorinko_render::Scope;

/// The addressing scope of test renders: a fixed space — byte 0x2a everywhere
/// (raw bytes; `Display` adds the 'S' marker) — for link-href assertions, in a
/// context-less scope (`default: None`), so links render full-weight.
pub fn test_space() -> Scope {
    Scope {
        space: Some(kolorinko_rt::SpaceId::from_bytes([0x2a; 16])),
        default: None,
    }
}
