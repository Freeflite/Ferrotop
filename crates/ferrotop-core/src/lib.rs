//! Process and system monitoring logic for Ferrotop.
//!
//! No `unsafe` lives here: all OS access goes through `ferrotop-sys`, and everything in this
//! crate is plain data processing that can be unit-tested with synthetic samples.
#![forbid(unsafe_code)]

pub mod delta;
pub mod details;
pub mod find;
pub mod format;
#[cfg(feature = "fuzzing")]
pub mod fuzzing;
pub mod history;
pub mod json;
pub mod lhm;
pub mod names;
pub mod nettools;
pub mod network;
pub mod perf;
pub mod properties;
pub mod provider;
pub mod sampler;
pub mod settings;
pub mod table;
pub mod tree;

#[cfg(test)]
mod fuzz;
