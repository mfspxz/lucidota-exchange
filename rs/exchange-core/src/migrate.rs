//! Schema migration + seed — port of worker.js `migrate()` and `seed()`.
//!
//! The DDL is verbatim (CREATE TABLE IF NOT EXISTS / ensure-ADD-COLUMN /
//! CREATE INDEX IF NOT EXISTS), so a Rust-migrated database is identical
//! to a JS-migrated one. The ballot_policy default row and the v0.3
//! evidence backfill are preserved.

use crate::db::{Db, DbError, RowExt, Val};
use crate::util::{minutes_ago, now_iso};
use serde_json::{json, Value};

async fn cols_of(db: &dyn Db, t: &str) -> Result<std::collections::HashSet<String>, DbError> {
    let rows = db.query_all(&format!("PRAGMA table_info({})", t), &[]).await?;
    Ok(rows.iter().map(|c| c.text_or("name", "")).collect())
}

async fn ensure(db: &dyn Db, t: &str, col: &str, ddl: &str) -> Result<(), DbError> {
    let names = cols_of(db, t).await?;
    if !names.contains(col) {
        db.exec(&format!("ALTER TABLE {} ADD COLUMN {}", t, ddl), &[]).await?;
    }
    Ok(())
}

/// migrate: idempotent schema migration (safe to run on every boot).
pub async fn migrate(db: &dyn Db) -> Result<(), DbError> {
    // v0.3 columns
    ensure(db, "resources", "evidence_state", "evidence_state TEXT DEFAULT 'UNKNOWN'").await?;
    ensure(db, "resources", "evidence_source", "evidence_source TEXT").await?;
    ensure(db, "resources", "evidence_at", "evidence_at TEXT").await?;
    ensure(db, "resources", "cost_basis", "cost_basis TEXT DEFAULT 'unknown'").await?;
    ensure(db, "quotas", "measured", "measured INTEGER DEFAULT 0").await?;
    ensure(db, "quotas", "measured_at", "measured_at TEXT").await?;
    // v0.4 columns
    ensure(db, "resources", "throttled_until", "throttled_until TEXT").await?;
    ensure(db, "resources", "suspended_until", "suspended_until TEXT").await?;
    ensure(db, "resources", "credential_state", "credential_state TEXT DEFAULT 'unknown'").await?;
    ensure(db, "leases", "authority", "authority TEXT").await?;
    ensure(db, "leases", "expected_cost", "expected_cost REAL DEFAULT 0").await?;
    ensure(db, "leases", "quota_snapshot", "quota_snapshot TEXT").await?;
    ensure(db, "leases", "policy_version", "policy_version TEXT").await?;
    ensure(db, "leases", "expires_at", "expires_at TEXT").await?;
    ensure(db, "executions", "attempt_no", "attempt_no INTEGER").await?;
    ensure(db, "executions", "lease_id", "lease_id TEXT").await?;
    ensure(db, "executions", "failure_taxonomy", "failure_taxonomy TEXT").await?;
    ensure(db, "executions", "request_hash", "request_hash TEXT").await?;
    ensure(db, "receipts", "attempt_id", "attempt_id TEXT").await?;
    ensure(db, "receipts", "policy_version", "policy_version TEXT").await?;
    ensure(db, "receipts", "lease", "lease TEXT").await?;
    ensure(db, "receipts", "cost_before", "cost_before REAL").await?;
    ensure(db, "receipts", "cost_after", "cost_after REAL").await?;
    ensure(db, "receipts", "health_before", "health_before TEXT").await?;
    ensure(db, "receipts", "health_after", "health_after TEXT").await?;
    ensure(db, "receipts", "fallback_chain", "fallback_chain TEXT").await?;
    ensure(db, "jobs", "defer_count", "defer_count INTEGER DEFAULT 0").await?;
    db.exec(
        "CREATE TABLE IF NOT EXISTS evidence_log (
           id INTEGER PRIMARY KEY AUTOINCREMENT, at TEXT DEFAULT (datetime('now')),
           resource_id TEXT, from_state TEXT, to_state TEXT, trigger TEXT, detail TEXT)",
        &[],
    )
    .await?;
    db.exec(
        "CREATE TABLE IF NOT EXISTS discovery_sources (
           id TEXT PRIMARY KEY, name TEXT, url TEXT, kind TEXT, enabled INTEGER DEFAULT 1,
           last_fetched_at TEXT, last_result TEXT)",
        &[],
    )
    .await?;
    // v0.6 — Worker Protocol v1
    db.exec(
        "CREATE TABLE IF NOT EXISTS workers (
           worker_id TEXT PRIMARY KEY, secret_hash TEXT NOT NULL,
           trust_class TEXT DEFAULT 'remote', name TEXT,
           created_at TEXT DEFAULT (datetime('now')), last_seen_at TEXT,
           disabled INTEGER DEFAULT 0)",
        &[],
    )
    .await?;
    db.exec(
        "CREATE TABLE IF NOT EXISTS claims (
           claim_id TEXT PRIMARY KEY, job_id TEXT NOT NULL, worker_id TEXT NOT NULL,
           lease_id TEXT NOT NULL UNIQUE, resource_id TEXT NOT NULL,
           attempt_no INTEGER DEFAULT 1, status TEXT DEFAULT 'active',
           claimed_at TEXT DEFAULT (datetime('now')), lease_expiry TEXT NOT NULL,
           policy_snapshot TEXT, receipt_requirements TEXT,
           idempotency_key TEXT, input_hash TEXT,
           worker_outcome TEXT, completed_at TEXT, completion_summary TEXT)",
        &[],
    )
    .await?;
    db.exec("CREATE INDEX IF NOT EXISTS idx_claims_lease ON claims(lease_id)", &[]).await?;
    db.exec("CREATE INDEX IF NOT EXISTS idx_claims_job ON claims(job_id)", &[]).await?;
    db.exec(
        "CREATE TABLE IF NOT EXISTS accounting (
           id TEXT PRIMARY KEY, execution_id TEXT NOT NULL, claim_id TEXT,
           job_id TEXT, resource_id TEXT, measured_cost REAL DEFAULT 0,
           cost_basis TEXT DEFAULT 'unknown', quota_effect TEXT,
           recorded_at TEXT DEFAULT (datetime('now')))",
        &[],
    )
    .await?;
    ensure(db, "executions", "claim_id", "claim_id TEXT").await?;
    ensure(db, "executions", "requested_model", "requested_model TEXT").await?;
    ensure(db, "executions", "actual_model", "actual_model TEXT").await?;
    ensure(db, "executions", "model_match", "model_match INTEGER").await?;
    ensure(db, "executions", "usage_json", "usage_json TEXT").await?;
    ensure(db, "resources", "claimant_trust_required", "claimant_trust_required TEXT DEFAULT 'any'").await?;
    // C3: completing_at
    ensure(db, "claims", "completing_at", "completing_at TEXT").await?;
    ensure(db, "receipts", "claim_id", "claim_id TEXT").await?;
    ensure(db, "receipts", "worker_outcome", "worker_outcome TEXT").await?;
    ensure(db, "receipts", "ternary_verdict", "ternary_verdict INTEGER").await?;
    // v0.6.4 — voting
    db.exec(
        "CREATE TABLE IF NOT EXISTS ballots (
           id TEXT PRIMARY KEY, ballot_type TEXT NOT NULL, proposition TEXT NOT NULL,
           voters TEXT NOT NULL, quorum INTEGER NOT NULL, deadline_at TEXT NOT NULL,
           state TEXT DEFAULT 'open', verdict TEXT, tally TEXT,
           created_at TEXT DEFAULT (datetime('now')))",
        &[],
    )
    .await?;
    ensure(db, "receipts", "ballot_id", "ballot_id TEXT").await?;
    db.exec(
        "CREATE TABLE IF NOT EXISTS ballot_policy (
           id TEXT PRIMARY KEY, voters TEXT NOT NULL, quorum_rule TEXT DEFAULT 'majority',
           deadline_s INTEGER DEFAULT 600, tie_policy TEXT DEFAULT 'unknown')",
        &[],
    )
    .await?;
    db.exec(
        "CREATE TABLE IF NOT EXISTS envelope_rejections (
           id INTEGER PRIMARY KEY AUTOINCREMENT, at TEXT, route TEXT, reason TEXT,
           claim_id TEXT, actor TEXT)",
        &[],
    )
    .await?;
    db.exec(
        "CREATE TABLE IF NOT EXISTS seal_registry (
           seal TEXT PRIMARY KEY, first_seen INTEGER NOT NULL)",
        &[],
    )
    .await?;
    let pol = db.query_one("SELECT id FROM ballot_policy WHERE id='default'", &[]).await?;
    if pol.is_none() {
        db.exec(
            "INSERT INTO ballot_policy (id,voters,quorum_rule,deadline_s,tie_policy) VALUES (?,?,?,?,?)",
            &[
                Val::from("default"),
                Val::from(
                    serde_json::to_string(&vec![
                        "openai/gpt-oss-20b",
                        "z-ai/glm-5.3-flash",
                        "nvidia/nemotron-3.5-lightning-30b-a3b",
                    ])
                    .unwrap_or_default(),
                ),
                Val::from("majority"),
                Val::from(600i64),
                Val::from("unknown"),
            ],
        )
        .await?;
    }
    // v0.6.14: health_observations version-controlled
    db.exec(
        "CREATE TABLE IF NOT EXISTS health_observations (
           id INTEGER PRIMARY KEY AUTOINCREMENT,
           resource_id TEXT NOT NULL, ok INTEGER NOT NULL,
           latency_ms REAL, detail TEXT,
           observed_at TEXT DEFAULT (datetime('now')))",
        &[],
    )
    .await?;
    ensure(db, "health_observations", "resource_id", "resource_id TEXT").await?;
    ensure(db, "health_observations", "ok", "ok INTEGER").await?;
    ensure(db, "health_observations", "latency_ms", "latency_ms REAL").await?;
    ensure(db, "health_observations", "detail", "detail TEXT").await?;
    ensure(db, "health_observations", "observed_at", "observed_at TEXT DEFAULT (datetime('now'))").await?;
    db.exec(
        "CREATE INDEX IF NOT EXISTS idx_health_obs_resource_observed ON health_observations (resource_id, observed_at DESC)",
        &[],
    )
    .await?;
    db.exec(
        "CREATE INDEX IF NOT EXISTS idx_health_obs_resource_ok_observed ON health_observations (resource_id, ok, observed_at DESC)",
        &[],
    )
    .await?;
    db.exec("CREATE INDEX IF NOT EXISTS idx_receipts_created_at ON receipts (created_at DESC)", &[]).await?;
    // v0.6.15: read-audit index bundle
    db.exec("CREATE INDEX IF NOT EXISTS idx_executions_resource ON executions (resource_id)", &[]).await?;
    db.exec("CREATE INDEX IF NOT EXISTS idx_receipts_resource ON receipts (resource)", &[]).await?;
    db.exec("CREATE INDEX IF NOT EXISTS idx_claims_status_expiry ON claims (status, lease_expiry)", &[]).await?;
    db.exec("CREATE INDEX IF NOT EXISTS idx_jobs_created ON jobs (created_at DESC)", &[]).await?;
    db.exec("CREATE INDEX IF NOT EXISTS idx_executions_job ON executions (job_id)", &[]).await?;
    db.exec("CREATE INDEX IF NOT EXISTS idx_leases_job ON leases (job_id)", &[]).await?;
    db.exec("CREATE INDEX IF NOT EXISTS idx_leases_resource ON leases (resource_id)", &[]).await?;
    db.exec("CREATE INDEX IF NOT EXISTS idx_resources_provider ON resources (provider_id)", &[]).await?;
    db.exec("CREATE INDEX IF NOT EXISTS idx_leases_state_created ON leases (state, created_at)", &[]).await?;
    let done = db
        .query_one("SELECT COUNT(*) c FROM evidence_log WHERE trigger='backfill-v0.3'", &[])
        .await?;
    if done.map(|r| r.i64_or("c", 0)).unwrap_or(0) == 0 {
        backfill_evidence(db).await?;
    }
    Ok(())
}

async fn backfill_evidence(db: &dyn Db) -> Result<(), DbError> {
    let rs = db.query_all("SELECT id,state FROM resources", &[]).await?;
    for r in rs {
        let rid = r.text_or("id", "");
        let ok7 = db
            .query_one(
                "SELECT COUNT(*) c FROM health_observations WHERE resource_id=? AND ok=1 AND observed_at>?",
                &[Val::from(rid.clone()), Val::from(minutes_ago(7 * 24 * 60))],
            )
            .await
            .map(|o| o.map(|r| r.i64_or("c", 0)).unwrap_or(0))
            .unwrap_or(0);
        let ok24 = db
            .query_one(
                "SELECT COUNT(*) c FROM health_observations WHERE resource_id=? AND ok=1 AND observed_at>?",
                &[Val::from(rid.clone()), Val::from(minutes_ago(24 * 60))],
            )
            .await
            .map(|o| o.map(|r| r.i64_or("c", 0)).unwrap_or(0))
            .unwrap_or(0);
        let exec_ok = db
            .query_one(
                "SELECT COUNT(*) c FROM executions WHERE resource_id=? AND result='ok'",
                &[Val::from(rid.clone())],
            )
            .await
            .map(|o| o.map(|r| r.i64_or("c", 0)).unwrap_or(0))
            .unwrap_or(0);
        let (ev, basis) = if rid == "exchange-self" {
            ("VERIFIED", "measured")
        } else if ok7 >= 3 && ok24 >= 1 && r.text_or("state", "") == "AVAILABLE" && exec_ok >= 1 {
            ("VERIFIED", "measured")
        } else if ok24 >= 1 && r.text_or("state", "") == "AVAILABLE" && exec_ok >= 1 {
            ("AVAILABLE", "measured")
        } else if exec_ok >= 1 {
            ("FREE", "measured")
        } else if ok7 >= 1 {
            ("ACCESSIBLE", "unknown")
        } else {
            ("UNKNOWN", "unknown")
        };
        db.exec(
            "UPDATE resources SET evidence_state=?, evidence_source=?, evidence_at=?, cost_basis=? WHERE id=?",
            &[
                Val::from(ev),
                Val::from("backfill-v0.3"),
                Val::from(now_iso()),
                Val::from(basis),
                Val::from(rid.clone()),
            ],
        )
        .await?;
        db.exec(
            "INSERT INTO evidence_log (resource_id,from_state,to_state,trigger,detail) VALUES (?,?,?,?,?)",
            &[
                Val::from(rid.clone()),
                Val::from("UNKNOWN"),
                Val::from(ev),
                Val::from("backfill-v0.3"),
                Val::from(format!(
                    "ok7d={} ok24h={} exec_ok={} health={}",
                    ok7,
                    ok24,
                    exec_ok,
                    r.text_or("state", "")
                )),
            ],
        )
        .await?;
    }
    Ok(())
}

// --- SEED --------------------------------------------------------------------

struct SeedProvider {
    id: &'static str,
    name: &'static str,
    kind: &'static str,
    state: &'static str,
    notes: &'static str,
}
struct SeedResource {
    id: &'static str,
    provider_id: &'static str,
    name: &'static str,
    kind: &'static str,
    state: &'static str,
    cost_class: &'static str,
    sovereignty: &'static str,
    config: Value,
}

fn seed_providers() -> Vec<SeedProvider> {
    vec![
        SeedProvider { id: "cloudflare", name: "Cloudflare", kind: "edge", state: "UNKNOWN",
            notes: "Workers/D1/KV under account 243aabef9144fd3b8dd334cb19319e75 — state observed via API, not asserted" },
        SeedProvider { id: "ai-lanes", name: "Free AI inference lanes", kind: "inference-pool", state: "UNKNOWN",
            notes: "keyless, untrusted, fragments-only" },
        SeedProvider { id: "github", name: "GitHub", kind: "ci", state: "UNKNOWN",
            notes: "Actions/CI availability UNKNOWN — no token in this environment; registration path exists, access does not" },
    ]
}

fn seed_resources() -> Vec<SeedResource> {
    vec![
        SeedResource { id: "exchange-self", provider_id: "cloudflare", name: "lucidota-exchange",
            kind: "worker", state: "AVAILABLE", cost_class: "free", sovereignty: "self-hosted",
            config: json!({ "adapter": "self", "capabilities": ["orchestrate", "govern", "score"] }) },
        SeedResource { id: "lane-pollinations", provider_id: "ai-lanes", name: "pollinations",
            kind: "ai-lane", state: "UNKNOWN", cost_class: "free", sovereignty: "external",
            config: json!({ "adapter": "ai-lane", "baseUrl": "https://text.pollinations.ai/openai",
                "model": "openai", "rpmLimit": 30, "timeoutMs": 20000, "trust": "untrusted", "fragmentsOnly": true }) },
        SeedResource { id: "lane-kilo", provider_id: "ai-lanes", name: "kilo",
            kind: "ai-lane", state: "UNKNOWN", cost_class: "free", sovereignty: "external",
            config: json!({ "adapter": "ai-lane", "baseUrl": "https://api.kilo.ai/api/gateway",
                "model": "kilo-auto/free", "rpmLimit": 10, "timeoutMs": 20000, "trust": "untrusted", "fragmentsOnly": true }) },
        SeedResource { id: "lane-ovh", provider_id: "ai-lanes", name: "ovh",
            kind: "ai-lane", state: "UNKNOWN", cost_class: "free", sovereignty: "external",
            config: json!({ "adapter": "ai-lane", "baseUrl": "https://oai.endpoints.kepler.ai.cloud.ovh.net/v1",
                "model": "Qwen3.5-9B", "rpmLimit": 2, "timeoutMs": 20000, "trust": "untrusted", "fragmentsOnly": true }) },
        SeedResource { id: "lane-llm7", provider_id: "ai-lanes", name: "llm7",
            kind: "ai-lane", state: "UNKNOWN", cost_class: "free", sovereignty: "external",
            config: json!({ "adapter": "ai-lane", "baseUrl": "https://api.llm7.io/v1",
                "model": "default", "rpmLimit": 2, "timeoutMs": 60000, "trust": "untrusted", "fragmentsOnly": true }) },
        SeedResource { id: "ctcc-heartbeat", provider_id: "cloudflare", name: "ctcc-router",
            kind: "worker", state: "UNKNOWN", cost_class: "free", sovereignty: "self-hosted",
            config: json!({ "adapter": "http-post", "url": "https://ctcc-router.cutecity.workers.dev/ingest",
                "method": "POST", "timeoutMs": 15000, "expect_status": [200, 202], "trust": "self" }) },
        SeedResource { id: "cutecity-health", provider_id: "cloudflare", name: "cutecity.cc health",
            kind: "http-endpoint", state: "UNKNOWN", cost_class: "free", sovereignty: "self-hosted",
            config: json!({ "adapter": "http-get", "url": "https://cutecity.cc/api/health",
                "method": "GET", "timeoutMs": 15000, "expect_status": [200],
                "verify_json_path": "database", "verify_equals": true, "trust": "self" }) },
    ]
}

/// seed: insert the default providers/resources/capabilities/quotas/policies.
pub async fn seed(db: &dyn Db) -> Result<Value, DbError> {
    for p in seed_providers() {
        db.exec(
            "INSERT OR IGNORE INTO providers (id,name,kind,state,notes) VALUES (?,?,?,?,?)",
            &[Val::from(p.id), Val::from(p.name), Val::from(p.kind), Val::from(p.state), Val::from(p.notes)],
        )
        .await?;
    }
    for r in seed_resources() {
        db.exec(
            "INSERT OR IGNORE INTO resources (id,provider_id,name,kind,state,cost_class,sovereignty,config,evidence_state,evidence_source,evidence_at,cost_basis) VALUES (?,?,?,?,?,?,?,?,?,?,?,?)",
            &[
                Val::from(r.id), Val::from(r.provider_id), Val::from(r.name), Val::from(r.kind),
                Val::from(r.state), Val::from(r.cost_class), Val::from(r.sovereignty),
                Val::from(r.config.to_string()),
                Val::from("DISCOVERED"), Val::from("seed"), Val::from(now_iso()), Val::from("unknown"),
            ],
        )
        .await?;
    }
    let caps: Vec<(&str, &str, &str)> = vec![
        ("exchange-self", "orchestrate", "{}"),
        ("exchange-self", "govern", "{}"),
        ("exchange-self", "score", "{}"),
        ("lane-pollinations", "inference", "{\"context\":\"32k\",\"stream\":true}"),
        ("lane-kilo", "inference", "{\"context\":\"128k\",\"stream\":true}"),
        ("lane-ovh", "inference", "{\"context\":\"32k\",\"stream\":true}"),
        ("lane-llm7", "inference", "{\"context\":\"64k\",\"stream\":true}"),
        ("ctcc-heartbeat", "ingest", "{\"format\":\"heartbeat\"}"),
        ("cutecity-health", "http-check", "{\"verifies\":\"database\"}"),
    ];
    for (rid, cap, detail) in caps {
        db.exec(
            "INSERT OR IGNORE INTO resource_capabilities (resource_id,capability,detail) VALUES (?,?,?)",
            &[Val::from(rid), Val::from(cap), Val::from(detail)],
        )
        .await?;
    }
    for (rid, rpm) in [("lane-pollinations", 30), ("lane-kilo", 10), ("lane-ovh", 2), ("lane-llm7", 2)] {
        db.exec(
            "INSERT OR IGNORE INTO quotas (resource_id,limit_per_min) VALUES (?,?)",
            &[Val::from(rid), Val::from(rpm)],
        )
        .await?;
    }
    let policies: Vec<(&str, &str, Value, &str)> = vec![
        ("no-secrets-to-keyless", "No secrets to keyless lanes",
            json!({ "deny_when": { "resource_trust": "untrusted", "data_class_in": ["sensitive", "secret"] } }), "deny"),
        ("zero-spend", "Default authorized spend $0",
            json!({ "deny_when": { "cost_gt": "job.max_cost" } }), "deny"),
    ];
    for (id, name, rule, effect) in policies {
        db.exec(
            "INSERT OR IGNORE INTO policies (id,name,rule,effect) VALUES (?,?,?,?)",
            &[Val::from(id), Val::from(name), Val::from(rule.to_string()), Val::from(effect)],
        )
        .await?;
    }
    Ok(json!({ "seeded": true }))
}
