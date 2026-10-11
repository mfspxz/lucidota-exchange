//! Provider adapters — port of worker.js `adapters` registry.
//!
//! Five adapters: `ai-lane` (OpenAI-compatible inference), `http-post`,
//! `http-get`, `self` (no-op), `claimed` (delegated; never executes on the
//! Worker). `adapter_for()` mirrors `adapterFor()`: `config.adapter` else
//! `resource.kind`.

use crate::db::{Row, RowExt};
use crate::env::Env;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Instant;

pub const ADAPTER_NAMES: &[&str] = &["ai-lane", "http-post", "http-get", "self", "claimed"];

#[derive(Clone, Debug)]
pub struct ProbeResult {
    pub ok: bool,
    pub latency_ms: f64,
    pub detail: String,
}

#[derive(Clone, Debug)]
pub struct ExecResult {
    pub ok: bool,
    pub output: Option<Value>,
    pub latency_ms: f64,
    pub error: Option<String>,
    pub error_class: Option<String>,
}

#[async_trait(?Send)]
pub trait Adapter: Send + Sync {
    fn name(&self) -> &'static str;
    fn claimed(&self) -> bool {
        false
    }
    async fn probe(&self, resource: &Row, env: &Env) -> ProbeResult;
    async fn execute(&self, resource: &Row, spec: &Value, env: &Env) -> ExecResult;
}

pub fn classify_http_error(status: u16, err_text: &str) -> &'static str {
    if status == 429 {
        return "RATE_LIMITED";
    }
    if status == 401 || status == 403 {
        return "AUTH_ERROR";
    }
    if status >= 500 {
        return "SERVER_ERROR";
    }
    if status == 408 {
        return "TIMEOUT";
    }
    if status >= 400 {
        return "BAD_RESPONSE";
    }
    let lower = err_text.to_lowercase();
    if lower.contains("abort") || lower.contains("timeout") || lower.contains("timed out") {
        return "TIMEOUT";
    }
    if lower.contains("fetch failed")
        || lower.contains("network")
        || lower.contains("enotfound")
        || lower.contains("econn")
    {
        return "NETWORK";
    }
    "UNKNOWN"
}

/// fetch with timeout: returns (response, latency_ms, timed_out, error).
async fn fetch_with_timeout(
    env: &Env,
    method: &str,
    url: &str,
    headers: &[(String, String)],
    body: Option<&str>,
    timeout_ms: u64,
) -> (Option<(u16, String)>, f64, bool, Option<String>) {
    let start = Instant::now();
    let fut = env.http.fetch(method, url, headers, body);
    // On wasm32 (Workers), tokio is unavailable; the Workers runtime enforces
    // its own fetch timeouts. On native, use tokio::time::timeout.
    #[cfg(not(target_arch = "wasm32"))]
    let res = {
        let timeout = std::time::Duration::from_millis(timeout_ms);
        match tokio::time::timeout(timeout, fut).await {
            Ok(r) => Ok(r),
            Err(_) => Err("timeout".to_string()),
        }
    };
    #[cfg(target_arch = "wasm32")]
    let res: Result<Result<(u16, String), String>, String> = Ok(fut.await);
    match res {
        Ok(Ok((status, text))) => (Some((status, text)), start.elapsed().as_millis() as f64, false, None),
        Ok(Err(e)) => {
            let timed_out = e.to_lowercase().contains("abort") || e.to_lowercase().contains("timeout");
            (
                None,
                start.elapsed().as_millis() as f64,
                timed_out,
                Some(e.chars().take(200).collect()),
            )
        }
        Err(_) => (None, start.elapsed().as_millis() as f64, true, Some("timeout".to_string())),
    }
}

fn cfg_of(resource: &Row) -> Value {
    serde_json::from_str(&resource.text_or("config", "{}")).unwrap_or(json!({}))
}

fn timeout_ms(cfg: &Value, spec: &Value, default: u64) -> u64 {
    let c = cfg.get("timeoutMs").and_then(|v| v.as_u64()).unwrap_or(default);
    let s = spec.get("timeout_ms").and_then(|v| v.as_u64()).unwrap_or(60000);
    c.min(s)
}

fn headers_of(cfg: &Value) -> Vec<(String, String)> {
    cfg.get("headers")
        .and_then(|v| v.as_object())
        .map(|m| {
            m.iter()
                .map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_string()))
                .collect()
        })
        .unwrap_or_default()
}

fn probe_detail(r: &ExecResult) -> String {
    if r.ok {
        "probe ok".to_string()
    } else {
        format!(
            "{}: {}",
            r.error_class.clone().unwrap_or_default(),
            r.error.clone().unwrap_or_default()
        )
    }
}

fn no_resp(latency_ms: f64, timed_out: bool, error: Option<String>) -> ExecResult {
    ExecResult {
        ok: false,
        output: None,
        latency_ms,
        error,
        error_class: Some(if timed_out { "TIMEOUT" } else { "NETWORK" }.to_string()),
    }
}

// --- ai-lane ---------------------------------------------------------------

struct AiLane;
#[async_trait(?Send)]
impl Adapter for AiLane {
    fn name(&self) -> &'static str {
        "ai-lane"
    }
    async fn probe(&self, resource: &Row, env: &Env) -> ProbeResult {
        let spec = json!({ "input": { "messages": [{ "role": "user", "content": "Reply with exactly: ok" }], "max_tokens": 8 }, "timeout_ms": 15000 });
        let r = self.execute(resource, &spec, env).await;
        ProbeResult { ok: r.ok, latency_ms: r.latency_ms, detail: probe_detail(&r) }
    }
    async fn execute(&self, resource: &Row, spec: &Value, env: &Env) -> ExecResult {
        let cfg = cfg_of(resource);
        let input = spec.get("input").cloned().unwrap_or(json!({}));
        let model = input
            .get("model")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .or_else(|| cfg.get("model").and_then(|v| v.as_str()).map(|s| s.to_string()))
            .unwrap_or_default();
        let body = json!({
            "model": model,
            "messages": input.get("messages").cloned().unwrap_or(json!([])),
            "stream": false,
            "max_tokens": input.get("max_tokens").and_then(|v| v.as_u64()).unwrap_or(512),
            "temperature": input.get("temperature").and_then(|v| v.as_f64()).unwrap_or(0.7),
        });
        let base = cfg
            .get("baseUrl")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim_end_matches('/')
            .to_string();
        let url = format!("{}/chat/completions", base);
        let tmo = timeout_ms(&cfg, spec, 20000);
        let headers = vec![("Content-Type".to_string(), "application/json".to_string())];
        let (resp, latency_ms, timed_out, error) =
            fetch_with_timeout(env, "POST", &url, &headers, Some(&body.to_string()), tmo).await;
        let (status, text) = match resp {
            Some(x) => x,
            None => return no_resp(latency_ms, timed_out, error),
        };
        if !(200..300).contains(&status) {
            let e = format!("lane HTTP {}: {}", status, text.chars().take(200).collect::<String>());
            let cls = classify_http_error(status, &text).to_string();
            return ExecResult { ok: false, output: None, latency_ms, error: Some(e), error_class: Some(cls) };
        }
        let parsed: Value = match serde_json::from_str(&text) {
            Ok(p) => p,
            Err(_) => {
                return ExecResult {
                    ok: false,
                    output: None,
                    latency_ms,
                    error: Some("lane returned non-JSON".to_string()),
                    error_class: Some("BAD_RESPONSE".to_string()),
                }
            }
        };
        let content = parsed
            .get("choices")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("message"))
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_str())
            .map(|s| s.to_string());
        match content {
            Some(content) => ExecResult {
                ok: true,
                latency_ms,
                output: Some(json!({
                    "content": content,
                    "model": parsed.get("model").and_then(|v| v.as_str()).map(|s| s.to_string())
                        .or_else(|| cfg.get("model").and_then(|v| v.as_str()).map(|s| s.to_string())),
                    "usage": parsed.get("usage").cloned().unwrap_or(Value::Null),
                })),
                error: None,
                error_class: None,
            },
            None => ExecResult {
                ok: false,
                output: None,
                latency_ms,
                error: Some("lane returned no content".to_string()),
                error_class: Some("BAD_RESPONSE".to_string()),
            },
        }
    }
}

// --- http-post / http-get -----------------------------------------------------

fn verify_path(
    cfg: &Value,
    parsed: &Option<Value>,
    text: &str,
    latency_ms: f64,
) -> Option<ExecResult> {
    let (path, p) = match (
        cfg.get("verify_json_path").and_then(|v| v.as_str()),
        parsed.as_ref(),
    ) {
        (Some(a), Some(b)) => (a, b),
        _ => return None,
    };
    let mut val: &Value = p;
    for k in path.split('.') {
        val = val.get(k).unwrap_or(&Value::Null);
    }
    if let Some(expect) = cfg.get("verify_equals") {
        if val != expect {
            return Some(ExecResult {
                ok: false,
                latency_ms,
                output: Some(json!({ "raw": text.chars().take(500).collect::<String>() })),
                error: Some(format!(
                    "verify failed: {}={}",
                    path,
                    serde_json::to_string(val).unwrap_or_default()
                )),
                error_class: Some("VERIFICATION_FAILED".to_string()),
            });
        }
    }
    None
}

fn expect_status(cfg: &Value) -> Vec<u16> {
    cfg.get("expect_status")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_u64().map(|n| n as u16)).collect())
        .unwrap_or_else(|| vec![200])
}

struct HttpPost;
#[async_trait(?Send)]
impl Adapter for HttpPost {
    fn name(&self) -> &'static str {
        "http-post"
    }
    async fn probe(&self, resource: &Row, env: &Env) -> ProbeResult {
        let spec = json!({ "input": {}, "timeout_ms": 15000 });
        let r = self.execute(resource, &spec, env).await;
        ProbeResult { ok: r.ok, latency_ms: r.latency_ms, detail: probe_detail(&r) }
    }
    async fn execute(&self, resource: &Row, spec: &Value, env: &Env) -> ExecResult {
        let cfg = cfg_of(resource);
        let tmo = timeout_ms(&cfg, spec, 15000);
        let mut body_map = cfg
            .get("input_body")
            .and_then(|v| v.as_object())
            .cloned()
            .unwrap_or_default();
        if let Some(inp) = spec.get("input").and_then(|v| v.as_object()) {
            for (k, v) in inp {
                body_map.insert(k.clone(), v.clone());
            }
        }
        let body = Value::Object(body_map).to_string();
        let url = cfg.get("url").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let headers = headers_of(&cfg);
        let (resp, latency_ms, timed_out, error) =
            fetch_with_timeout(env, "POST", &url, &headers, Some(&body), tmo).await;
        let (status, text) = match resp {
            Some(x) => x,
            None => return no_resp(latency_ms, timed_out, error),
        };
        if !expect_status(&cfg).contains(&status) {
            let e = format!("HTTP {}: {}", status, text.chars().take(200).collect::<String>());
            let cls = classify_http_error(status, &text).to_string();
            return ExecResult { ok: false, output: None, latency_ms, error: Some(e), error_class: Some(cls) };
        }
        let parsed: Option<Value> =
            if text.trim().is_empty() { None } else { serde_json::from_str(&text).ok() };
        if let Some(fail) = verify_path(&cfg, &parsed, &text, latency_ms) {
            return fail;
        }
        ExecResult {
            ok: true,
            latency_ms,
            output: Some(json!({ "status": status,
                "body": parsed.unwrap_or_else(|| Value::from(text.chars().take(2000).collect::<String>())) })),
            error: None,
            error_class: None,
        }
    }
}

struct HttpGet;
#[async_trait(?Send)]
impl Adapter for HttpGet {
    fn name(&self) -> &'static str {
        "http-get"
    }
    async fn probe(&self, resource: &Row, env: &Env) -> ProbeResult {
        // NOTE: probe uses this adapter's own execute (GET), never POST.
        let spec = json!({ "input": {}, "timeout_ms": 15000 });
        let r = self.execute(resource, &spec, env).await;
        ProbeResult { ok: r.ok, latency_ms: r.latency_ms, detail: probe_detail(&r) }
    }
    async fn execute(&self, resource: &Row, spec: &Value, env: &Env) -> ExecResult {
        let cfg = cfg_of(resource);
        let base = cfg.get("url").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let query = spec
            .get("input")
            .and_then(|i| i.get("query"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let url = if query.is_empty() {
            base.clone()
        } else {
            format!("{}{}{}", base, if base.contains('?') { "&" } else { "?" }, query)
        };
        let tmo = timeout_ms(&cfg, spec, 15000);
        let headers = headers_of(&cfg);
        let (resp, latency_ms, timed_out, error) =
            fetch_with_timeout(env, "GET", &url, &headers, None, tmo).await;
        let (status, text) = match resp {
            Some(x) => x,
            None => return no_resp(latency_ms, timed_out, error),
        };
        if !expect_status(&cfg).contains(&status) {
            let e = format!("HTTP {}: {}", status, text.chars().take(200).collect::<String>());
            let cls = classify_http_error(status, &text).to_string();
            return ExecResult { ok: false, output: None, latency_ms, error: Some(e), error_class: Some(cls) };
        }
        let parsed: Option<Value> =
            if text.trim().is_empty() { None } else { serde_json::from_str(&text).ok() };
        if let Some(fail) = verify_path(&cfg, &parsed, &text, latency_ms) {
            return fail;
        }
        ExecResult {
            ok: true,
            latency_ms,
            output: Some(json!({ "status": status,
                "body": parsed.unwrap_or_else(|| Value::from(text.chars().take(2000).collect::<String>())) })),
            error: None,
            error_class: None,
        }
    }
}

// --- self / claimed ------------------------------------------------------------

struct SelfAdapter;
#[async_trait(?Send)]
impl Adapter for SelfAdapter {
    fn name(&self) -> &'static str {
        "self"
    }
    async fn probe(&self, _resource: &Row, _env: &Env) -> ProbeResult {
        ProbeResult { ok: true, latency_ms: 0.0, detail: "self".to_string() }
    }
    async fn execute(&self, _resource: &Row, _spec: &Value, _env: &Env) -> ExecResult {
        ExecResult {
            ok: false,
            output: None,
            latency_ms: 0.0,
            error: Some("self adapter does not execute jobs".to_string()),
            error_class: Some("POLICY_BLOCKED".to_string()),
        }
    }
}

struct ClaimedAdapter;
#[async_trait(?Send)]
impl Adapter for ClaimedAdapter {
    fn name(&self) -> &'static str {
        "claimed"
    }
    fn claimed(&self) -> bool {
        true
    }
    async fn probe(&self, _resource: &Row, _env: &Env) -> ProbeResult {
        ProbeResult {
            ok: false,
            latency_ms: 0.0,
            detail: "delegated — probed via claimant observations".to_string(),
        }
    }
    async fn execute(&self, _resource: &Row, _spec: &Value, _env: &Env) -> ExecResult {
        ExecResult {
            ok: false,
            output: None,
            latency_ms: 0.0,
            error: Some("delegated execution — awaiting claimant".to_string()),
            error_class: Some("NO_ADAPTER".to_string()),
        }
    }
}

/// `adapterFor(resource)`: `adapters[cfg.adapter || resource.kind] || null`.
pub fn adapter_for(resource: &Row) -> Option<Arc<dyn Adapter>> {
    let cfg = cfg_of(resource);
    let name = cfg
        .get("adapter")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| resource.text_or("kind", ""));
    match name.as_str() {
        "ai-lane" => Some(Arc::new(AiLane)),
        "http-post" => Some(Arc::new(HttpPost)),
        "http-get" => Some(Arc::new(HttpGet)),
        "self" => Some(Arc::new(SelfAdapter)),
        "claimed" => Some(Arc::new(ClaimedAdapter)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adapter_names_known() {
        assert!(ADAPTER_NAMES.contains(&"claimed"));
        assert!(ADAPTER_NAMES.contains(&"ai-lane"));
    }

    #[test]
    fn classify_http() {
        assert_eq!(classify_http_error(429, ""), "RATE_LIMITED");
        assert_eq!(classify_http_error(401, ""), "AUTH_ERROR");
        assert_eq!(classify_http_error(500, ""), "SERVER_ERROR");
        assert_eq!(classify_http_error(400, ""), "BAD_RESPONSE");
        assert_eq!(classify_http_error(0, "fetch failed: network"), "NETWORK");
    }
}
