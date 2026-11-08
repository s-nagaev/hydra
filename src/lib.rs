//! Hydra library target: configuration, storage, and HTTP handlers shared
//! by the binary and the integration test suite. The `main.rs` binary stays
//! a thin startup shell so `tests/` can drive the real router against an
//! in-process mock upstream.

pub mod cache;
pub mod config;
pub mod db;
pub mod http;
pub mod keys;
pub mod logging;
