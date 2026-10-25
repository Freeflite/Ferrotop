//! Thin FFI layer over the NT and Win32 APIs Ferrotop needs.
//!
//! This is the only crate in the workspace allowed to use `unsafe`. The rules:
//!
//! - every unsafe block is a single operation with a `SAFETY:` comment (enforced by clippy);
//! - every buffer the kernel hands back is bounds-checked before anything is read from it;
//! - handles are owned by [`std::os::windows::io::OwnedHandle`] so they can't leak or double-close.
//!
//! Raw structure layouts are ported from System Informer's `phnt` headers (MIT).
#![cfg(windows)]

pub mod amd;
mod buffer;
pub mod connections;
pub mod control;
pub mod cpu_temp;
pub mod desktop;
pub mod disk;
pub mod environment;
mod error;
pub mod gpu;
pub mod handles;
pub mod hardware;
pub mod icon;
pub mod memory;
pub mod modules;
pub mod net;
mod ntapi;
pub mod nvidia;
pub mod pawnio;
pub mod pdh;
pub mod process;
pub mod service;
pub mod signature;
pub mod stack;
pub mod system;
pub mod threads;
pub mod token;
pub mod version;
pub mod window;

#[cfg(test)]
mod fuzz;
#[cfg(feature = "fuzzing")]
pub mod fuzzing;
#[cfg(test)]
mod robustness;

pub use error::{Error, Result};
