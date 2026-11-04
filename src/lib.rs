//! A Pumpkin server plugin that speaks the rewrite-bridge passthrough link protocol
//! (`docs/CONTRACT.md` of rewrite-bridge, revision 6). See the README.
//!
//! Everything that does not need the server's API lives in modules that build and are tested on any
//! target; `plugin` is the thin glue to Pumpkin and only builds for WebAssembly.

pub mod bridge;
pub mod collision;
pub mod config;
pub mod mapping;

#[cfg(all(feature = "plugin", target_arch = "wasm32"))]
mod plugin;
