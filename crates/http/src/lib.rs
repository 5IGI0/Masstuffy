//! HTTP API over the Masstuffy repository engine.
//!
//! [`router`] builds an axum `Router` (JSON, WARC and raw-envelope routes, documented in
//! `docs/http-api.md`); [`serve`] runs it with a bounded accept loop and a graceful drain; the
//! [`locator`] module makes engine record positions addressable in URLs. The `masstuffy-server`
//! binary (cargo feature `server`) wires these to a repository directory and the process signals.
//!
//! # Security
//!
//! There is **no authentication or TLS**. Bind to loopback and put a reverse proxy that does TLS
//! and authentication in front of any exposed server. Body limits stay enforced by the server.
#![forbid(unsafe_code)]

mod b64;
mod body;
mod error;
mod handlers;
mod jobs;
mod json;
pub mod locator;
mod middleware;
mod options;
mod params;
mod router;
mod serve;

#[cfg(test)]
mod api_tests;
#[cfg(test)]
mod serve_tests;
#[cfg(test)]
mod testutil;

pub use error::ApiError;
pub use locator::{
    LOCATOR_PREFIX, LocatorError, MAX_LOCATOR_TOKEN_LEN, decode_locator, encode_locator,
};
pub use options::{HttpOptions, OptionsError, ServeOptions};
pub use router::router;
pub use serve::{Stop, serve};
