//! exchange-core: the Lucidota Exchange governor logic in Rust.
//!
//! Faithful port of `~/workspace/exchange/worker.js` (v0.6.16 hardening
//! bundle). SQL strings are verbatim; control flow mirrors the JS.
//!
//! The core is host-agnostic: it runs against any [`db::Db`] and any
//! [`env::Env`], so the same logic serves Cloudflare Workers (D1 + fetch),
//! the rusqlite test harness, and future hosts.

pub mod adapters;
pub mod ballot;
pub mod claims;
pub mod db;
pub mod env;
pub mod govern;
pub mod migrate;
pub mod observe;
pub mod policy;
pub mod routes;
pub mod util;

pub use claims::WorkerIdent;

pub const VERSION: &str = "0.6.16";
