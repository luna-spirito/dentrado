#![feature(box_take, async_trait_bounds)]
#![warn(clippy::pedantic)]
#![deny(unsafe_code)]

pub mod core;
pub(crate) mod fs;
pub mod types;
pub mod utils;
pub mod wire;
