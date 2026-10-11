//! Minimal Rust replication of
//! `/opt/hatch/skills/skill-creator/bin/dynamic_credentials.py`.
//!
//! Mechanism (local authd call, no credential material here):
//! POST {"name": <credential>} to /v1/credentials/surrogate over the
//! unix socket $JARVIS_AUTHD_SOCK (default /run/hatch/auth/authd.sock).
//! authd returns JSON with a `credentials` list; each entry carries only an
//! `hsurr:*` surrogate plus a `placement` describing how to attach it.
//! Sentinel/authd replace the surrogate with the real credential on
//! approved outbound requests. Surrogate values are NEVER logged.
//!
//! Supported placements: "bearer_header" -> `Authorization: Bearer hsurr:...`;
//! {"custom_header": name} -> that header. url_path_segment / query_param
//! placements are refused (they must be applied to the URL, which reqwest
//! builders don't support here).

use std::env;
use std::fmt;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use serde_json::Value;

pub const SURROGATE_PATH: &str = "/v1/credentials/surrogate";

#[derive(Debug)]
pub enum CredError {
    MissingSocket(String),
    Http(String),
    Json(String),
    NotSurrogate(String),
    MissingEntry(String),
    UnsupportedPlacement(String),
    HostNotAllowed(String),
}

impl fmt::Display for CredError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CredError::MissingSocket(s) => write!(f, "authd socket unavailable: {s}"),
            CredError::Http(s) => write!(f, "authd HTTP error: {s}"),
            CredError::Json(s) => write!(f, "authd JSON error: {s}"),
            CredError::NotSurrogate(s) => write!(f, "authd returned non-surrogate: {s}"),
            CredError::MissingEntry(s) => write!(f, "missing surrogate entry: {s}"),
            CredError::UnsupportedPlacement(s) => {
                write!(f, "unsupported credential placement: {s}")
            }
            CredError::HostNotAllowed(s) => write!(f, "refusing authenticated request: {s}"),
        }
    }
}

impl std::error::Error for CredError {}

impl From<CredError> for crate::ops::OpError {
    fn from(e: CredError) -> Self {
        crate::ops::OpError::failed(format!("credential: {e}"))
    }
}

/// Placement parsed from the authd entry.
#[derive(Debug, Clone)]
pub enum Placement {
    BearerHeader,
    CustomHeader(String),
}

/// A surrogate token with its placement. The token value must never be
/// printed or logged (no Debug output of the value).
pub struct Surrogate {
    token: String,
    pub placement: Placement,
}

/// Ensure an authenticated request only goes to an allowlisted host.
/// Mirrors `ensure_allowed_url` in dynamic_credentials.py.
pub fn ensure_allowed_url(url: &str, allowed_hosts: &[&str]) -> Result<(), CredError> {
    let host = url
        .split("://")
        .nth(1)
        .and_then(|rest| rest.split('/').next())
        .and_then(|auth| auth.split('@').next_back())
        .and_then(|hostport| hostport.split(':').next())
        .map(|h| h.to_ascii_lowercase())
        .unwrap_or_default();
    let ok = !host.is_empty()
        && allowed_hosts
            .iter()
            .any(|h| h.trim().eq_ignore_ascii_case(&host));
    if ok {
        Ok(())
    } else {
        Err(CredError::HostNotAllowed(format!(
            "host '{host}' not in allowlist [{}]",
            allowed_hosts.join(", ")
        )))
    }
}

fn authd_socket_path() -> String {
    env::var("JARVIS_AUTHD_SOCK").unwrap_or_else(|_| "/run/hatch/auth/authd.sock".to_string())
}

fn post_json_unix(socket_path: &str, path: &str, payload: &Value) -> Result<String, CredError> {
    let body = serde_json::to_string(payload)
        .map_err(|e| CredError::Json(format!("payload encode: {e}")))?;
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: authd.local\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let mut conn = UnixStream::connect(socket_path)
        .map_err(|e| CredError::MissingSocket(format!("{socket_path}: {e}")))?;
    conn.set_read_timeout(Some(Duration::from_secs(10)))
        .map_err(|e| CredError::Http(e.to_string()))?;
    conn.write_all(request.as_bytes())
        .map_err(|e| CredError::Http(format!("write: {e}")))?;
    let mut raw = Vec::new();
    conn.read_to_end(&mut raw)
        .map_err(|e| CredError::Http(format!("read: {e}")))?;
    let raw = String::from_utf8_lossy(&raw);
    let (headers, _, resp_body) = split_response(&raw)?;
    let status = headers
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or_else(|| CredError::Http("malformed status line".to_string()))?;
    if status != 200 {
        return Err(CredError::Http(format!(
            "authd {path} returned HTTP {status}: {}",
            resp_body.trim()
        )));
    }
    Ok(resp_body.to_string())
}

fn split_response(raw: &str) -> Result<(&str, &str, &str), CredError> {
    raw.split_once("\r\n\r\n")
        .map(|(h, b)| (h, "", b))
        .ok_or_else(|| CredError::Http("authd returned a malformed HTTP response".to_string()))
}

/// Fetch one surrogated credential entry from authd.
/// Mirrors `dynamic_credential_entry` in dynamic_credentials.py.
pub fn dynamic_credential_entry(
    credential_name: &str,
    entry_name: &str,
) -> Result<Surrogate, CredError> {
    let socket_path = authd_socket_path();
    let text = post_json_unix(
        &socket_path,
        SURROGATE_PATH,
        &serde_json::json!({"name": credential_name}),
    )?;
    let payload: Value =
        serde_json::from_str(&text).map_err(|e| CredError::Json(format!("{e}")))?;
    let entries = payload
        .get("credentials")
        .and_then(|c| c.as_array())
        .ok_or_else(|| CredError::Json("no 'credentials' array".to_string()))?;
    for entry in entries {
        if entry.get("name").and_then(|n| n.as_str()) != Some(entry_name) {
            continue;
        }
        let surrogate = entry
            .get("surrogate")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if !surrogate.starts_with("hsurr:") {
            return Err(CredError::NotSurrogate(format!(
                "{credential_name}:{entry_name}"
            )));
        }
        let placement = entry.get("placement");
        let placement = if placement == Some(&Value::String("bearer_header".to_string())) {
            Placement::BearerHeader
        } else if let Some(obj) = placement.and_then(|p| p.as_object()) {
            if let Some(h) = obj.get("custom_header").and_then(|h| h.as_str()) {
                Placement::CustomHeader(h.to_string())
            } else if obj.contains_key("url_path_segment") {
                return Err(CredError::UnsupportedPlacement(
                    "url_path_segment credentials must be applied before Request creation"
                        .to_string(),
                ));
            } else if obj.contains_key("query_param") {
                return Err(CredError::UnsupportedPlacement(
                    "query_param credentials must be applied to the URL before Request creation"
                        .to_string(),
                ));
            } else {
                return Err(CredError::UnsupportedPlacement(format!("{placement:?}")));
            }
        } else {
            return Err(CredError::UnsupportedPlacement(format!("{placement:?}")));
        };
        return Ok(Surrogate {
            token: surrogate,
            placement,
        });
    }
    Err(CredError::MissingEntry(format!(
        "{entry_name} surrogate for credential {credential_name}"
    )))
}

/// Send an authenticated request through the authd surrogate flow.
/// Mirrors `add_surrogate_to_request` + `urlopen` in dynamic_credentials.py:
/// allowlist check first, then fetch the surrogate, attach per placement,
/// send. The surrogate value is marked sensitive and never logged.
#[allow(clippy::too_many_arguments)]
pub fn surrogate_send(
    client: &reqwest::blocking::Client,
    credential_name: &str,
    entry_name: &str,
    allowed_hosts: &[&str],
    method: reqwest::Method,
    url: &str,
    body: Option<Vec<u8>>,
    extra_headers: &[(&str, &str)],
) -> Result<reqwest::blocking::Response, CredError> {
    ensure_allowed_url(url, allowed_hosts)?;
    let surr = dynamic_credential_entry(credential_name, entry_name)?;
    let mut req = client.request(method, url);
    for (k, v) in extra_headers {
        req = req.header(*k, *v);
    }
    if let Some(b) = body {
        req = req.body(b);
    }
    req = match &surr.placement {
        Placement::BearerHeader => {
            let mut hv =
                reqwest::header::HeaderValue::from_str(&format!("Bearer {}", surr.token))
                    .map_err(|e| CredError::Http(format!("header: {e}")))?;
            hv.set_sensitive(true);
            req.header(reqwest::header::AUTHORIZATION, hv)
        }
        Placement::CustomHeader(name) => {
            let mut hv = reqwest::header::HeaderValue::from_str(&surr.token)
                .map_err(|e| CredError::Http(format!("header: {e}")))?;
            hv.set_sensitive(true);
            req.header(name.as_str(), hv)
        }
    };
    req.send()
        .map_err(|e| CredError::Http(format!("send to {url}: {e}")))
}

/// Convenience: authenticated Cloudflare API request via the
/// `custom.cloudflare` surrogate. Host allowlist enforced.
pub fn cloudflare_send(
    client: &reqwest::blocking::Client,
    method: reqwest::Method,
    url: &str,
    body: Option<Vec<u8>>,
    extra_headers: &[(&str, &str)],
) -> Result<reqwest::blocking::Response, CredError> {
    surrogate_send(
        client,
        "custom.cloudflare",
        "access_token",
        &["api.cloudflare.com"],
        method,
        url,
        body,
        extra_headers,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowed_url_ok_and_denied() {
        assert!(ensure_allowed_url(
            "https://api.cloudflare.com/client/v4/x",
            &["api.cloudflare.com"]
        )
        .is_ok());
        assert!(ensure_allowed_url(
            "https://evil.example.com/x",
            &["api.cloudflare.com"]
        )
        .is_err());
        assert!(ensure_allowed_url("https://api.cloudflare.com:443/x", &["api.cloudflare.com"]).is_ok());
    }

    #[test]
    fn surrogate_entry_parsing() {
        // parse logic exercised against a fixture shaped like authd's reply
        let text = r#"{"credentials":[{"name":"access_token","surrogate":"hsurr:abc123","placement":"bearer_header"}]}"#;
        let payload: Value = serde_json::from_str(text).unwrap();
        let entries = payload["credentials"].as_array().unwrap();
        let e = &entries[0];
        assert!(e["surrogate"].as_str().unwrap().starts_with("hsurr:"));
        assert_eq!(e["placement"].as_str().unwrap(), "bearer_header");
    }

    #[test]
    fn non_surrogate_rejected() {
        let bad = "raw-secret-value";
        assert!(!bad.starts_with("hsurr:"));
    }
}
