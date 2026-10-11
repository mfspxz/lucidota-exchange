//! eo/govern — deploy the Exchange governor worker.js via the Cloudflare
//! Workers API. Port of deploy-governor.py.
//!
//! Auth: the skill's dynamic credential (custom.cloudflare) via the local
//! authd surrogate flow (crate::creds) — raw keys are never touched, printed,
//! or logged. Steps: upload script (ES module + D1 binding) then set cron
//! triggers.

use serde_json::Value;

use super::OpError;
use crate::creds;

pub const ACCOUNT_ID: &str = "243aabef9144fd3b8dd334cb19319e75";
pub const SCRIPT_NAME: &str = "lucidota-exchange";
pub const D1_ID: &str = "0ea89801-9b0a-4efa-9d2f-170d7d285e69";
pub const API: &str = "https://api.cloudflare.com/client/v4";
pub const WORKER_SRC: &str = "/home/hatch/workspace/exchange/worker.js";
pub const CRON: &str = "*/5 * * * *";

fn boundary() -> String {
    // unique multipart boundary without a uuid crate
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("----exchangedeploy{t:x}{}", std::process::id())
}

/// Build the multipart upload body (pure, testable).
pub fn build_upload_body(boundary: &str, worker_src: &[u8]) -> Vec<u8> {
    let metadata = serde_json::json!({
        "main_module": "worker.js",
        "compatibility_date": "2026-10-01",
        "bindings": [
            {"type": "d1", "name": "EXCHANGE_DB", "id": D1_ID},
        ],
    });
    let mut body: Vec<u8> = Vec::new();
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"metadata\"\r\nContent-Type: application/json\r\n\r\n{}\r\n",
            serde_json::to_string(&metadata).unwrap()
        )
        .as_bytes(),
    );
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"worker.js\"; filename=\"worker.js\"\r\nContent-Type: application/javascript+module\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(worker_src);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    body
}

fn cf_put(path: &str, body: Vec<u8>, content_type: &str) -> Result<Value, OpError> {
    let client = super::http_client(120);
    let url = format!("{API}{path}");
    let resp = creds::cloudflare_send(
        &client,
        reqwest::Method::PUT,
        &url,
        Some(body),
        &[("Content-Type", content_type)],
    )?;
    let val: Value = resp
        .json()
        .map_err(|e| OpError::failed(format!("decode {path}: {e}")))?;
    Ok(val)
}

pub fn run(_input: Value) -> Result<Value, OpError> {
    let worker_src = std::fs::read(WORKER_SRC)
        .map_err(|e| OpError::failed(format!("read {WORKER_SRC}: {e}")))?;

    let bnd = boundary();
    let body = build_upload_body(&bnd, &worker_src);
    let r = cf_put(
        &format!("/accounts/{ACCOUNT_ID}/workers/scripts/{SCRIPT_NAME}"),
        body,
        &format!("multipart/form-data; boundary={bnd}"),
    )?;
    if r.get("success").and_then(|s| s.as_bool()) != Some(true) {
        let detail: String = serde_json::to_string(&r).unwrap_or_default().chars().take(2000).collect();
        return Err(OpError::failed(format!("UPLOAD FAILED: {detail}")));
    }
    let upload_result: String = serde_json::to_string(r.get("result").unwrap_or(&Value::Null))
        .unwrap_or_default()
        .chars()
        .take(300)
        .collect();

    let r2 = cf_put(
        &format!("/accounts/{ACCOUNT_ID}/workers/scripts/{SCRIPT_NAME}/schedules"),
        serde_json::json!([{"cron": CRON}]).to_string().into_bytes(),
        "application/json",
    )?;
    if r2.get("success").and_then(|s| s.as_bool()) != Some(true) {
        let detail: String = serde_json::to_string(&r2).unwrap_or_default().chars().take(2000).collect();
        return Err(OpError::failed(format!("SCHEDULE FAILED: {detail}")));
    }

    Ok(serde_json::json!({
        "deploy": "complete",
        "worker_js_bytes": worker_src.len(),
        "upload": "ok",
        "upload_result": upload_result,
        "cron": CRON,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multipart_body_shape() {
        let body = build_upload_body("BND", b"console.log(1);");
        let s = String::from_utf8(body).unwrap();
        assert!(s.contains("--BND\r\nContent-Disposition: form-data; name=\"metadata\""));
        assert!(s.contains("\"main_module\":\"worker.js\""));
        assert!(s.contains(&format!("\"id\":\"{D1_ID}\"")));
        assert!(s.contains("name=\"worker.js\"; filename=\"worker.js\""));
        assert!(s.contains("console.log(1);"));
        assert!(s.ends_with("--BND--\r\n"));
    }

    #[test]
    fn cf_constants() {
        assert_eq!(ACCOUNT_ID, "243aabef9144fd3b8dd334cb19319e75");
        assert_eq!(SCRIPT_NAME, "lucidota-exchange");
        assert_eq!(CRON, "*/5 * * * *");
    }
}
