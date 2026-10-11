//! Torture: disproof corroboration quorum games.
//!
//! Port of `tests/torture-prune.mjs` (Finding 1 re-attack, NOVEL vectors).
//! v0.6.16: corroboration = COUNT(DISTINCT eligible worker_id); disabled
//! workers excluded; prune is one atomic batch with deterministic
//! INSERT..ON CONFLICT DO NOTHING.

mod harness;

use exchange_core::db::{Db, RowExt, Val};
use exchange_core::env::Env;
use exchange_core::routes::{handle, Request};
use exchange_core::util::{now_iso, sha256hex};
use harness::{fresh_db, test_env, SqliteDb};
use serde_json::{json, Value};
use std::collections::HashMap;

const TEST_KEY_HEX: &str = "5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a";

fn mint_envelope(claim_id: &str) -> Value {
    let key = hex::decode(TEST_KEY_HEX).unwrap();
    let mut payload = serde_json::Map::new();
    payload.insert("kind".to_string(), Value::from("completion"));
    let map = pl::envelope::wrap_with(&key, "luci", claim_id, "established", payload, "GENESIS", &now_iso())
        .expect("mint envelope");
    Value::Object(map)
}

fn post(path: &str, body: Value, headers: HashMap<String, String>) -> Request {
    Request {
        method: "POST".to_string(),
        path: path.to_string(),
        query: HashMap::new(),
        headers,
        body: Some(body),
    }
}

fn worker_headers(wid: &str, secret: &str) -> HashMap<String, String> {
    let mut h = HashMap::new();
    h.insert("x-worker-id".to_string(), wid.to_string());
    h.insert("x-worker-secret".to_string(), secret.to_string());
    h.insert("content-type".to_string(), "application/json".to_string());
    h
}

fn test_env_with_key() -> Env {
    let mut e = test_env();
    e.secrets.insert("PL_SIG_KEY".to_string(), TEST_KEY_HEX.to_string());
    e
}

fn future_iso() -> String {
    (chrono::Utc::now() + chrono::Duration::hours(1))
        .format("%Y-%m-%dT%H:%M:%S%.3fZ")
        .to_string()
}

const SECRETS: [(&str, &str); 4] = [
    ("w1", "s3cret"),
    ("w2", "s3cret2"),
    ("w3", "s3cret3"),
    ("w4", "s3cret4"),
];

async fn seed_prune_db(db: &SqliteDb) {
    let f = future_iso();
    db.exec(
        "INSERT INTO providers (id,name,kind,state) VALUES (?,?,?,?)",
        &[Val::from("p1"), Val::from("Test"), Val::from("test"), Val::from("UP")],
    )
    .await
    .unwrap();
    db.exec(
        "INSERT INTO resources (id,provider_id,name,kind,state,cost_class,config) VALUES (?,?,?,?,?,?,?)",
        &[Val::from("r1"), Val::from("p1"), Val::from("TestRes"), Val::from("test"),
          Val::from("AVAILABLE"), Val::from("free"), Val::from("{}")],
    )
    .await
    .unwrap();
    db.exec("INSERT INTO quotas (resource_id,limit_per_min) VALUES (?,?)",
        &[Val::from("r1"), Val::from(60i64)]).await.unwrap();
    for (wid, sec) in SECRETS {
        db.exec("INSERT INTO workers (worker_id,secret_hash) VALUES (?,?)",
            &[Val::from(wid), Val::from(sha256hex(sec))]).await.unwrap();
    }
    for j in ["jA1", "jA2", "jA3", "jA4"] {
        db.exec(
            "INSERT INTO jobs (id,organelle,capability,spec,state,input_hash) VALUES (?,?,?,?,?,?)",
            &[Val::from(j), Val::from("test"), Val::from("inference"),
              Val::from(json!({"capability": "inference", "input": {"model": "m1"}}).to_string()),
              Val::from("running"), Val::from("H")],
        )
        .await
        .unwrap();
    }
    db.exec(
        "INSERT INTO jobs (id,organelle,capability,spec,state,input_hash) VALUES (?,?,?,?,?,?)",
        &[Val::from("jB"), Val::from("test"), Val::from("inference"),
          Val::from(json!({"capability": "inference", "input": {"model": "m1"}}).to_string()),
          Val::from("deferred"), Val::from("H")],
    )
    .await
    .unwrap();
    for (j, w, c, l, e) in [
        ("jA1", "w1", "cA1", "lA1", "eA1"),
        ("jA2", "w2", "cA2", "lA2", "eA2"),
        ("jA3", "w3", "cA3", "lA3", "eA3"),
        ("jA4", "w4", "cA4", "lA4", "eA4"),
    ] {
        db.exec(
            "INSERT INTO leases (id,job_id,resource_id,state,expires_at) VALUES (?,?,?,?,?)",
            &[Val::from(l), Val::from(j), Val::from("r1"), Val::from("active"), Val::from(f.clone())],
        )
        .await
        .unwrap();
        db.exec(
            "INSERT INTO executions (id,job_id,resource_id,claim_id,request_hash) VALUES (?,?,?,?,?)",
            &[Val::from(e), Val::from(j), Val::from("r1"), Val::from(c), Val::from("H")],
        )
        .await
        .unwrap();
        db.exec(
            "INSERT INTO claims (claim_id,job_id,worker_id,lease_id,resource_id,status,lease_expiry,policy_snapshot) VALUES (?,?,?,?,?,?,?,?)",
            &[Val::from(c), Val::from(j), Val::from(w), Val::from(l), Val::from("r1"),
              Val::from("active"), Val::from(f.clone()), Val::from("{}")],
        )
        .await
        .unwrap();
    }
}

async fn job_state(db: &SqliteDb, id: &str) -> String {
    db.query_one("SELECT state FROM jobs WHERE id=?", &[Val::from(id.to_string())])
        .await
        .unwrap()
        .map(|r| r.text_or("state", ""))
        .unwrap_or_default()
}

async fn prune_count(db: &SqliteDb) -> i64 {
    db.query_one(
        "SELECT COUNT(*) c FROM receipts WHERE job_id='jB' AND json_extract(decision,'$.pruned')=1",
        &[],
    )
    .await
    .unwrap()
    .map(|r| r.i64_or("c", 0))
    .unwrap_or(0)
}

async fn neg1(db: &SqliteDb, env: &Env, wid: &str, claim_id: &str) -> exchange_core::routes::Response {
    let secret = SECRETS.iter().find(|(w, _)| *w == wid).map(|(_, s)| *s).unwrap();
    let body = json!({
        "worker_id": wid, "worker_secret": secret,
        "claim_id": claim_id,
        "worker_outcome": "SUCCEEDED",
        "output": { "content": "checked: claim is false", "ternary_verdict": -1 },
        "envelope": mint_envelope(claim_id),
    });
    handle(db, &post("/v1/complete", body, worker_headers(wid, secret)), env).await
}

/// P1: sequential 3-worker quorum; third -1 must not double-prune.
#[tokio::test]
async fn p1_sequential_quorum_no_double_prune() {
    let db = fresh_db().await;
    seed_prune_db(&db).await;
    let env = test_env_with_key();
    // w1: -1 → proposal only (1 witness), no prune.
    let r1 = neg1(&db, &env, "w1", "cA1").await;
    assert_eq!(r1.status, 200, "w1 -1: {:?}", r1.body);
    assert_eq!(job_state(&db, "jB").await, "deferred", "jB stays deferred after 1 witness");
    assert_eq!(prune_count(&db).await, 0);
    // w2: -1 → quorum (2 distinct witnesses) → prune jB.
    let r2 = neg1(&db, &env, "w2", "cA2").await;
    assert_eq!(r2.status, 200, "w2 -1: {:?}", r2.body);
    assert_eq!(job_state(&db, "jB").await, "failed", "jB pruned after 2 witnesses");
    assert_eq!(prune_count(&db).await, 1, "exactly 1 prune receipt");
    // w3: -1 → must NOT double-prune (jB already failed; conditional UPDATE).
    let r3 = neg1(&db, &env, "w3", "cA3").await;
    assert_eq!(r3.status, 200);
    assert_eq!(prune_count(&db).await, 1, "still exactly 1 prune receipt after 3rd -1");
}

/// P2/P3: disabled worker cannot propose; its -1 doesn't count.
#[tokio::test]
async fn p2_disabled_worker_cannot_propose() {
    let db = fresh_db().await;
    seed_prune_db(&db).await;
    let env = test_env_with_key();
    // Disable w2.
    db.exec("UPDATE workers SET disabled=1 WHERE worker_id='w2'", &[]).await.unwrap();
    // w1: -1 → proposal (1 witness).
    neg1(&db, &env, "w1", "cA1").await;
    // w2 (disabled): -1 → auth fails (403), no proposal.
    let r = neg1(&db, &env, "w2", "cA2").await;
    assert_eq!(r.status, 403, "disabled worker rejected: {:?}", r.body);
    assert_eq!(job_state(&db, "jB").await, "deferred", "jB not pruned — disabled witness excluded");
    // w3: -1 → quorum with w1 (2 eligible witnesses) → prune.
    neg1(&db, &env, "w3", "cA3").await;
    assert_eq!(job_state(&db, "jB").await, "failed");
}

/// P6: same worker -1 on three of its own jobs — still one witness.
#[tokio::test]
async fn p6_same_worker_one_witness() {
    let db = fresh_db().await;
    seed_prune_db(&db).await;
    let env = test_env_with_key();
    // w1 completes cA1 with -1 (proposal).
    neg1(&db, &env, "w1", "cA1").await;
    assert_eq!(job_state(&db, "jB").await, "deferred");
    // w1 cannot complete cA2 (belongs to w2) — forgery. Instead, give w1
    // two more of its own claims on the same input_hash.
    for (j, c, l, e) in [("jA5", "cA5", "lA5", "eA5"), ("jA6", "cA6", "lA6", "eA6")] {
        let f = future_iso();
        db.exec(
            "INSERT INTO jobs (id,organelle,capability,spec,state,input_hash) VALUES (?,?,?,?,?,?)",
            &[Val::from(j), Val::from("test"), Val::from("inference"),
              Val::from(json!({"capability": "inference", "input": {"model": "m1"}}).to_string()),
              Val::from("running"), Val::from("H")],
        ).await.unwrap();
        db.exec(
            "INSERT INTO leases (id,job_id,resource_id,state,expires_at) VALUES (?,?,?,?,?)",
            &[Val::from(l), Val::from(j), Val::from("r1"), Val::from("active"), Val::from(f.clone())],
        ).await.unwrap();
        db.exec(
            "INSERT INTO executions (id,job_id,resource_id,claim_id,request_hash) VALUES (?,?,?,?,?)",
            &[Val::from(e), Val::from(j), Val::from("r1"), Val::from(c), Val::from("H")],
        ).await.unwrap();
        db.exec(
            "INSERT INTO claims (claim_id,job_id,worker_id,lease_id,resource_id,status,lease_expiry,policy_snapshot) VALUES (?,?,?,?,?,?,?,?)",
            &[Val::from(c), Val::from(j), Val::from("w1"), Val::from(l), Val::from("r1"),
              Val::from("active"), Val::from(f), Val::from("{}")],
        ).await.unwrap();
        neg1(&db, &env, "w1", c).await;
    }
    // Three -1s from the SAME worker = 1 distinct witness → no prune.
    assert_eq!(job_state(&db, "jB").await, "deferred", "same worker x3 = 1 witness, no prune");
    // A second DISTINCT worker tips the quorum.
    neg1(&db, &env, "w2", "cA2").await;
    assert_eq!(job_state(&db, "jB").await, "failed", "2 distinct witnesses → prune");
}
