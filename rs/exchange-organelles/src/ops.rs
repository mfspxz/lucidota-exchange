//! Op registry: (codon, op) -> native runner.
//! Mirrors adapter.py DISPATCH; ops run in-process (no subprocess).

pub mod eb_rollup;
pub mod ed_discover;
pub mod eg_sync;
pub mod ei_compute;
pub mod ek_claim;
pub mod em_submit;
pub mod eo_govern;
pub mod ep_probe;
pub mod er_refresh;
pub mod ew_execute;

use serde_json::Value;
use std::fmt;

/// A failed op: wire `error` = {"verdict": ..., "reason": ...}.
#[derive(Debug, Clone)]
pub struct OpError {
    pub verdict: &'static str,
    pub reason: String,
}

impl OpError {
    pub fn failed(reason: impl Into<String>) -> Self {
        OpError {
            verdict: "failed",
            reason: reason.into(),
        }
    }
    pub fn rejected(reason: impl Into<String>) -> Self {
        OpError {
            verdict: "rejected",
            reason: reason.into(),
        }
    }
}

impl fmt::Display for OpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.verdict, self.reason)
    }
}

impl std::error::Error for OpError {}

/// One op runner: native in-process implementation.
pub type Runner = fn(Value) -> Result<Value, OpError>;

/// (codon, op, runner). Mirrors adapter.py DISPATCH one-for-one.
pub const DISPATCH: &[(&str, &str, Runner)] = &[
    ("eb", "rollup", eb_rollup::run),
    ("eo", "govern", eo_govern::run),
    ("ed", "discover", ed_discover::run),
    ("eg", "sync", eg_sync::run),
    ("ei", "compute", ei_compute::run),
    ("ek", "claim", ek_claim::run),
    ("em", "submit", em_submit::run),
    ("ew", "execute", ew_execute::run),
    ("ep", "probe", ep_probe::run),
    ("er", "refresh", er_refresh::run),
];

pub fn lookup(codon: &str, op: &str) -> Option<Runner> {
    DISPATCH
        .iter()
        .find(|(c, o, _)| *c == codon && *o == op)
        .map(|(_, _, r)| *r)
}

fn home() -> String {
    std::env::var("HOME").unwrap_or_else(|_| "/home/hatch".to_string())
}

fn expanduser(p: &str) -> String {
    if let Some(rest) = p.strip_prefix("~/") {
        format!("{}/{}", home(), rest)
    } else {
        p.to_string()
    }
}

/// Read the Exchange bearer key from ~/.exchange_key.
pub fn exchange_key() -> Result<String, OpError> {
    let path = expanduser("~/.exchange_key");
    std::fs::read_to_string(&path)
        .map(|s| s.trim().to_string())
        .map_err(|e| OpError::failed(format!("read {path}: {e}")))
}

/// Blocking reqwest client with a sane timeout.
pub fn http_client(timeout_secs: u64) -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(timeout_secs))
        .user_agent("lucidota-exchange-organelles/1.0")
        .build()
        .expect("client build")
}
