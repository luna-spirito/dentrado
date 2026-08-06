#![feature(async_trait_bounds)]
#![warn(clippy::pedantic)]
#![deny(unsafe_code)]

// Alias the current crate as `dentrado` so macro-generated code can refer to
// it by absolute path (`dentrado::types::Localizable`) from both inside this
// crate and from downstream dependents uniformly.
pub extern crate self as dentrado;

/// Re-export the `#[gears]` module aggregator and the `#[gear]` fn marker so
/// gears can be authored as plain `async fn`s.
pub use dentrado_macros::{gear, gears};

pub mod core;
pub(crate) mod fs;
pub mod types;
pub mod utils;
pub mod wire;
