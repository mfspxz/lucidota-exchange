//! exchange-worker: Cloudflare Workers entry point (workers-rs).
//!
//! Thin glue: translates Workers HTTP/scheduled events into
//! `exchange_core::routes` calls. All business logic lives in
//! exchange-core; this crate only bridges the platform APIs
//! (D1, fetch, env vars/secrets).

use exchange_core::db::{Db, DbError, Row, Val};
use exchange_core::env::{Env, HttpClient};
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;
use wasm_bindgen::JsValue;
use worker::*;

/// D1-backed `Db` implementation.
struct D1Db {
    db: D1Database,
}

fn d1_val(v: &serde_json::Value) -> Val {
    match v {
        serde_json::Value::Null => Val::Null,
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Val::Int(i)
            } else if let Some(f) = n.as_f64() {
                Val::Real(f)
            } else {
                Val::Null
            }
        }
        serde_json::Value::String(s) => Val::Text(s.clone()),
        serde_json::Value::Bool(b) => Val::Int(if *b { 1 } else { 0 }),
        _ => Val::Null,
    }
}

fn to_js(v: &Val) -> JsValue {
    match v {
        Val::Null => JsValue::NULL,
        Val::Int(i) => JsValue::from(*i as f64),
        Val::Real(f) => JsValue::from(*f),
        Val::Text(s) => JsValue::from_str(s),
        Val::Blob(b) => {
            let arr = js_sys::Uint8Array::from(b.as_slice());
            JsValue::from(arr)
        }
    }
}

#[async_trait(?Send)]
impl Db for D1Db {
    async fn query_one(&self, sql: &str, params: &[Val]) -> Result<Option<Row>, DbError> {
        let ps: Vec<JsValue> = params.iter().map(to_js).collect();
        let stmt = self.db.prepare(sql).bind(&ps).map_err(|e| DbError(e.to_string()))?;
        let res: Option<HashMap<String, serde_json::Value>> =
            stmt.first(None).await.map_err(|e| DbError(e.to_string()))?;
        Ok(res.map(|m| m.into_iter().map(|(k, v)| (k, d1_val(&v))).collect()))
    }

    async fn query_all(&self, sql: &str, params: &[Val]) -> Result<Vec<Row>, DbError> {
        let ps: Vec<JsValue> = params.iter().map(to_js).collect();
        let stmt = self.db.prepare(sql).bind(&ps).map_err(|e| DbError(e.to_string()))?;
        let res = stmt.all().await.map_err(|e| DbError(e.to_string()))?;
        let rows: Vec<HashMap<String, serde_json::Value>> =
            res.results().map_err(|e| DbError(e.to_string()))?;
        Ok(rows
            .into_iter()
            .map(|m| m.into_iter().map(|(k, v)| (k, d1_val(&v))).collect())
            .collect())
    }

    async fn exec(&self, sql: &str, params: &[Val]) -> Result<u64, DbError> {
        let ps: Vec<JsValue> = params.iter().map(to_js).collect();
        let stmt = self.db.prepare(sql).bind(&ps).map_err(|e| DbError(e.to_string()))?;
        let info = stmt.run().await.map_err(|e| DbError(e.to_string()))?;
        let changes = info.meta().map_err(|e| DbError(e.to_string()))?
            .and_then(|m| m.changes).unwrap_or(0) as u64;
        Ok(changes)
    }

    async fn batch(&self, stmts: Vec<(String, Vec<Val>)>) -> Result<(), DbError> {
        let mut batch: Vec<D1PreparedStatement> = Vec::with_capacity(stmts.len());
        for (sql, params) in &stmts {
            let ps: Vec<JsValue> = params.iter().map(to_js).collect();
            let stmt = self.db.prepare(sql).bind(&ps).map_err(|e| DbError(e.to_string()))?;
            batch.push(stmt);
        }
        self.db.batch(batch).await.map_err(|e| DbError(e.to_string()))?;
        Ok(())
    }
}

/// Workers fetch-backed `HttpClient` (for adapters).
struct FetchHttp;

#[async_trait(?Send)]
impl HttpClient for FetchHttp {
    async fn fetch(
        &self,
        method: &str,
        url: &str,
        headers: &[(String, String)],
        body: Option<&str>,
    ) -> Result<(u16, String), String> {
        let mut init = RequestInit::new();
        let m = match method.to_uppercase().as_str() {
            "POST" => Method::Post,
            "PUT" => Method::Put,
            "DELETE" => Method::Delete,
            "PATCH" => Method::Patch,
            "HEAD" => Method::Head,
            _ => Method::Get,
        };
        init.with_method(m);
        if let Some(b) = body {
            init.with_body(Some(JsValue::from_str(b)));
        }
        let mut hm = Headers::new();
        for (k, v) in headers {
            hm.set(k, v).map_err(|e| e.to_string())?;
        }
        init.with_headers(hm);
        let req = Request::new_with_init(url, &init).map_err(|e| e.to_string())?;
        let mut resp = Fetch::Request(req).send().await.map_err(|e| e.to_string())?;
        let status = resp.status_code();
        let text = resp.text().await.map_err(|e| e.to_string())?;
        Ok((status, text))
    }
}

fn build_env(env: &worker::Env) -> Result<Env, String> {
    let mut secrets = HashMap::new();
    for key in ["PL_SIG_KEY", "EXCHANGE_KEY"] {
        if let Ok(v) = env.secret(key) {
            secrets.insert(key.to_string(), v.to_string());
        }
    }
    // Provider secrets: pass through any secret starting with a known prefix.
    // (The adapters read config; secrets come from env by name.)
    Ok(Env {
        exchange_key: env.secret("EXCHANGE_KEY").ok().map(|s| s.to_string()),
        secrets,
        http: Arc::new(FetchHttp),
    })
}

async fn to_core_request(req: &mut Request) -> Result<exchange_core::routes::Request, String> {
    let url = req.url().map_err(|e| e.to_string())?;
    let parsed = url; // req.url() already returns a Url
    let mut query = HashMap::new();
    for (k, v) in parsed.query_pairs() {
        query.insert(k.to_string(), v.to_string());
    }
    let mut headers = HashMap::new();
    for (k, v) in req.headers().entries() {
        headers.insert(k.to_lowercase(), v);
    }
    let body = if req.method() == Method::Post || req.method() == Method::Put {
        let text = req.text().await.map_err(|e| e.to_string())?;
        if text.trim().is_empty() {
            None
        } else {
            Some(serde_json::from_str(&text).map_err(|e| e.to_string())?)
        }
    } else {
        None
    };
    Ok(exchange_core::routes::Request {
        method: req.method().to_string(),
        path: parsed.path().to_string(),
        query,
        headers,
        body,
    })
}

#[event(fetch)]
pub async fn main(req: Request, env: worker::Env, _ctx: Context) -> Result<Response> {
    let db = env.d1("EXCHANGE_DB").map_err(|e| Error::from(e.to_string()))?;
    let core_env = build_env(&env).map_err(|e| Error::from(e))?;
    let d1db = D1Db { db };
    let mut req = req;
    let core_req = to_core_request(&mut req).await.map_err(|e| Error::from(e))?;
    let core_resp = exchange_core::routes::handle(&d1db, &core_req, &core_env).await;
    let mut headers = Headers::new();
    headers.set("Content-Type", "application/json").unwrap();
    Ok(Response::from_json(&core_resp.body)
        .map_err(|e| Error::from(e.to_string()))?
        .with_status(core_resp.status)
        .with_headers(headers))
}

#[event(scheduled)]
pub async fn scheduled(_event: ScheduledEvent, env: worker::Env, _ctx: ScheduleContext) {
    let db = match env.d1("EXCHANGE_DB") {
        Ok(d) => d,
        Err(e) => {
            console_log!("exchange cron: no D1 binding: {}", e);
            return;
        }
    };
    let core_env = match build_env(&env) {
        Ok(e) => e,
        Err(e) => {
            console_log!("exchange cron: env error: {}", e);
            return;
        }
    };
    let d1db = D1Db { db };
    match exchange_core::routes::scheduled(&d1db, &core_env).await {
        Ok(out) => console_log!("exchange cron: {}", serde_json::to_string(&out).unwrap_or_default()),
        Err(e) => console_log!("exchange cron fault: {}", e.0),
    }
}
