//! Environment abstraction: HTTP client + secrets.
//!
//! The core logic never touches the network or env vars directly — it goes
//! through this trait so the same code runs on Cloudflare Workers (workers-rs
//! fetch), in tests (mock), and on any other host.

use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Minimal HTTP surface the adapters need.
#[async_trait(?Send)]
pub trait HttpClient {
    /// Returns (status, body_text).
    async fn fetch(
        &self,
        method: &str,
        url: &str,
        headers: &[(String, String)],
        body: Option<&str>,
    ) -> Result<(u16, String), String>;
}

/// What the core needs from its host.
pub struct Env {
    /// EXCHANGE_KEY (master bearer key). None → all authed routes 503 fail-closed.
    pub exchange_key: Option<String>,
    /// Provider secrets by name (e.g. "NVIDIA_API_KEY").
    pub secrets: HashMap<String, String>,
    pub http: Arc<dyn HttpClient>,
    /// In-memory replay seal set — the read-free fast path in front of the
    /// durable `seal_registry` table (seal → first_seen epoch ms).
    ///
    /// `None` (production hosts) → routes use the process-global set in
    /// `routes`. On Cloudflare Workers each isolate has its own statics,
    /// so the global IS per-isolate there and behavior is unchanged.
    ///
    /// `Some(fresh set)` is the test-only isolation path: each test Env
    /// carries its own registry, so parallel tests in one binary never
    /// share replay state. With the shared global, two tests minting
    /// identical envelopes in the same millisecond produced identical
    /// seals and false 409 replays in each other's runs (the
    /// torture_complete / torture_prune flake, characterized 2026-11-22).
    pub seen_seals: Option<Arc<Mutex<HashMap<String, i64>>>>,
}

impl Env {
    pub fn secret(&self, name: &str) -> Option<&str> {
        self.secrets.get(name).map(|s| s.as_str())
    }
}

/// Mock HTTP client for tests: canned responses by URL substring.
pub struct MockHttp {
    pub routes: Vec<(String, u16, String)>,
}

impl MockHttp {
    pub fn new() -> Self {
        MockHttp { routes: Vec::new() }
    }
    pub fn route(mut self, url_contains: &str, status: u16, body: &str) -> Self {
        self.routes.push((url_contains.to_string(), status, body.to_string()));
        self
    }
}

impl Default for MockHttp {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait(?Send)]
impl HttpClient for MockHttp {
    async fn fetch(
        &self,
        _method: &str,
        url: &str,
        _headers: &[(String, String)],
        _body: Option<&str>,
    ) -> Result<(u16, String), String> {
        for (pat, status, body) in &self.routes {
            if url.contains(pat) {
                return Ok((*status, body.clone()));
            }
        }
        Err(format!("mock: no route for {url}"))
    }
}
