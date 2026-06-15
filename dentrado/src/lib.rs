#![feature(box_take)]
#![warn(clippy::pedantic)]
#![deny(unsafe_code)]

pub mod core;
pub mod fadeno;
pub(crate) mod fs;
pub mod types;
pub mod utils;
pub mod wire;
