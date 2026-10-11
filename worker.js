/**
 * lucidota-exchange — LUCIDOTA Free Compute Exchange v0.4 — CONTROL PLANE
 *
 * The fundamental loop: DISCOVER → PROVE → AUTHORIZE → RESERVE → EXECUTE →
 * VERIFY → ACCOUNT → LEARN → REPLAN. The exchange autonomously maintains an
 * accurate model of what computation it can legitimately command, decides what
 * to use, proves it was allowed, survives reality changes, and leaves evidence.
 *
 * HARD PRE-EXECUTION GATES (in order, every one recorded, none bypassable by
 * score, retry, failover, urgency, or human convenience):
 *   IDENTITY → AUTHORITY → DATA → CAPABILITY → EVIDENCE → QUOTA → COST →
 *   INTEGRITY → LEASE → EXECUTE
 * Scoring (EFFICIENCY) is never authorization — it only ranks survivors.
 *
 * LEASE BEFORE SIDE EFFECT: after gates pass, a short-lived durable lease
 * (job_id + resource_id + authority + expected_cost + quota_snapshot +
 * policy_version + expiry ~60s) is created; critical invariants are revalidated
 * immediately before dispatch. Expired/lost/invalidated lease → discard +
 * replan. Never execute.
 *
 * IDEMPOTENCY + ACCOUNTING: executions ARE the attempts ledger. Every attempt
 * carries a failure taxonomy — RETRYABLE_FAILURE | PERMANENT_FAILURE |
 * POLICY_FAILURE | AUTH_FAILURE | QUOTA_FAILURE | CAPABILITY_FAILURE |
 * PROVIDER_FAILURE | VERIFICATION_FAILURE — never collapsed into "failed".
 *
 * RECEIPTS ARE THE SYSTEM OF RECORD: attempt_id, policy_version, lease
 * snapshot, quota_before/after, cost_before/after, health_before/after,
 * fallback_chain, gate trace — enough for another process to reconstruct why.
 *
 * ECONOMIC GOVERNOR: scoring includes effective_cost, expected_success, and
 * human-intervention cost. MAX_AUTHORIZED_SPEND defaults to $0. Paid resources
 * may register but can NEVER execute without explicit job.spend_authority > 0.
 *
 * LEARNING / POLICY SEPARATION: observations may write ONLY health, quota,
 * evidence, and accounting columns — never the policies table. Evidence
 * updates knowledge; policy controls permission. (See invariant comment above
 * observe().)
 *
 * EVIDENCE LIFECYCLE: DISCOVERED → ACCESSIBLE → FREE → QUOTA →
 * CURRENTLY AVAILABLE → VERIFIED. UNKNOWN = move on. PARKED = shelved.
 * Only AVAILABLE / VERIFIED execute. A provider listing is never capacity.
 *
 * Routes (all /v1/* need Authorization: Bearer <redacted>):
 *   GET  /                    → info
 *   GET  /v1/health           → exchange health (no auth)
 *   POST /v1/seed             → register providers/resources/policies (idempotent)
 *   GET  /v1/resources        → resources + health + quota + evidence state
 *   POST /v1/resources        → register a new resource (starts DISCOVERED, unusable)
 *   POST /v1/discover         → register a candidate lead (provider or resource)
 *   POST /v1/resources/:id/verify → run the evidence gauntlet now
 *   GET  /v1/resources/:id/evidence → evidence state + promotion/demotion log
 *   GET  /v1/capacity          → honest $0 commandable compute (usable resources only)
 *   POST /v1/job              → submit job contract → governor → lease → execute → receipt
 *   POST /v1/claim            → trusted claimant picks up delegated work (atomic lease claim)
 *   POST /v1/complete         → claimant reports a delegated execution (full accounting)
 *   GET  /v1/job/:id          → job + attempts + leases + receipts
 *   GET  /v1/jobs?state=      → job list (deferred queue visibility)
 *   GET  /v1/receipts?job=    → receipts
 *   GET  /v1/receipts/:id/verify → independent receipt verification
 *   POST /v1/observe          → manual health observation
 * Cron (every 30 min — cron "star-slash-30", burn-safe since 2026-10-09;
 * the old 5-min loop blew 5.3M D1 reads/day vs the 5M free quota)
 *   → OBSERVE → CLASSIFY → UPDATE → REPLAN loop:
 *   tier-based probing, throttle/exhaust recheck, credential-disable,
 *   provider-down detection, deferred-job replan, billing detection,
 *   discovery-source ingestion. No human prompt required.
 *
 * Truth law: never upgrade IN-CODE → DEPLOYED → WORKING without evidence.
 * Health states: AVAILABLE | DEGRADED | THROTTLED | EXHAUSTED | QUARANTINED | UNKNOWN
 * Evidence states: DISCOVERED | ACCESSIBLE | FREE | QUOTA | AVAILABLE | VERIFIED | UNKNOWN | PARKED
 * Job states: submitted | running | done | failed | blocked | deferred
 */

// SPEC-GAP-PL-007 (wave-32): envelope provenance at ingest. The verifier is a
// byte-faithful port of the lucidota-receipt/1 verify path (see envelope.js).
// NOTE for deploy: wrangler must bundle ./envelope.js with this module.
import { verifyEnvelope as verifyReceiptEnvelope } from "./envelope.js";

const VERSION = "0.6.16"; // v0.6.16: security hardening bundle (external review 2026-10-10): P1 distinct-eligible-worker disproof corroboration + atomic prune batch, P1 worker_outcome validated BEFORE the active->completing flip, P1/P2 D1 seal_registry durable replay arbiter (cross-isolate), VERIFY cron drift resolved (deployed==wrangler.toml==*/30 via read-only API; stale 5-min comments fixed) // v0.6.15: D1 read-burn index bundle (read audit): idx_executions_resource, idx_receipts_resource, idx_claims_status_expiry, idx_jobs_created, idx_executions_job, idx_leases_job, idx_leases_resource, idx_resources_provider, idx_leases_state_created (all IF NOT EXISTS, boot-applied); warrantedTierInfo memoized per resource per cron run; fresh path skips window computation (cheap indexed ok24 only) // v0.6.14: D1 read-burn code fixes (wave-29): warrantedTierInfo memoization (autoPromote+demoteCheck share one window computation per resource per cron run), health_observations retention (pruneHealthObservations, 30d, per scheduled run), health_observations CREATE TABLE version-controlled in migrate(), hot-path indexes applied at boot (0001 x2 + 0002 receipts(created_at)) // v0.6.13: migrate guard extended to scheduled() + */5 cron trigger enabled (reaction loop live) — migrate() once per isolate via __migrated guard (was every request: ~40 PRAGMAs + DDL reads/request, blew 75% of daily 5M rows_read free tier) // REDTEAM-2 tally fixes: ballot decision receipt excluded from its own tally (was phantom INVALID vote); verdict extraction takes LAST keyword in tail (was first — misread "Final verdict:" line) // REDTEAM-2 fixes: ballot_id stamped on ALL successful vote receipts (honest votes were invisible to tallies); INV7: SUCCEEDED+{} -> VERIFICATION_FAILURE for non-inference // REDTEAM-2 ballot fixes: ballot receipt job_id sentinel (was 500 on terminal transition); voter validation+slug-dedup pre-insert // 0.6.8: battery v4 R1 HIGH atomic completion idempotency (W2) // 0.6.7: deadline_s validated (400 on non-finite/non-positive, 30d cap) // voting amendments: /v1/ballots, receipt ballot_id, quorum policy // v0.6.3: ternary A1/A2/A3 // battery v4: INTEGRITY claimed-probe fix + max_cost<=spend_authority // battery v3 fixes: verify backpressure + claim expiry re-check // Worker Protocol v1 // battery v4 R1 HIGH: atomic completion idempotency (W2) + v0.6.7 voting // voting: deadline_s validated (400 on non-finite/non-positive, 30d cap): /v1/ballots, receipt ballot_id, quorum policy // v0.6.3: ternary A1/A2/A3 (ternary extraction, auto-prune, governor-minted needs_evidence) // battery v4: INTEGRITY claimed-probe fix + max_cost<=spend_authority // battery v3 fixes: verify backpressure + claim expiry re-check // Worker Protocol v1: sovereign governor, untrusted workers — governor scalability fix (backpressure + claim expiry filter + replan receipts)

/* ------------------------------------------------------------------ */
/* Seed registry — the known world. Registered idempotently via /v1/seed */
/* ------------------------------------------------------------------ */
const SEED = {
  providers: [
    { id: "cloudflare", name: "Cloudflare", kind: "edge", state: "UNKNOWN", notes: "Workers/D1/KV under account 243aabef9144fd3b8dd334cb19319e75 — state observed via API, not asserted" },
    { id: "ai-lanes", name: "Free AI inference lanes", kind: "inference-pool", state: "UNKNOWN", notes: "keyless, untrusted, fragments-only" },
    { id: "github", name: "GitHub", kind: "ci", state: "UNKNOWN", notes: "Actions/CI availability UNKNOWN — no token in this environment; registration path exists, access does not" },
  ],
  resources: [
    { id: "exchange-self", provider_id: "cloudflare", name: "lucidota-exchange", kind: "worker", state: "AVAILABLE", cost_class: "free", sovereignty: "self-hosted", config: { adapter: "self", capabilities: ["orchestrate", "govern", "score"] } },
    { id: "lane-pollinations", provider_id: "ai-lanes", name: "pollinations", kind: "ai-lane", state: "UNKNOWN", cost_class: "free", sovereignty: "external",
      config: { adapter: "ai-lane", baseUrl: "https://text.pollinations.ai/openai", model: "openai", rpmLimit: 30, timeoutMs: 20000, trust: "untrusted", fragmentsOnly: true } },
    { id: "lane-kilo", provider_id: "ai-lanes", name: "kilo", kind: "ai-lane", state: "UNKNOWN", cost_class: "free", sovereignty: "external",
      config: { adapter: "ai-lane", baseUrl: "https://api.kilo.ai/api/gateway", model: "kilo-auto/free", rpmLimit: 10, timeoutMs: 20000, trust: "untrusted", fragmentsOnly: true } },
    { id: "lane-ovh", provider_id: "ai-lanes", name: "ovh", kind: "ai-lane", state: "UNKNOWN", cost_class: "free", sovereignty: "external",
      config: { adapter: "ai-lane", baseUrl: "https://oai.endpoints.kepler.ai.cloud.ovh.net/v1", model: "Qwen3.5-9B", rpmLimit: 2, timeoutMs: 20000, trust: "untrusted", fragmentsOnly: true } },
    { id: "lane-llm7", provider_id: "ai-lanes", name: "llm7", kind: "ai-lane", state: "UNKNOWN", cost_class: "free", sovereignty: "external",
      config: { adapter: "ai-lane", baseUrl: "https://api.llm7.io/v1", model: "default", rpmLimit: 2, timeoutMs: 60000, trust: "untrusted", fragmentsOnly: true } },
    { id: "ctcc-heartbeat", provider_id: "cloudflare", name: "ctcc-router", kind: "worker", state: "UNKNOWN", cost_class: "free", sovereignty: "self-hosted",
      config: { adapter: "http-post", url: "https://ctcc-router.cutecity.workers.dev/ingest", method: "POST", timeoutMs: 15000, expect_status: [200, 202], trust: "self" } },
    { id: "cutecity-health", provider_id: "cloudflare", name: "cutecity.cc health", kind: "http-endpoint", state: "UNKNOWN", cost_class: "free", sovereignty: "self-hosted",
      config: { adapter: "http-get", url: "https://cutecity.cc/api/health", method: "GET", timeoutMs: 15000, expect_status: [200], verify_json_path: "database", verify_equals: true, trust: "self" } },
  ],
  capabilities: [
    ["exchange-self", "orchestrate", "{}"], ["exchange-self", "govern", "{}"], ["exchange-self", "score", "{}"],
    ["lane-pollinations", "inference", "{\"context\":\"32k\",\"stream\":true}"],
    ["lane-kilo", "inference", "{\"context\":\"128k\",\"stream\":true}"],
    ["lane-ovh", "inference", "{\"context\":\"32k\",\"stream\":true}"],
    ["lane-llm7", "inference", "{\"context\":\"64k\",\"stream\":true}"],
    ["ctcc-heartbeat", "ingest", "{\"format\":\"heartbeat\"}"],
    ["cutecity-health", "http-check", "{\"verifies\":\"database\"}"],
  ],
  quotas: [
    ["lane-pollinations", 30], ["lane-kilo", 10], ["lane-ovh", 2], ["lane-llm7", 2],
  ],
  policies: [
    { id: "no-secrets-to-keyless", name: "No secrets to keyless lanes",
      rule: JSON.stringify({ deny_when: { resource_trust: "untrusted", data_class_in: ["sensitive", "secret"] } }), effect: "deny" },
    { id: "zero-spend", name: "Default authorized spend $0",
      rule: JSON.stringify({ deny_when: { cost_gt: "job.max_cost" } }), effect: "deny" },
  ],
};

/* ------------------------------------------------------------------ */
/* Helpers                                                             */
/* ------------------------------------------------------------------ */
const now = () => new Date().toISOString();
const uid = (p) => p + "_" + Date.now().toString(36) + Math.random().toString(36).slice(2, 8);

async function sha256hex(s) {
  const d = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(s));
  return [...new Uint8Array(d)].map((b) => b.toString(16).padStart(2, "0")).join("");
}

function json(data, status = 200) {
  return new Response(JSON.stringify(data), { status, headers: { "Content-Type": "application/json" } });
}

function authed(request, env) {
  if (!env.EXCHANGE_KEY) return { ok: false, status: 503, error: "exchange key not configured — fail-closed" };
  const h = request.headers.get("Authorization") || "";
  if (h !== `Bearer ${env.EXCHANGE_KEY}`) return { ok: false, status: 401, error: "unauthorized" };
  return { ok: true };
}

/* ------------------------------------------------------------------ */
/* Worker Protocol v1 — the worker is untrusted execution capacity.    */
/* The governor owns authority (gates, leases, accounting, receipts);  */
/* the worker owns execution (pickup, validate, execute, capture,      */
/* report). Neither impersonates the other. Workers authenticate with  */
/* their own scoped credential — never the Exchange master key.        */
/*                                                                     */
/* A worker reports exactly one terminal outcome:                      */
/*   SUCCEEDED | FAILED_RETRYABLE | FAILED_PERMANENT | QUARANTINED |    */
/*   EXPIRED                                                           */
/* Worker output is evidence. Receipt verification decides what the     */
/* Exchange is willing to believe.                                     */
/* ------------------------------------------------------------------ */
const WORKER_OUTCOMES = ["SUCCEEDED", "FAILED_RETRYABLE", "FAILED_PERMANENT", "QUARANTINED", "EXPIRED"];

// Burner #3 fix (wave-29, d1-burn-codefixes): bounded retention for the
// append-only health_observations table. warrantedTier needs 7d of history,
// demoteCheck needs 24h — 30d is comfortably safe and stops H (hence per-run
// scan cost) from growing without bound. Enforced by pruneHealthObservations(),
// called once per scheduled() run.
const HEALTH_RETENTION_DAYS = 30;

// Worker-scoped auth: X-Worker-Id + X-Worker-Secret (or body fields).
// The secret is stored as sha256hex; the Exchange never returns it.
async function authWorker(db, request, body) {
  const wid = request.headers.get("x-worker-id") || (body && body.worker_id) || "";
  const wsec = request.headers.get("x-worker-secret") || (body && body.worker_secret) || "";
  if (!wid || !wsec) return { ok: false, status: 401, error: "worker credentials required (x-worker-id + x-worker-secret)" };
  const w = await db.prepare("SELECT * FROM workers WHERE worker_id=?").bind(wid).first();
  if (!w || w.disabled) return { ok: false, status: 403, error: "unknown or disabled worker" };
  if ((await sha256hex(wsec)) !== w.secret_hash)
    return { ok: false, status: 403, error: "bad worker secret" };
  await db.prepare("UPDATE workers SET last_seen_at=? WHERE worker_id=?").bind(now(), wid).run();
  return { ok: true, worker: w };
}

/* ------------------------------------------------------------------ */
/* Envelope provenance at ingest (SPEC-GAP-PL-007, wave-32)             */
/*                                                                     */
/* Every receipt crossing an ingest boundary MUST carry a valid         */
/* lucidota-receipt/1 envelope sealed by the authority key. Transport   */
/* auth (worker secret / Bearer) is NOT provenance: a compromised       */
/* credential must not be enough to inject forged receipts. Fail        */
/* closed: missing/invalid/unverifiable envelope → reject, record in    */
/* envelope_rejections, never store. The authority key arrives as the   */
/* PL_SIG_KEY Cloudflare secret at runtime (documented, never in code,  */
/* never logged); if it is absent the worker refuses ingest (503)       */
/* rather than accepting unverifiable receipts.                        */
/* ------------------------------------------------------------------ */
async function recordEnvelopeRejection(db, route, b, reason) {
  try {
    const e = (b && b.envelope) || {};
    await db.prepare(
      "INSERT INTO envelope_rejections (at, route, reason, claim_id, actor) VALUES (?,?,?,?,?)"
    ).bind(new Date().toISOString(), route, String(reason).slice(0, 300),
      (typeof e.claim_id === "string" ? e.claim_id : (b && b.claim_id) || null),
      (typeof e.actor === "string" ? e.actor : null)).run();
  } catch { /* the audit write must never break the fail-closed rejection */ }
}

/* ------------------------------------------------------------------ */
/* Replay/freshness binding (SPEC-GAP-PL-007 follow-up, wave-36)      */
/*                                                                     */
/* A valid-but-replayed envelope was accepted indefinitely. Now the    */
/* requireEnvelope path keeps a bounded in-memory registry of seen     */
/* envelope seals (seal = sha256 of the canonical envelope — content   */
/* addressed, so a byte-identical replay hashes to the same key).      */
/*                                                                     */
/* Freshness window: REPLAY_TTL_MS = 1h. Justification: the exchange's */
/* own cadence is tens of minutes (30-min cron since 2026-10-09, worker retries within minutes), */
/* not days — PL's 24h STALE-EXEC window is the wrong analogy here. 1h */
/* covers every legitimate retry horizon and the plausible            */
/* capture→reuse attack cycle while keeping the bound trivial.         */
/*                                                                     */
/* Bounds: REPLAY_MAX_SEEN = 10k entries (~1MB). Fail-closed: if the   */
/* registry is full (flood), NEW ingest is refused (503) rather than   */
/* evicting-and-silently-re-admitting replays. Entries expire by TTL   */
/* with a sweep on each check.                                         */
/*                                                                     */
/* DURABLE REGISTRY (hardening 2026-10-10, external review P1/P2):     */
/* the per-isolate map above is now only the fast path. The arbiter    */
/* is D1 seal_registry: one atomic INSERT..ON CONFLICT DO NOTHING per  */
/* ingest (changes=0 => replay, global across isolates). Retention is  */
/* bounded (pruneSealRegistry in scheduled(), 1 cheap DELETE per cron */
/* run). If D1 is unreachable the ingest is refused 503 — fail-closed, */
/* never fail-open.                                                    */
/*                                                                     */
/* HTTP mapping: missing envelope 400 / invalid 401 / replay 409 /     */
/* unprovisioned key 503 / registry-full 503 / registry-unavailable    */
/* 503.                                                                */
/* ------------------------------------------------------------------ */
const REPLAY_TTL_MS = 60 * 60 * 1000;
const REPLAY_MAX_SEEN = 10000;
const _seenSeals = new Map(); // seal -> first-seen epoch ms (per-isolate fast path)

function _sweepSeenSeals(nowMs) {
  for (const [s, t] of _seenSeals) {
    if (nowMs - t > REPLAY_TTL_MS) _seenSeals.delete(s);
  }
}

/** Durable replay check: per-isolate memory fast path, D1 seal_registry arbiter.
 *  @returns {{ok:true}|{replay:true}|{full:true}|{unavailable:true}} */
async function checkReplayDb(db, seal) {
  const nowMs = Date.now();
  _sweepSeenSeals(nowMs);
  if (_seenSeals.has(seal)) return { replay: true };
  if (_seenSeals.size >= REPLAY_MAX_SEEN) return { full: true };
  try {
    const ins = await db.prepare(
      "INSERT INTO seal_registry (seal, first_seen) VALUES (?, ?) ON CONFLICT(seal) DO NOTHING"
    ).bind(seal, nowMs).run();
    if ((((ins.meta) || {}).changes || 0) === 0) {
      _seenSeals.set(seal, nowMs); // remember the verdict locally too
      return { replay: true };
    }
  } catch (e) {
    return { unavailable: true }; // D1 error => refuse ingest, fail-closed
  }
  _seenSeals.set(seal, nowMs);
  return { ok: true };
}

/** Bounded retention for the durable seal registry: one cheap DELETE per
 *  cron run, capped at 500 rows. Seals older than the replay TTL are dead
 *  weight — a replay older than the TTL is already outside the threat
 *  window the registry exists to close. */
async function pruneSealRegistry(db) {
  const cutoff = Date.now() - REPLAY_TTL_MS;
  const r = await db.prepare(
    `DELETE FROM seal_registry WHERE seal IN (
       SELECT seal FROM seal_registry WHERE first_seen < ? LIMIT 500)`
  ).bind(cutoff).run();
  return (((r.meta) || {}).changes || 0);
}

async function requireEnvelope(db, route, b, env) {
  const keyHex = (env.PL_SIG_KEY || "").trim();
  if (!/^[0-9a-fA-F]{64}$/.test(keyHex)) {
    await recordEnvelopeRejection(db, route, b, "authority key not provisioned");
    return { error: "envelope authority key not provisioned — refusing unverifiable ingest", status: 503 };
  }
  const e = b ? b.envelope : null;
  if (!e || typeof e !== "object" || Array.isArray(e)) {
    await recordEnvelopeRejection(db, route, b, "missing envelope");
    return { error: "missing envelope: receipts must carry a lucidota-receipt/1 envelope", status: 400 };
  }
  try {
    await verifyReceiptEnvelope(e, keyHex);
  } catch (err) {
    const why = String((err && err.message) || err).slice(0, 200);
    await recordEnvelopeRejection(db, route, b, "rejected: " + why);
    return { error: "envelope rejected: " + why, status: 401 };
  }
  // verifyEnvelope passed: e.seal is the verified content hash of the envelope.
  const rp = await checkReplayDb(db, e.seal);
  if (rp.replay) {
    await recordEnvelopeRejection(db, route, b, "replay: envelope seal already seen");
    return { error: "envelope replay rejected: this envelope was already submitted", status: 409 };
  }
  if (rp.full) {
    await recordEnvelopeRejection(db, route, b, "replay registry full — refusing new ingest");
    return { error: "replay registry full — refusing new ingest (fail-closed)", status: 503 };
  }
  if (rp.unavailable) {
    await recordEnvelopeRejection(db, route, b, "replay registry unavailable — refusing new ingest");
    return { error: "replay registry unavailable — refusing new ingest (fail-closed)", status: 503 };
  }
  // Claim binding (wave-36): a well-formed, properly sealed envelope can
  // still be TRANSPLANTED — lifted from claim A's request and submitted on
  // claim B's. The envelope must attest to THIS request's claim context.
  try {
    await checkClaimBinding(db, route, b, e);
  } catch (err) {
    const why = String((err && err.message) || err).slice(0, 200);
    await recordEnvelopeRejection(db, route, b, "binding: " + why);
    const status = (err && err.status) || 401;
    const prefix = status === 400 ? "claim context missing: " : "envelope rejected: ";
    return { error: prefix + why, status };
  }
  return { ok: true };
}

/* ------------------------------------------------------------------ */
/* Envelope<->request claim binding (wave-36, next_gap 2 of the        */
/* wave-32 envelope-ingest receipt).                                   */
/*                                                                     */
/* verifyReceiptEnvelope proves an envelope is well-formed and sealed   */
/* by the authority key. It does NOT prove the envelope belongs to     */
/* THIS request. Without binding, a valid envelope for claim A can be  */
/* transplanted onto a request for claim B. Binding rule per route     */
/* (fail-closed — no grace for missing context):                       */
/*   complete: e.claim_id === request claim_id                         */
/*             (b.claim_id, else the claim resolved from b.lease_id —  */
/*             the same transitional fallback completeClaimed uses)    */
/*   observe:  e.payload.resource_id === b.resource_id                 */
/*   ballots:  e.payload.proposition === b.proposition                 */
/* The envelope fields are sealed by the authority sig, so a match     */
/* proves the authority attested to this request's claim context.      */
/* Missing request context -> 400 (the downstream handlers already    */
/* 400 on the same missing fields, so no honest caller is newly        */
/* broken). Envelope missing the binding field, or mismatch -> 401.    */
/* Minting-side requirement this documents: observe envelopes MUST     */
/* carry payload.resource_id, ballot envelopes MUST carry              */
/* payload.proposition, completion envelopes MUST carry claim_id.      */
/* ------------------------------------------------------------------ */
class ClaimBindingError extends Error {
  constructor(message, status) {
    super(message);
    this.name = "ClaimBindingError";
    this.status = status;
  }
}

async function claimContextFor(db, route, b) {
  if (route === "complete") {
    let cid = (b && typeof b.claim_id === "string" && b.claim_id) || null;
    if (!cid && b && typeof b.lease_id === "string" && b.lease_id) {
      const c = await db.prepare(
        "SELECT claim_id FROM claims WHERE lease_id=? AND status='active'"
      ).bind(b.lease_id).first();
      cid = (c && typeof c.claim_id === "string" && c.claim_id) || null;
    }
    if (!cid) throw new ClaimBindingError(
      "complete requires claim_id (or a resolvable lease_id)", 400);
    return { field: "claim_id", expect: cid };
  }
  if (route === "observe") {
    const rid = (b && typeof b.resource_id === "string" && b.resource_id) || null;
    if (!rid) throw new ClaimBindingError("observe requires resource_id", 400);
    return { field: "payload.resource_id", expect: rid };
  }
  if (route === "ballots") {
    const prop = (b && typeof b.proposition === "string" && b.proposition) || null;
    if (!prop) throw new ClaimBindingError("ballots requires proposition", 400);
    return { field: "payload.proposition", expect: prop };
  }
  throw new ClaimBindingError("unknown route " + JSON.stringify(route), 400);
}

function envelopeBindingValue(e, field) {
  if (field === "claim_id")
    return (typeof e.claim_id === "string" && e.claim_id) || null;
  const p = e.payload;
  if (!p || typeof p !== "object" || Array.isArray(p)) return null;
  if (field === "payload.resource_id")
    return (typeof p.resource_id === "string" && p.resource_id) || null;
  if (field === "payload.proposition")
    return (typeof p.proposition === "string" && p.proposition) || null;
  return null;
}

async function checkClaimBinding(db, route, b, e) {
  const { field, expect } = await claimContextFor(db, route, b);
  const got = envelopeBindingValue(e, field);
  if (got === null) throw new ClaimBindingError(
    `envelope carries no binding for ${route} (missing ${field}) — minting-side requirement`, 401);
  if (got !== expect) throw new ClaimBindingError(
    `claim binding mismatch on ${field}: envelope ${JSON.stringify(got)} != request ${JSON.stringify(expect)}`, 401);
}

/* ------------------------------------------------------------------ */
/* Seed                                                                */
/* ------------------------------------------------------------------ */
async function seed(db) {
  for (const p of SEED.providers)
    await db.prepare("INSERT OR IGNORE INTO providers (id,name,kind,state,notes) VALUES (?,?,?,?,?)")
      .bind(p.id, p.name, p.kind, p.state, p.notes).run();
  for (const r of SEED.resources)
    await db.prepare("INSERT OR IGNORE INTO resources (id,provider_id,name,kind,state,cost_class,sovereignty,config,evidence_state,evidence_source,evidence_at,cost_basis) VALUES (?,?,?,?,?,?,?,?,?,?,?,?)")
      .bind(r.id, r.provider_id, r.name, r.kind, r.state, r.cost_class, r.sovereignty, JSON.stringify(r.config),
        "DISCOVERED", "seed", now(), "unknown").run();
  for (const [rid, cap, detail] of SEED.capabilities)
    await db.prepare("INSERT OR IGNORE INTO resource_capabilities (resource_id,capability,detail) VALUES (?,?,?)")
      .bind(rid, cap, detail).run();
  for (const [rid, rpm] of SEED.quotas)
    await db.prepare("INSERT OR IGNORE INTO quotas (resource_id,limit_per_min) VALUES (?,?)").bind(rid, rpm).run();
  for (const p of SEED.policies)
    await db.prepare("INSERT OR IGNORE INTO policies (id,name,rule,effect) VALUES (?,?,?,?)")
      .bind(p.id, p.name, p.rule, p.effect).run();
  return { seeded: true };
}

/* ------------------------------------------------------------------ */
/* Evidence lifecycle — DISCOVERED → ACCESSIBLE → FREE → QUOTA →       */
/* CURRENTLY AVAILABLE → VERIFIED. UNKNOWN = move on. PARKED = shelved.*/
/* A listing is never usable capacity. Only AVAILABLE/VERIFIED execute.*/
/* ------------------------------------------------------------------ */
const EVIDENCE_LADDER = ["DISCOVERED", "ACCESSIBLE", "FREE", "QUOTA", "AVAILABLE", "VERIFIED"];
const EVIDENCE_USABLE = ["AVAILABLE", "VERIFIED"];

function evRank(s) {
  if (s === "PARKED") return -2;
  if (s === "UNKNOWN") return -1;
  return EVIDENCE_LADDER.indexOf(s);
}

// Parse D1 datetime('now') format ('YYYY-MM-DD HH:MM:SS', UTC) or ISO.
function ts(s) { return Date.parse(String(s || "").replace(" ", "T") + "Z"); }
function minutesAgo(n) { return new Date(Date.now() - n * 60000).toISOString().replace("T", " ").slice(0, 19); }

async function migrate(db) {
  const colsOf = async (t) => new Set(((await db.prepare(`PRAGMA table_info(${t})`).all()).results || []).map((c) => c.name));
  const ensure = async (t, col, ddl) => {
    const names = await colsOf(t);
    if (!names.has(col)) await db.prepare(`ALTER TABLE ${t} ADD COLUMN ${ddl}`).run();
  };
  // v0.3 columns
  await ensure("resources", "evidence_state", "evidence_state TEXT DEFAULT 'UNKNOWN'");
  await ensure("resources", "evidence_source", "evidence_source TEXT");
  await ensure("resources", "evidence_at", "evidence_at TEXT");
  await ensure("resources", "cost_basis", "cost_basis TEXT DEFAULT 'unknown'");
  await ensure("quotas", "measured", "measured INTEGER DEFAULT 0");
  await ensure("quotas", "measured_at", "measured_at TEXT");
  // v0.4 columns — reaction loop + leases + attempts + receipt accounting
  await ensure("resources", "throttled_until", "throttled_until TEXT");
  await ensure("resources", "suspended_until", "suspended_until TEXT");
  await ensure("resources", "credential_state", "credential_state TEXT DEFAULT 'unknown'");
  await ensure("leases", "authority", "authority TEXT");
  await ensure("leases", "expected_cost", "expected_cost REAL DEFAULT 0");
  await ensure("leases", "quota_snapshot", "quota_snapshot TEXT");
  await ensure("leases", "policy_version", "policy_version TEXT");
  await ensure("leases", "expires_at", "expires_at TEXT");
  await ensure("executions", "attempt_no", "attempt_no INTEGER");
  await ensure("executions", "lease_id", "lease_id TEXT");
  await ensure("executions", "failure_taxonomy", "failure_taxonomy TEXT");
  await ensure("executions", "request_hash", "request_hash TEXT");
  await ensure("receipts", "attempt_id", "attempt_id TEXT");
  await ensure("receipts", "policy_version", "policy_version TEXT");
  await ensure("receipts", "lease", "lease TEXT");
  await ensure("receipts", "cost_before", "cost_before REAL");
  await ensure("receipts", "cost_after", "cost_after REAL");
  await ensure("receipts", "health_before", "health_before TEXT");
  await ensure("receipts", "health_after", "health_after TEXT");
  await ensure("receipts", "fallback_chain", "fallback_chain TEXT");
  await ensure("jobs", "defer_count", "defer_count INTEGER DEFAULT 0");
  await db.prepare(`CREATE TABLE IF NOT EXISTS evidence_log (
    id INTEGER PRIMARY KEY AUTOINCREMENT, at TEXT DEFAULT (datetime('now')),
    resource_id TEXT, from_state TEXT, to_state TEXT, trigger TEXT, detail TEXT)`).run();
  await db.prepare(`CREATE TABLE IF NOT EXISTS discovery_sources (
    id TEXT PRIMARY KEY, name TEXT, url TEXT, kind TEXT, enabled INTEGER DEFAULT 1,
    last_fetched_at TEXT, last_result TEXT)`).run();
  // v0.6 — Worker Protocol v1: sovereign governor, untrusted workers.
  // JOB → CLAIM → EXECUTION → RECEIPT → ACCOUNTING are distinct objects.
  await db.prepare(`CREATE TABLE IF NOT EXISTS workers (
    worker_id TEXT PRIMARY KEY, secret_hash TEXT NOT NULL,
    trust_class TEXT DEFAULT 'remote', name TEXT,
    created_at TEXT DEFAULT (datetime('now')), last_seen_at TEXT,
    disabled INTEGER DEFAULT 0)`).run();
  await db.prepare(`CREATE TABLE IF NOT EXISTS claims (
    claim_id TEXT PRIMARY KEY, job_id TEXT NOT NULL, worker_id TEXT NOT NULL,
    lease_id TEXT NOT NULL UNIQUE, resource_id TEXT NOT NULL,
    attempt_no INTEGER DEFAULT 1, status TEXT DEFAULT 'active',
    claimed_at TEXT DEFAULT (datetime('now')), lease_expiry TEXT NOT NULL,
    policy_snapshot TEXT, receipt_requirements TEXT,
    idempotency_key TEXT, input_hash TEXT,
    worker_outcome TEXT, completed_at TEXT, completion_summary TEXT)`).run();
  await db.prepare(`CREATE INDEX IF NOT EXISTS idx_claims_lease ON claims(lease_id)`).run();
  await db.prepare(`CREATE INDEX IF NOT EXISTS idx_claims_job ON claims(job_id)`).run();
  await db.prepare(`CREATE TABLE IF NOT EXISTS accounting (
    id TEXT PRIMARY KEY, execution_id TEXT NOT NULL, claim_id TEXT,
    job_id TEXT, resource_id TEXT, measured_cost REAL DEFAULT 0,
    cost_basis TEXT DEFAULT 'unknown', quota_effect TEXT,
    recorded_at TEXT DEFAULT (datetime('now')))`).run();
  await ensure("executions", "claim_id", "claim_id TEXT");
  await ensure("executions", "requested_model", "requested_model TEXT");
  await ensure("executions", "actual_model", "actual_model TEXT");
  await ensure("executions", "model_match", "model_match INTEGER");
  await ensure("executions", "usage_json", "usage_json TEXT");
  await ensure("resources", "claimant_trust_required", "claimant_trust_required TEXT DEFAULT 'any'");
  // FIX C3 (2026-10-10, torture campaign): the stuck-completion sweep keys on
  // the completing-flip time, not claim age. completing_at is stamped (SQLite
  // datetime format, via minutesAgo) atomically with the active->completing
  // flip; the sweep can never reset an in-flight completion mid-flight.
  await ensure("claims", "completing_at", "completing_at TEXT");
  await ensure("receipts", "claim_id", "claim_id TEXT");
  await ensure("receipts", "worker_outcome", "worker_outcome TEXT");
  await ensure("receipts", "ternary_verdict", "ternary_verdict INTEGER"); // +1 | 0 | -1 | NULL (amendment A1, v0.6.3)
  // v0.6.4 — voting amendments (Zackary approved 2026-10-07).
  await db.prepare(`CREATE TABLE IF NOT EXISTS ballots (
    id TEXT PRIMARY KEY, ballot_type TEXT NOT NULL, proposition TEXT NOT NULL,
    voters TEXT NOT NULL, quorum INTEGER NOT NULL, deadline_at TEXT NOT NULL,
    state TEXT DEFAULT 'open', verdict TEXT, tally TEXT,
    created_at TEXT DEFAULT (datetime('now')))`).run();
  await ensure("receipts", "ballot_id", "ballot_id TEXT"); // A2: additive, optional
  await db.prepare(`CREATE TABLE IF NOT EXISTS ballot_policy (
    id TEXT PRIMARY KEY, voters TEXT NOT NULL, quorum_rule TEXT DEFAULT 'majority',
    deadline_s INTEGER DEFAULT 600, tie_policy TEXT DEFAULT 'unknown')`).run();
  // SPEC-GAP-PL-007 (wave-32): durable audit of fail-closed envelope rejections
  // at the ingest boundary. Additive, auto-applies on next boot like the
  // other migrate() DDL — no separate Cloudflare step.
  await db.prepare(`CREATE TABLE IF NOT EXISTS envelope_rejections (
    id INTEGER PRIMARY KEY AUTOINCREMENT, at TEXT, route TEXT, reason TEXT,
    claim_id TEXT, actor TEXT)`).run();
  // HARDENING (2026-10-10, external review P1/P2): durable replay registry.
  // The per-isolate _seenSeals map cannot stop a replay that lands on a
  // different isolate. seal_registry makes the seal-uniqueness check atomic
  // and global: INSERT..ON CONFLICT DO NOTHING is the arbiter (changes=0 =>
  // replay). D1-budget note: one PK INSERT per envelope ingest (a write,
  // not a read — writes are 100K/day free); the hot path checks isolate
  // memory first and never reads D1. Bounded retention via
  // pruneSealRegistry() in scheduled().
  await db.prepare(`CREATE TABLE IF NOT EXISTS seal_registry (
    seal TEXT PRIMARY KEY, first_seen INTEGER NOT NULL)`).run();
  const pol = await db.prepare("SELECT id FROM ballot_policy WHERE id='default'").first();
  if (!pol) await db.prepare(
    "INSERT INTO ballot_policy (id,voters,quorum_rule,deadline_s,tie_policy) VALUES (?,?,?,?,?)"
  ).bind("default",
    JSON.stringify(["openai/gpt-oss-20b","z-ai/glm-5.3-flash","nvidia/nemotron-3.5-lightning-30b-a3b"]),
    "majority", 600, "unknown").run();
  // v0.6.14 (wave-29, d1-burn-codefixes): health_observations was created
  // out-of-band in D1 — the schema lived nowhere in the repo. Version-control
  // it here. Column list reconstructed from the INSERT in observe()
  // (resource_id, ok, latency_ms, detail) + observed_at default. The ensure()
  // calls cover a pre-existing out-of-band table missing any column.
  await db.prepare(`CREATE TABLE IF NOT EXISTS health_observations (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    resource_id TEXT NOT NULL, ok INTEGER NOT NULL,
    latency_ms REAL, detail TEXT,
    observed_at TEXT DEFAULT (datetime('now')))`).run();
  await ensure("health_observations", "resource_id", "resource_id TEXT");
  await ensure("health_observations", "ok", "ok INTEGER");
  await ensure("health_observations", "latency_ms", "latency_ms REAL");
  await ensure("health_observations", "detail", "detail TEXT");
  await ensure("health_observations", "observed_at", "observed_at TEXT DEFAULT (datetime('now'))");
  // v0.6.14 (wave-29): hot-path indexes, boot-time application path.
  // migrations/0001 (wave-15) was committed but never applied to live D1;
  // migrations/0002 (wave-29) adds the receipts index. Both files remain the
  // out-of-band (wrangler) application path; IF NOT EXISTS keeps the two
  // paths from conflicting. Landing them here applies them automatically on
  // the next boot — no separate Cloudflare-gated step required.
  await db.prepare(`CREATE INDEX IF NOT EXISTS idx_health_obs_resource_observed ON health_observations (resource_id, observed_at DESC)`).run();
  await db.prepare(`CREATE INDEX IF NOT EXISTS idx_health_obs_resource_ok_observed ON health_observations (resource_id, ok, observed_at DESC)`).run();
  await db.prepare(`CREATE INDEX IF NOT EXISTS idx_receipts_created_at ON receipts (created_at DESC)`).run();
  // v0.6.15 (d1-burn-codefixes): read-audit index bundle. Verified offenders:
  // executions(resource_id) and receipts(resource) full-scan per resource per
  // cron in warrantedTierInfo; claims(status,lease_expiry) full-scan in
  // sweepExpiredClaims; jobs(created_at) full-scan+sort on GET /v1/jobs;
  // executions(job_id) full-scan on /v1/job/:id and receipt verify;
  // leases(job_id) on job-scoped lease reads; leases(resource_id) on
  // unclaimedQueueDepth; resources(provider_id) on providerDownCheck's
  // triple-nested providers x resources; leases(state,created_at) on the
  // orphan-lease sweep (claims side already has idx_claims_lease).
  // IF NOT EXISTS: safe with the migrations/ out-of-band path. Read-only
  // effect — no behavior change, no data touched.
  await db.prepare(`CREATE INDEX IF NOT EXISTS idx_executions_resource ON executions (resource_id)`).run();
  await db.prepare(`CREATE INDEX IF NOT EXISTS idx_receipts_resource ON receipts (resource)`).run();
  await db.prepare(`CREATE INDEX IF NOT EXISTS idx_claims_status_expiry ON claims (status, lease_expiry)`).run();
  await db.prepare(`CREATE INDEX IF NOT EXISTS idx_jobs_created ON jobs (created_at DESC)`).run();
  await db.prepare(`CREATE INDEX IF NOT EXISTS idx_executions_job ON executions (job_id)`).run();
  await db.prepare(`CREATE INDEX IF NOT EXISTS idx_leases_job ON leases (job_id)`).run();
  await db.prepare(`CREATE INDEX IF NOT EXISTS idx_leases_resource ON leases (resource_id)`).run();
  await db.prepare(`CREATE INDEX IF NOT EXISTS idx_resources_provider ON resources (provider_id)`).run();
  await db.prepare(`CREATE INDEX IF NOT EXISTS idx_leases_state_created ON leases (state, created_at)`).run();
  const done = await db.prepare("SELECT COUNT(*) c FROM evidence_log WHERE trigger='backfill-v0.3'").first();
  if (!done || done.c === 0) await backfillEvidence(db);
}

// One-time honest backfill: derive evidence tiers from actual observations,
// never from config claims. cost_class='free' in seed config is asserted,
// not measured — cost_basis stays 'unknown' until a $0 execution is recorded.
async function backfillEvidence(db) {
  const rs = await db.prepare("SELECT id,state FROM resources").all();
  for (const r of (rs.results || [])) {
    const ok7 = (await db.prepare("SELECT COUNT(*) c FROM health_observations WHERE resource_id=? AND ok=1 AND observed_at>?").bind(r.id, minutesAgo(7 * 24 * 60)).first() || {}).c || 0;
    const ok24 = (await db.prepare("SELECT COUNT(*) c FROM health_observations WHERE resource_id=? AND ok=1 AND observed_at>?").bind(r.id, minutesAgo(24 * 60)).first() || {}).c || 0;
    const execOk = (await db.prepare("SELECT COUNT(*) c FROM executions WHERE resource_id=? AND result='ok'").bind(r.id).first() || {}).c || 0;
    let ev = "UNKNOWN", basis = "unknown";
    if (r.id === "exchange-self") { ev = "VERIFIED"; basis = "measured"; }
    else if (ok7 >= 3 && ok24 >= 1 && r.state === "AVAILABLE" && execOk >= 1) { ev = "VERIFIED"; basis = "measured"; }
    else if (ok24 >= 1 && r.state === "AVAILABLE" && execOk >= 1) { ev = "AVAILABLE"; basis = "measured"; }
    else if (execOk >= 1) { ev = "FREE"; basis = "measured"; }
    else if (ok7 >= 1) { ev = "ACCESSIBLE"; }
    await db.prepare("UPDATE resources SET evidence_state=?, evidence_source=?, evidence_at=?, cost_basis=? WHERE id=?")
      .bind(ev, "backfill-v0.3", now(), basis, r.id).run();
    await db.prepare("INSERT INTO evidence_log (resource_id,from_state,to_state,trigger,detail) VALUES (?,?,?,?,?)")
      .bind(r.id, "UNKNOWN", ev, "backfill-v0.3", `ok7d=${ok7} ok24h=${ok24} exec_ok=${execOk} health=${r.state}`).run();
  }
}

async function setEvidence(db, rid, to, trigger, detail, force = false) {
  const cur = await db.prepare("SELECT evidence_state FROM resources WHERE id=?").bind(rid).first();
  const from = (cur && cur.evidence_state) || "UNKNOWN";
  if (from === to) return { from, to, changed: false };
  if (!force && evRank(to) < evRank(from)) return { from, to, changed: false, skipped: "demotion requires force" };
  await db.prepare("UPDATE resources SET evidence_state=?, evidence_at=? WHERE id=?").bind(to, now(), rid).run();
  await db.prepare("INSERT INTO evidence_log (resource_id,from_state,to_state,trigger,detail) VALUES (?,?,?,?,?)")
    .bind(rid, from, to, trigger, detail || null).run();
  return { from, to, changed: true };
}

// Highest tier warranted by recorded evidence alone. DISCOVERED never comes
// from observations — only from explicit registration (a lead, not proof).
//
// Burner #2 fix (wave-29, d1-burn-codefixes): warrantedTier recomputed the
// ok7/ok24 health windows on every call, and autoPromote + demoteCheck each
// triggered a computation per resource per cron run (3 window scans per
// resource per run). warrantedTierInfo computes the tier AND the underlying
// counts once; autoPromote/demoteCheck accept the info object via an
// optional parameter and share it. Callers that need only one of the two
// pass nothing and behave exactly as before — no query-count regression on
// the fresh path. warrantedTier keeps its original signature/semantics.
async function warrantedTierInfo(db, rid) {
  const r = await db.prepare("SELECT state FROM resources WHERE id=?").bind(rid).first();
  if (!r) return { tier: "UNKNOWN", ok7: 0, ok24: 0, execOk: 0, freeExec: 0, quotaMeasured: false };
  const ok7 = (await db.prepare("SELECT COUNT(*) c FROM health_observations WHERE resource_id=? AND ok=1 AND observed_at>?").bind(rid, minutesAgo(7 * 24 * 60)).first() || {}).c || 0;
  const ok24 = (await db.prepare("SELECT COUNT(*) c FROM health_observations WHERE resource_id=? AND ok=1 AND observed_at>?").bind(rid, minutesAgo(24 * 60)).first() || {}).c || 0;
  const execOk = (await db.prepare("SELECT COUNT(*) c FROM executions WHERE resource_id=? AND result='ok'").bind(rid).first() || {}).c || 0;
  const freeExec = (await db.prepare("SELECT COUNT(*) c FROM receipts WHERE resource=? AND result='ok' AND (cost IS NULL OR cost=0)").bind(rid).first() || {}).c || 0;
  const q = await db.prepare("SELECT * FROM quotas WHERE resource_id=?").bind(rid).first();
  const quotaMeasured = !!(q && q.measured === 1 && (q.used_window || 0) < (q.limit_per_min || 0));
  let tier = "UNKNOWN";
  if (ok7 >= 3 && ok24 >= 1 && r.state === "AVAILABLE" && execOk >= 1) tier = "VERIFIED";
  else if (ok24 >= 1 && (r.state === "AVAILABLE" || r.state === "DEGRADED") && execOk >= 1) tier = "AVAILABLE";
  else if (quotaMeasured) tier = "QUOTA";
  else if (freeExec >= 1) tier = "FREE";
  else if (ok7 >= 1) tier = "ACCESSIBLE";
  return { tier, ok7, ok24, execOk, freeExec, quotaMeasured };
}

async function warrantedTier(db, rid) {
  return (await warrantedTierInfo(db, rid)).tier;
}

async function autoPromote(db, rid, tierInfo) {
  const cur = (await db.prepare("SELECT evidence_state FROM resources WHERE id=?").bind(rid).first() || {}).evidence_state || "UNKNOWN";
  if (cur === "PARKED") return cur;
  const w = tierInfo ? tierInfo.tier : await warrantedTier(db, rid);
  if (evRank(w) > evRank(cur)) {
    await setEvidence(db, rid, w, "auto-promote", "warranted by recorded observations");
    return w;
  }
  return cur;
}

// Notice-and-adapt: usable tiers go stale without fresh success.
async function demoteCheck(db, rid, tierInfo) {
  const r = await db.prepare("SELECT evidence_state FROM resources WHERE id=?").bind(rid).first();
  if (!r) return null;
  const ev = r.evidence_state;
  if (ev !== "AVAILABLE" && ev !== "VERIFIED") return ev;
  const ok24 = tierInfo ? tierInfo.ok24 : ((await db.prepare("SELECT COUNT(*) c FROM health_observations WHERE resource_id=? AND ok=1 AND observed_at>?").bind(rid, minutesAgo(24 * 60)).first() || {}).c || 0);
  if (ok24 === 0) {
    const down = ev === "VERIFIED" ? "AVAILABLE" : "FREE";
    await setEvidence(db, rid, down, "stale-demote", "no successful observation in 24h", true);
    return down;
  }
  return ev;
}

// Burner #3 fix (wave-29): enforce HEALTH_RETENTION_DAYS on the append-only
// health_observations table, once per cron run. Per-resource indexed deletes
// via idx_health_obs_resource_observed (resource_id, observed_at): seek on
// resource_id, range on observed_at — no table scan, no new index needed.
async function pruneHealthObservations(db) {
  const cutoff = minutesAgo(HEALTH_RETENTION_DAYS * 24 * 60);
  const rs = await db.prepare("SELECT id FROM resources").all();
  let pruned = 0;
  for (const r of (rs.results || [])) {
    const res = await db.prepare(
      "DELETE FROM health_observations WHERE resource_id=? AND observed_at<?"
    ).bind(r.id, cutoff).run();
    pruned += Number((res && res.meta && res.meta.changes) || 0);
  }
  return pruned;
}

// Notice-and-adapt: billing appeared on a resource we believed was free.
async function billingCheck(db) {
  const rows = await db.prepare("SELECT id,resource,cost,created_at FROM receipts ORDER BY created_at DESC LIMIT 40").all();
  const out = [];
  for (const rc of (rows.results || [])) {
    if (!rc.resource || !(rc.cost > 0)) continue;
    if (ts(rc.created_at) < Date.now() - 3600000) continue; // only fresh charges
    const r = await db.prepare("SELECT evidence_state,cost_class FROM resources WHERE id=?").bind(rc.resource).first();
    if (!r) continue;
    // Only demote resources we believed were free. A paid resource executing
    // with explicit spend_authority must not become single-use.
    if (r.cost_class === "free" && ["FREE", "QUOTA", "AVAILABLE", "VERIFIED"].includes(r.evidence_state)) {
      await db.prepare("UPDATE resources SET cost_class='paid', cost_basis='measured' WHERE id=?").bind(rc.resource).run();
      const ch = await setEvidence(db, rc.resource, "ACCESSIBLE", "billing-appeared",
        `receipt ${rc.id} charged $${rc.cost} — reachable but not free`, true);
      out.push({ resource: rc.resource, charged: rc.cost, ...ch });
    }
  }
  return out;
}

// Register a candidate lead. Never usable on registration — verification only.
async function registerCandidate(db, b, source) {
  const nm = b.name || b.provider_name;
  if (!b.provider_id || !nm) return { error: "provider_id and name (or provider_name) required" };
  const pid = b.provider_id;
  const rid = b.id || ("res_" + pid.replace(/[^a-z0-9]/gi, "").toLowerCase());
  await db.prepare("INSERT OR IGNORE INTO providers (id,name,kind,state,notes) VALUES (?,?,?,?,?)")
    .bind(pid, nm, b.provider_kind || "candidate", "UNKNOWN",
      (b.notes || "") + " [DISCOVERED lead — not a resource. Do not route.]").run();
  await db.prepare("INSERT INTO evidence_log (resource_id,from_state,to_state,trigger,detail) VALUES (?,?,?,?,?)")
    .bind("provider:" + pid, "UNKNOWN", "DISCOVERED", source || "manual", b.notes || null).run();
  if (b.resource === false) return { discovered: "provider:" + pid, evidence_state: "DISCOVERED", usable: false };
  const cfg = b.config || {};
  if (cfg.adapter && !adapters[cfg.adapter]) return { error: `unknown adapter ${cfg.adapter}` };
  await db.prepare("INSERT OR REPLACE INTO resources (id,provider_id,name,kind,state,cost_class,sovereignty,config,evidence_state,evidence_source,evidence_at,cost_basis) VALUES (?,?,?,?,?,?,?,?,?,?,?,?)")
    .bind(rid, pid, b.name, b.kind || "candidate", "UNKNOWN", b.cost_class || "unknown",
      b.sovereignty || "external", JSON.stringify(cfg), "DISCOVERED", source || "manual", now(), "unknown").run();
  for (const cap of b.capabilities || [])
    await db.prepare("INSERT OR IGNORE INTO resource_capabilities (resource_id,capability,detail) VALUES (?,?,?)")
      .bind(rid, cap.name || cap, JSON.stringify((cap && cap.detail) || {})).run();
  if (b.rpm_limit)
    await db.prepare("INSERT OR REPLACE INTO quotas (resource_id,limit_per_min) VALUES (?,?)").bind(rid, b.rpm_limit).run();
  await db.prepare("INSERT INTO evidence_log (resource_id,from_state,to_state,trigger,detail) VALUES (?,?,?,?,?)")
    .bind(rid, "UNKNOWN", "DISCOVERED", source || "manual", b.notes || null).run();
  return { discovered: rid, evidence_state: "DISCOVERED", usable: false };
}

// Pull candidate manifests from configured discovery sources (bounded).
async function runDiscovery(db) {
  const srcs = await db.prepare("SELECT * FROM discovery_sources WHERE enabled=1").all();
  const out = [];
  for (const s of (srcs.results || [])) {
    if (s.last_fetched_at && ts(s.last_fetched_at) > Date.now() - 24 * 3600 * 1000) { out.push({ source: s.id, skipped: "fresh" }); continue; }
    try {
      const resp = await fetch(s.url, { signal: AbortSignal.timeout(15000) });
      if (!resp.ok) throw new Error("HTTP " + resp.status);
      const data = await resp.json();
      const cands = data.resources || data.candidates || [];
      let added = 0;
      for (const c of cands.slice(0, 50)) {
        if (!c.provider_id || !c.name) continue;
        const id = c.id || ("res_" + String(c.provider_id).replace(/[^a-z0-9]/gi, "").toLowerCase());
        const exists = await db.prepare("SELECT id FROM resources WHERE id=?").bind(id).first();
        if (exists) continue;
        const r = await registerCandidate(db, { ...c, id }, "discovery:" + s.id);
        if (!r.error) added++;
      }
      await db.prepare("UPDATE discovery_sources SET last_fetched_at=?, last_result=? WHERE id=?")
        .bind(now(), `added ${added} candidate(s)`, s.id).run();
      out.push({ source: s.id, added });
    } catch (e) {
      await db.prepare("UPDATE discovery_sources SET last_result=? WHERE id=?")
        .bind("fetch failed: " + String(e).slice(0, 120), s.id).run();
      out.push({ source: s.id, error: String(e).slice(0, 120) });
    }
  }
  return out;
}

/* ------------------------------------------------------------------ */
/* Control plane primitives — leases, taxonomy, policy versioning      */
/* ------------------------------------------------------------------ */

// Probe staleness budget by evidence tier (minutes). Shared by the cron and
// the INTEGRITY gate so both judge freshness by the same rule.
function staleAfterEv(ev) {
  return ev === "VERIFIED" || ev === "AVAILABLE" ? 15
    : ev === "QUOTA" || ev === "FREE" || ev === "ACCESSIBLE" ? 60
    : ev === "DISCOVERED" ? 180 : ev === "PARKED" ? 1440 : 360;
}

// Policy version: hash of the canonical policy set. Recorded on every lease
// and receipt so a later process can tell exactly which rules authorized what.
// Policies are versioned, never silently mutated (see invariant below).
async function policyVersion(db) {
  const rows = await db.prepare("SELECT id,rule,effect FROM policies ORDER BY id").all();
  const canon = JSON.stringify((rows.results || []).map((p) => [p.id, p.rule, p.effect]));
  return (await sha256hex(canon)).slice(0, 16);
}

// Failure taxonomy — distinct failures are never collapsed into "failed".
// A provider returning an error is evidence about the PROVIDER, not evidence
// the workload succeeded elsewhere.
function taxonomyOf(error_class) {
  switch (error_class) {
    case "RATE_LIMITED": return "QUOTA_FAILURE";
    case "TIMEOUT":
    case "NETWORK":
    case "SERVER_ERROR":
    case "UNKNOWN": return "RETRYABLE_FAILURE";
    case "BAD_RESPONSE": return "PERMANENT_FAILURE";
    case "POLICY_BLOCKED": return "POLICY_FAILURE";
    case "AUTH_ERROR": return "AUTH_FAILURE";
    case "NO_ADAPTER": return "CAPABILITY_FAILURE";
    case "VERIFICATION_FAILED": return "VERIFICATION_FAILURE";
    default: return "RETRYABLE_FAILURE";
  }
}

// LEASE BEFORE SIDE EFFECT. A lease reserves a resource for one job attempt.
// No answer from this check = no authorization: expired, lost, or materially
// changed → do not execute, discard, replan.
async function createLease(db, job_id, resource_id, expected_cost, quota_snapshot, policy_version) {
  const lease_id = uid("lease");
  const expires_at = new Date(Date.now() + 60000).toISOString(); // ~60s
  await db.prepare(
    "INSERT INTO leases (id,job_id,resource_id,state,authority,expected_cost,quota_snapshot,policy_version,expires_at) VALUES (?,?,?,?,?,?,?,?,?)"
  ).bind(lease_id, job_id, resource_id, "active", "bearer:exchange-key", expected_cost,
    JSON.stringify(quota_snapshot), policy_version, expires_at).run();
  return { lease_id, expires_at };
}

async function validateLease(db, lease_id) {
  const l = await db.prepare("SELECT * FROM leases WHERE id=?").bind(lease_id).first();
  if (!l) return { ok: false, reason: "lost (no such lease)" };
  if (l.state !== "active") return { ok: false, reason: `not active (state=${l.state})` };
  if (Date.parse(l.expires_at) < Date.now()) {
    await db.prepare("UPDATE leases SET state='expired' WHERE id=?").bind(lease_id).run();
    return { ok: false, reason: "expired" };
  }
  return { ok: true, lease: l };
}

// DELEGATED EXECUTION. The Worker reserves the resource with a lease and
// parks the job as deferred; a trusted claimant holding the credential
// DELEGATE — the governor reserves capacity and parks the job as deferred.
// A worker picks the delegation up via /v1/claim and reports via
// /v1/complete. The delegation itself is a material decision → durable
// receipt. Pickup window is 10 minutes; the cron replans a deferred job
// after its lease expires unclaimed (never double-runs).
//
// Temporal chain (Worker Protocol v1): JOB → CLAIM → EXECUTION → RECEIPT
// → ACCOUNTING. The execution record is created at CLAIM time, not here:
// no worker has touched this work yet, so there is no execution to record.
async function delegateExecution(db, job_id, spec, resource, provider, trace) {
  const policy_version = await policyVersion(db);
  const input_hash = await sha256hex(JSON.stringify(spec.input));
  const lease = await createLease(db, job_id, resource.id, costOf(resource), { delegated: true }, policy_version);
  const pickup = new Date(Date.now() + 10 * 60000).toISOString();
  await db.prepare("UPDATE leases SET expires_at=? WHERE id=?").bind(pickup, lease.lease_id).run();
  await db.prepare("UPDATE jobs SET state='deferred' WHERE id=?").bind(job_id).run();
  trace.push({ gate: "LEASE", pass: true, reason: `lease ${lease.lease_id} delegated (pickup window 10m)`,
    detail: { resource: resource.id, expected_cost: costOf(resource), policy_version }, at: now() });
  trace.push({ gate: "EXECUTE", pass: true, reason: "delegated to claimant — awaiting pickup",
    detail: { resource: resource.id, lease: lease.lease_id }, at: now() });
  const receipt_id = uid("rcpt");
  const leaseSnap = await db.prepare("SELECT * FROM leases WHERE id=?").bind(lease.lease_id).first();
  await db.prepare(
    `INSERT INTO receipts (id,job_id,execution_id,attempt_id,provider,resource,started_at,finished_at,
      input_hash,output_hash,result,cost,cost_before,cost_after,quota_before,quota_after,
      health_before,health_after,policy_version,lease,decision,fallback_chain,errors,worker_outcome)
     VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)`
  ).bind(receipt_id, job_id, null, null, provider.id, resource.id, now(), null,
    input_hash, null, "deferred", 0, costOf(resource), 0, null, null,
    resource.state, null, policy_version, JSON.stringify(leaseSnap),
    JSON.stringify({ trace, delegated: true, trust_required: resource.claimant_trust_required || "any" }),
    JSON.stringify([]),
    "delegated to claimant — result pending", null).run();
  await db.prepare("INSERT INTO evidence_log (resource_id,from_state,to_state,trigger,detail) VALUES (?,?,?,?,?)")
    .bind(resource.id, resource.evidence_state, resource.evidence_state, "delegated", `job ${job_id} awaiting claimant`).run();
  return { job_id, state: "deferred", lease_id: lease.lease_id, receipt_id, trace };
}

// COMPLETE a delegated execution — Worker Protocol v1.
//
// The worker is untrusted execution capacity. It reports exactly one
// terminal outcome (SUCCEEDED|FAILED_RETRYABLE|FAILED_PERMANENT|
// QUARANTINED|EXPIRED). Everything else is the governor's decision:
//
// - Worker identity is verified: a claim belongs to exactly one worker.
//   Forged worker/resource/job bindings are rejected, never executed.
// - Completion is idempotent on claim_id: replays return the original
//   result with no duplicate accounting.
// - Cost is derived from the resource's measured/declared economics —
//   never from the worker's assertion. A malicious cost=0 on a paid
//   execution is ignored; the accounting row records what the Exchange
//   measured, not what the worker said.
// - SUCCEEDED output is independently verified: empty content becomes
//   VERIFICATION_FAILURE. The worker cannot manufacture success.
// - Requested vs actually-executed capacity (model identity) is bound
//   into the execution record and the receipt.
async function completeClaimed(db, b, worker) {
  let claim_id = b.claim_id;
  // Transitional fallback: resolve the active claim from a lease id.
  if (!claim_id && b.lease_id) {
    const c = await db.prepare("SELECT claim_id FROM claims WHERE lease_id=? AND status='active'").bind(b.lease_id).first();
    if (c) claim_id = c.claim_id;
  }
  if (!claim_id) return { error: "claim_id required", status: 400 };
  const claim = await db.prepare("SELECT * FROM claims WHERE claim_id=?").bind(claim_id).first();
  if (!claim) return { error: "unknown claim", status: 404 };
  // Invariant: a claim belongs to exactly one worker. Cross-worker
  // completion is forgery, rejected outright.
  if (claim.worker_id !== worker.worker_id)
    return { error: "claim belongs to another worker — forgery rejected", status: 403 };
  // Invariant: the worker cannot rebind the claim to a different
  // resource or job. Assertions that disagree with the claim are forgery.
  if (b.resource_id && b.resource_id !== claim.resource_id)
    return { error: "resource_id does not match claim — forgery rejected", status: 400 };
  if (b.job_id && b.job_id !== claim.job_id)
    return { error: "job_id does not match claim — forgery rejected", status: 400 };
  // Idempotency (W2, battery v4 R1 HIGH): an already-completed claim
  // replays the original result. No new receipt, no new execution, no
  // double accounting.
  if (claim.status === "completed") {
    const s = JSON.parse(claim.completion_summary || "{}");
    return { ...s, idempotent: true, note: "already completed — no duplicate accounting", status: 200 };
  }
  if (claim.status === "expired")
    return { error: "claim expired — completion rejected", status: 409 };
  // HARDENING (2026-10-10, external review P1): the request is validated
  // FULLY before the active->completing flip. The old code flipped first
  // and 400'd on a bad worker_outcome afterwards, stranding the claim in
  // 'completing' (invisible to the active sweep until the 15min stuck-
  // completion recovery). No state transition precedes validation.
  const outcome = b.worker_outcome;
  if (!WORKER_OUTCOMES.includes(outcome))
    return { error: `worker_outcome must be one of ${WORKER_OUTCOMES.join(" | ")}`, status: 400 };
  // ATOMIC WIN: flip active→completing in one conditional UPDATE. This is
  // the race arbiter — it precedes ALL side effects (observe, quotaConsume,
  // accounting, receipts). The loser replays the winner's result; nothing
  // double-executes. Mirrors the claim path's atomic lease win.
  // FIX C3 (2026-10-10): stamp completing_at atomically with the flip, in
  // SQLite datetime('now') format (NOT ISO — the stuck sweep compares against
  // datetime('now','-15 minutes'); ISO's 'T' > ' ' breaks the comparison).
  // The stuck-completion sweep keys on this flip time, so it can never reset
  // an in-flight completion back to 'active' mid-flight.
  const flip = await db.prepare(
    "UPDATE claims SET status='completing', completing_at=? WHERE claim_id=? AND status='active'"
  ).bind(minutesAgo(0), claim_id).run();
  if ((((flip.meta) || {}).changes || 0) === 0) {
    const done = await db.prepare("SELECT status, completion_summary FROM claims WHERE claim_id=?").bind(claim_id).first();
    if (done && done.status === "completed") {
      const s = JSON.parse(done.completion_summary || "{}");
      return { ...s, idempotent: true, note: "already completed — no duplicate accounting", status: 200 };
    }
    return { error: `claim not active (status=${(done && done.status) || "unknown"}) — completion rejected`, status: 409 };
  }
  // Lease expiry (W4): a worker that outruns its lease does not get to
  // resurrect the job. The governor replans; the worker is told EXPIRED.
  if (Date.parse(claim.lease_expiry) < Date.now()) {
    await expireClaim(db, claim, "worker completed after lease expiry");
    return { error: "LEASE_EXPIRED — completion rejected, job replanned", status: 409, worker_outcome: "EXPIRED" };
  }

  const lease_id = claim.lease_id;
  const ex = await db.prepare("SELECT * FROM executions WHERE claim_id=?").bind(claim_id).first();
  const job = await db.prepare("SELECT * FROM jobs WHERE id=?").bind(claim.job_id).first();
  let resource = await db.prepare("SELECT * FROM resources WHERE id=?").bind(claim.resource_id).first();
  const provider = await db.prepare("SELECT * FROM providers WHERE id=?").bind(resource.provider_id).first();
  const spec = JSON.parse(job.spec);
  const policy_version = await policyVersion(db);
  const error_class = b.error_class || null;
  // Worker-asserted latency is evidence, not fact: clamp to a sane range.
  // Negative or absurd values (999999999) are lies or bugs, not measurements.
  const lat = Math.max(0, Math.min(3600000, Number(b.latency_ms) || 0));
  const succeeded = outcome === "SUCCEEDED";

  // Governor-side outcome mapping. The worker's terminal is evidence;
  // the taxonomy, verification, and terminal state are the governor's.
  let taxonomy;
  if (outcome === "SUCCEEDED") taxonomy = "ok";
  else if (outcome === "FAILED_RETRYABLE") taxonomy = "RETRYABLE_FAILURE";
  else if (outcome === "FAILED_PERMANENT") taxonomy = taxonomyOf(error_class || "UNKNOWN");
  else if (outcome === "QUARANTINED") taxonomy = "PROVIDER_FAILURE";
  else taxonomy = "RETRYABLE_FAILURE"; // EXPIRED is time-bound

  const q0 = await db.prepare("SELECT used_window FROM quotas WHERE resource_id=?").bind(resource.id).first();
  const health_before = resource.state;
  const quota_before = (q0 && q0.used_window) || 0;
  await observe(db, resource.id, succeeded, lat, "claimant: " + String(succeeded ? "execution ok" : `${outcome}${error_class ? "/" + error_class : ""}: ${b.error || ""}`).slice(0, 180));
  await quotaConsume(db, resource.id);
  // A completed delegated execution at $0 is measured economics, not a claim.
  if (succeeded) await db.prepare("UPDATE resources SET cost_basis='measured' WHERE id=?").bind(resource.id).run();

  // A 429 is a measured quota ceiling: the rate that triggered it is an
  // honest upper bound. Record it as measured, not asserted.
  // SPREE FIX (2026-10-09): require corroboration before throttling.
  // A single worker's 429 is recorded as evidence only; throttle requires
  // 2+ independent worker reports or a governor observation. Prevents a
  // compromised worker from DoSing shared resources with fake 429s.
  if (error_class === "RATE_LIMITED") {
    const recent = await db.prepare(
      `SELECT detail FROM health_observations WHERE resource_id=?
       ORDER BY observed_at DESC LIMIT 5`
    ).bind(resource.id).all();
    const rows = recent.results || [];
    let consecWorker = 0;
    for (const h of rows) {
      const d = h.detail || "";
      if (d.startsWith("claimant:") && d.includes("RATE_LIMITED")) consecWorker++;
      else break;
    }
    const governorSeen = rows.some((h) => !(h.detail || "").startsWith("claimant:") && (h.detail || "").includes("RATE_LIMITED"));
    if (consecWorker >= 2 || governorSeen) {
      const w = await db.prepare("SELECT used_window FROM quotas WHERE resource_id=?").bind(resource.id).first();
      const ceil = Math.max(1, (w && w.used_window) || 1);
      const has = await db.prepare("SELECT resource_id FROM quotas WHERE resource_id=?").bind(resource.id).first();
      if (has) await db.prepare("UPDATE quotas SET limit_per_min=?, measured=1, measured_at=? WHERE resource_id=?").bind(ceil, now(), resource.id).run();
      else await db.prepare("INSERT INTO quotas (resource_id, limit_per_min, measured, measured_at) VALUES (?,?,1,?)").bind(resource.id, ceil, now()).run();
    }
    // Uncorroborated 429: logged as evidence, no throttle applied.
  }

  // Requested vs actually-executed capacity (W9). The receipt exposes the
  // discrepancy; requested capacity ≠ actually executed capacity.
  const rconf = JSON.parse(resource.config || "{}");
  const requested_model = rconf.model || null;
  const actual_model = b.model_actual || null;
  const model_match = requested_model && actual_model ? (requested_model === actual_model ? 1 : 0) : null;
  if (model_match === 0)
    await db.prepare("INSERT INTO evidence_log (resource_id,from_state,to_state,trigger,detail) VALUES (?,?,?,?,?)")
      .bind(resource.id, resource.evidence_state, resource.evidence_state, "model-mismatch",
        `requested ${requested_model}, provider executed ${actual_model}`).run();

  // Independent verification: SUCCEEDED with empty content is not success.
  // The worker cannot manufacture semantic success (W8).
  // For inference, SUCCEEDED requires non-empty string content — a missing
  // content field is not "no output to check", it is a failed inference.
  // Non-inference capabilities (compute etc.) may return structured output
  // without a content field; those verify on presence of the output object.
  let verified = true, verifyNote = null;
  let output_hash = null;
  if (succeeded) {
    const content = b.output && b.output.content;
    const isInference = (spec.capability || "") === "inference";
    // INV7: empty output is never success. An empty object {} carries zero
    // evidence (REDTEAM-2: {} verified as ok — battery-v2 X11 gap persisted).
    const outputHasSubstance = b.output != null
      && (typeof b.output !== "object" || Object.keys(b.output).length > 0)
      && (content === undefined || (typeof content === "string" && content.length > 0));
    verified = isInference
      ? (typeof content === "string" && content.length > 0)
      : outputHasSubstance;
    if (!verified) verifyNote = isInference
      ? "worker reported SUCCEEDED with empty/missing content for inference"
      : "worker reported SUCCEEDED with empty output (INV7)";
    else output_hash = await sha256hex(JSON.stringify(b.output));
  }
  const finalTaxonomy = succeeded ? (verified ? "ok" : "VERIFICATION_FAILURE") : taxonomy;
  const failed = !succeeded || !verified;

  // QUARANTINED is a worker signal; quarantine itself is the governor's
  // decision. A worker crying wolf on a healthy resource only records a
  // failed observation; on an already-degraded resource it quarantines.
  if (outcome === "QUARANTINED" && (resource.state === "DEGRADED" || resource.state === "THROTTLED")) {
    await db.prepare("UPDATE resources SET state='QUARANTINED' WHERE id=?").bind(resource.id).run();
    await db.prepare("INSERT INTO evidence_log (resource_id,from_state,to_state,trigger,detail) VALUES (?,?,?,?,?)")
      .bind(resource.id, "health:" + health_before, "health:QUARANTINED", "worker-quarantine-signal",
        `worker ${worker.worker_id} reported QUARANTINED: ${(b.error || "").slice(0, 120)}`).run();
  }

  const q2 = await db.prepare("SELECT used_window FROM quotas WHERE resource_id=?").bind(resource.id).first();
  resource = await db.prepare("SELECT * FROM resources WHERE id=?").bind(resource.id).first();
  const quota_after = (q2 && q2.used_window) || 0;

  // ACCOUNTING — the governor's books, never the worker's assertion (W6).
  // A worker submitting cost=$0 (or $999) for a paid execution is ignored:
  // measured/declared resource economics win.
  const measured_cost = costOf(resource);
  const cost_basis = resource.cost_basis || "unknown";
  const accounting_id = uid("acct");
  await db.prepare("INSERT INTO accounting (id,execution_id,claim_id,job_id,resource_id,measured_cost,cost_basis,quota_effect) VALUES (?,?,?,?,?,?,?,?)")
    .bind(accounting_id, ex.id, claim_id, job.id, resource.id, measured_cost, cost_basis,
      JSON.stringify({ quota_before, quota_after })).run();

  // Token usage is the scarce resource: persist the provider's reported
  // usage (the claimant captures output.usage) on the execution row, not
  // just buried in the receipt's decision JSON. Receipts stay blind no more.
  const usage_json = b.output && b.output.usage ? JSON.stringify(b.output.usage) : null;
  await db.prepare("UPDATE executions SET result=?, output_hash=?, error=?, error_class=?, failure_taxonomy=?, finished_at=?, requested_model=?, actual_model=?, model_match=?, usage_json=? WHERE id=?")
    .bind(failed ? "error" : "ok", output_hash, failed ? (verifyNote || b.error || null) : null,
      failed ? (verifyNote ? "VERIFICATION_FAILED" : error_class) : null,
      finalTaxonomy, now(), requested_model, actual_model, model_match, usage_json, ex.id).run();
  await db.prepare("UPDATE leases SET state=? WHERE id=?").bind(failed ? "released" : "consumed", lease_id).run();

  // Terminal state: retryable/time-bound failures defer for replan (bounded);
  // anything else fails honestly with its taxonomy. Never a bare "failed".
  const deferCount = job.defer_count || 0;
  const retryable = failed && (finalTaxonomy === "RETRYABLE_FAILURE" || finalTaxonomy === "QUOTA_FAILURE");
  const state = !failed ? "done" : retryable && deferCount < 8 ? "deferred" : "failed";
  if (failed && state === "deferred")
    await db.prepare("UPDATE jobs SET defer_count=defer_count+1 WHERE id=?").bind(job.id).run();
  await db.prepare("UPDATE jobs SET state=? WHERE id=?").bind(state, job.id).run();

  const leaseSnap = await db.prepare("SELECT * FROM leases WHERE id=?").bind(lease_id).first();
  const receipt_id = uid("rcpt");
  const completionTrace = [
    { gate: "CLAIM", pass: true, reason: `claim ${claim_id} by worker ${worker.worker_id}`,
      detail: { lease: lease_id, policy_snapshot: JSON.parse(claim.policy_snapshot || "{}") }, at: now() },
    { gate: "VERIFY", pass: !failed,
      reason: failed ? (verifyNote || `${finalTaxonomy}: ${b.error || ""}`) : "worker output independently verified",
      detail: { output_hash: output_hash ? output_hash.slice(0, 16) + "…" : null,
        requested_model, actual_model, model_match: model_match === null ? "unknown" : !!model_match }, at: now() },
    { gate: "ACCOUNT", pass: true, reason: `job ${state}, lease ${failed ? "released" : "consumed"}`,
      detail: { taxonomy: finalTaxonomy, quota_before, quota_after,
        measured_cost, cost_basis, worker_cost_assertion: "ignored — governor's books" }, at: now() },
  ];
  await db.prepare(
    `INSERT INTO receipts (id,job_id,execution_id,attempt_id,provider,resource,started_at,finished_at,
      input_hash,output_hash,result,cost,cost_before,cost_after,quota_before,quota_after,
      health_before,health_after,policy_version,lease,decision,fallback_chain,errors,claim_id,worker_outcome)
     VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)`
  ).bind(receipt_id, job.id, ex.id, ex.id, provider.id, resource.id, ex.started_at, now(),
    ex.request_hash, output_hash, state === "done" ? "ok" : state, measured_cost, 0, measured_cost,
    String(quota_before), String(quota_after), health_before, resource.state, policy_version,
    JSON.stringify(leaseSnap),
    JSON.stringify({ trace: completionTrace, delegated: true, worker: worker.worker_id,
      vote_model: (spec.input && spec.input.model) || null,
      output: !failed ? b.output : null }),
    JSON.stringify([{ attempt: ex.id, claim: claim_id, resource: resource.id, lease: lease_id, executed: true,
      taxonomy: finalTaxonomy, worker_outcome: outcome,
      error_class: failed ? (verifyNote ? "VERIFICATION_FAILED" : error_class) : null,
      error: failed ? (verifyNote || b.error) : null }]),
    failed ? `${resource.id} [${finalTaxonomy}]: ${verifyNote || b.error}` : null,
    claim_id, outcome).run();

  // PROTOCOL AMENDMENTS (Zackary approved 2026-10-07, v0.6.3):
  // A1 — governor-side ternary extraction: the worker's output may carry a
  //   ternary_verdict (+1 proven | 0 unknown | -1 disproven). The governor
  //   extracts it into a FIRST-CLASS receipt field — never buried in JSON.
  //   The model is the subject, never the judge: the verdict is the worker's
  //   deterministic check result, recorded as evidence.
  // A2 — auto-prune on -1: disproven work prunes its dependents in O(1).
  //   Deferred jobs with the same input_hash (same work) or an explicit
  //   spec.depends_on pointing at the disproven job are marked failed with
  //   taxonomy DISPROVEN_DEPENDENCY + a prune receipt each. No consensus
  //   debate, no wasted replan tokens.
  // A3 — governor-minted needs_evidence: an Unknown verdict (0) on a
  //   succeeded execution mints a needs_evidence job the swarm claims via
  //   /v1/claim. The Exchange IS the queue — no daemon. Bounded: evidence
  //   jobs never mint further evidence jobs (evidence_depth < 2).
  let ternary_verdict = null;
  if (!failed && b.output && [1, 0, -1].includes(b.output.ternary_verdict)) {
    ternary_verdict = b.output.ternary_verdict;
    await db.prepare("UPDATE receipts SET ternary_verdict=? WHERE id=?")
      .bind(ternary_verdict, receipt_id).run();
  }
  // Voting A2: stamp ballot_id on ANY successful vote-job receipt. Honest
  // worker votes carry no ternary_verdict, so this must NOT live inside the
  // ternary branch (REDTEAM-2: every honest vote was invisible to tallies).
  if (!failed && spec.ballot_id)
    await db.prepare("UPDATE receipts SET ballot_id=? WHERE id=?")
      .bind(spec.ballot_id, receipt_id).run();
  if (ternary_verdict === -1) {
    // SPREE FIX (2026-10-09): -1 is a PROPOSAL, not a verdict. A single
    // worker's -1 marks dependents as pending_disproof; prune requires
    // corroboration from a second independent worker. Prevents a compromised
    // worker from destroying legitimate work with one malicious completion.
    // HARDENING (2026-10-10, external review P1): corroboration counts
    // DISTINCT ELIGIBLE WORKERS, not proposals. The old code counted
    // proposals (COUNT(*)), so one compromised worker submitting two -1s
    // for the same input_hash pruned legitimate work by itself. Eligible =
    // registered and not disabled in workers. The current worker's own
    // not-yet-stamped proposal counts as one vote only if it has not
    // already proposed for this input_hash.
    const deps = await db.prepare(
      `SELECT id FROM jobs WHERE state='deferred' AND id != ?
       AND (input_hash = ? OR json_extract(spec, '$.depends_on') = ?)`
    ).bind(job.id, ex.request_hash, job.id).all();
    const witnesses = await db.prepare(
      `SELECT COUNT(DISTINCT c.worker_id) AS w
       FROM receipts r
       JOIN claims c ON c.claim_id = r.claim_id
       JOIN workers w2 ON w2.worker_id = c.worker_id AND COALESCE(w2.disabled, 0) = 0
       WHERE r.ternary_verdict = -1
         AND json_extract(r.decision, '$.disproof_proposal') = 1
         AND r.job_id IN (SELECT id FROM jobs WHERE input_hash = ?)`
    ).bind(ex.request_hash).first();
    const alreadyProposed = await db.prepare(
      `SELECT 1 FROM receipts r
       JOIN claims c ON c.claim_id = r.claim_id
       WHERE r.ternary_verdict = -1
         AND json_extract(r.decision, '$.disproof_proposal') = 1
         AND c.worker_id = ?
         AND r.job_id IN (SELECT id FROM jobs WHERE input_hash = ?)
       LIMIT 1`
    ).bind(worker.worker_id, ex.request_hash).first();
    const votes = (witnesses?.w || 0) + (alreadyProposed ? 0 : 1);
    if (votes >= 2) {
      // ATOMIC PRUNE: the proposal->prune transition runs as one D1 batch
      // (atomic on D1). Dep updates are conditional on state='deferred' and
      // prune receipts use deterministic ids (rcpt-prune-<dep>-<job>) with
      // INSERT..ON CONFLICT DO NOTHING, so a raced double-prune cannot
      // double-account. Residual race: two -1 completions that both read
      // votes<2 before either stamps both record proposals and neither
      // prunes — fail-safe (no prune), the next -1 completes the quorum.
      const stmts = [];
      for (const dep of (deps.results || [])) {
        const pruneId = `rcpt-prune-${dep.id}-${job.id}`;
        stmts.push(db.prepare(
          "UPDATE jobs SET state='failed' WHERE id=? AND state='deferred'").bind(dep.id));
        stmts.push(db.prepare(
          "UPDATE leases SET state='released' WHERE job_id=? AND state='active'").bind(dep.id));
        stmts.push(db.prepare(
          `INSERT INTO receipts (id,job_id,result,policy_version,decision,errors,ternary_verdict)
           VALUES (?,?,?,?,?,?,?)
           ON CONFLICT(id) DO NOTHING`
        ).bind(pruneId, dep.id, "failed", policy_version,
          JSON.stringify({ pruned: true, trigger: "auto-prune",
            disproven_by: job.id, claim: claim_id, witnesses: votes,
            reason: "dependency disproven (-1) — O(1) prune, no replan" }),
          `pruned: dependency ${job.id} disproven`, -1));
        stmts.push(db.prepare(
          "INSERT INTO evidence_log (resource_id,from_state,to_state,trigger,detail) VALUES (?,?,?,?,?)")
          .bind("job:" + dep.id, "deferred", "failed", "auto-prune",
            `disproven by ${job.id} (claim ${claim_id}) — ${votes} distinct witnesses`));
      }
      if (stmts.length) await db.batch(stmts);
      if ((deps.results || []).length)
        await db.prepare("INSERT INTO evidence_log (resource_id,from_state,to_state,trigger,detail) VALUES (?,?,?,?,?)")
          .bind("job:" + job.id, state, state, "auto-prune",
            `verdict -1 pruned ${(deps.results || []).length} dependent job(s) — ${votes} distinct witnesses`).run();
    } else {
      // Single -1: record as proposal, do NOT prune. Awaiting corroboration.
      await db.prepare("UPDATE receipts SET decision=json_patch(decision, ?) WHERE id=?")
        .bind(JSON.stringify({ disproof_proposal: 1, votes: 1, proposer: worker.worker_id }), receipt_id).run();
    }
  }
  if (ternary_verdict === 0 && !failed) {
    const evDepth = (spec.evidence_depth || 0);
    if (spec.organelle !== "exchange-governor" && evDepth < 2) {
      const evSpec = {
        organelle: "exchange-governor",
        capability: (b.output && b.output.needs_evidence_capability) || spec.capability || "cognitive",
        input: { needs_evidence_for: job.id, claim_id,
          gap: (b.output && b.output.evidence_gap) || "unknown verdict — evidence required",
          evidence_depth: evDepth + 1 },
        data_class: spec.data_class || "public", max_cost: 0, priority: 7,
        evidence_depth: evDepth + 1,
      };
      const evId = uid("job");
      const evHash = await sha256hex(JSON.stringify(evSpec.input));
      await db.prepare(
        "INSERT INTO jobs (id,organelle,capability,input_hash,spec,data_class,max_cost,priority,state,defer_count) VALUES (?,?,?,?,?,?,?,?,?,?)"
      ).bind(evId, evSpec.organelle, evSpec.capability, evHash, JSON.stringify(evSpec),
        evSpec.data_class, 0, 7, "submitted", 0).run();
      const evRes = await runJob(db, evId, evSpec, env);
      await db.prepare("INSERT INTO evidence_log (resource_id,from_state,to_state,trigger,detail) VALUES (?,?,?,?,?)")
        .bind("job:" + job.id, state, state, "needs-evidence",
          `verdict 0 minted evidence job ${evId} -> ${evRes.state || evRes.error}`).run();
    }
  }

  const summary = { job_id: job.id, state, receipt_id, lease_id, claim_id, resource: resource.id,
    taxonomy: finalTaxonomy, worker_outcome: outcome, ternary_verdict,
    output: !failed ? b.output : null, error: failed ? (verifyNote || b.error) : null };
  await db.prepare("UPDATE claims SET status='completed', worker_outcome=?, completed_at=?, completion_summary=? WHERE claim_id=? AND status='completing'")
    .bind(outcome, now(), JSON.stringify(summary), claim_id).run();

  return { ...summary, status: 200 };
}

// Expire a claim: the worker outran its lease (or vanished — zombie sweep).
// The execution is marked UNCERTAIN (W10): the provider may have executed,
// so the Exchange preserves the uncertainty instead of pretending retries
// are free. The job defers for replan; nothing is auto-marked success.
async function expireClaim(db, claim, reason) {
  await db.prepare("UPDATE claims SET status='expired', worker_outcome='EXPIRED', completed_at=? WHERE claim_id=?")
    .bind(now(), claim.claim_id).run();
  await db.prepare("UPDATE leases SET state='expired' WHERE id=?").bind(claim.lease_id).run();
  await db.prepare("UPDATE executions SET result='uncertain', error=?, finished_at=? WHERE claim_id=?")
    .bind("lease expired before completion — execution uncertain: " + reason, now(), claim.claim_id).run();
  const job = await db.prepare("SELECT defer_count FROM jobs WHERE id=?").bind(claim.job_id).first();
  const dc = (job && job.defer_count) || 0;
  if (dc < 8) {
    await db.prepare("UPDATE jobs SET state='deferred', defer_count=defer_count+1 WHERE id=?").bind(claim.job_id).run();
  } else {
    await db.prepare("UPDATE jobs SET state='failed' WHERE id=?").bind(claim.job_id).run();
  }
  await db.prepare(
    `INSERT INTO receipts (id,job_id,execution_id,attempt_id,provider,resource,started_at,finished_at,
      input_hash,output_hash,result,cost,policy_version,decision,errors,claim_id,worker_outcome)
     VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)`
  ).bind(uid("rcpt"), claim.job_id, null, null, null, claim.resource_id, claim.claimed_at, now(),
    claim.input_hash, null, dc < 8 ? "deferred" : "failed", 0, await policyVersion(db),
    JSON.stringify({ trace: [{ gate: "EXPIRE", pass: true, reason, at: now(),
      detail: { claim: claim.claim_id, uncertainty: "provider may have executed — retries are not free" } }] }),
    `claim ${claim.claim_id}: ${reason}`, claim.claim_id, "EXPIRED").run();
}

// Zombie sweep: claims that outran their leases with no completion.
// Lease → TIMEOUT → REPLAN. No permanently stuck work.
//
// TEMPORAL FIX (audit 2026-10-06): lease_expiry is ISO-8601 ("T" separator)
// but SQLite datetime('now') uses a space separator. String comparison
// "2026-10-06T10:00:00Z" < "2026-10-06 22:00:00" is FALSE ('T' > ' '),
// so same-day expiries never swept. Compare against a bound ISO string.
async function sweepExpiredClaims(db) {
  const nowIso = new Date().toISOString();
  const rows = await db.prepare("SELECT * FROM claims WHERE status='active' AND lease_expiry < ?").bind(nowIso).all();
  const out = [];
  for (const claim of (rows.results || [])) {
    await expireClaim(db, claim, "zombie sweep — no completion before lease expiry");
    out.push({ claim: claim.claim_id, job: claim.job_id, outcome: "EXPIRED" });
  }
  // STUCK COMPLETIONS: death between the atomic completing-flip and the
  // final completed UPDATE leaves a claim in 'completing' — uncompletable
  // (flip requires status='active'), invisible to the active sweep.
  // Reset to 'active' after 15min so the worker can retry; the flip
  // re-arbitrates. 15min >> any legitimate completion (lease max 10min).
  // FIX C3 (2026-10-10): key on the completing-flip time (completing_at),
  // NOT claim age. Keying on claimed_at let the sweep reset an IN-FLIGHT
  // completion (old claim, fresh flip), whose final conditional UPDATE then
  // no-op'd — the retry double-executed (2 receipts + 2 accounting rows).
  // Rows predating the completing_at column (NULL) fall back to claimed_at.
  const stuck = await db.prepare(
    `SELECT claim_id, job_id FROM claims WHERE status='completing' AND (
       completing_at < datetime('now','-15 minutes')
       OR (completing_at IS NULL AND claimed_at < datetime('now','-15 minutes'))
     ) LIMIT 10`
  ).all();
  for (const claim of ((stuck.results) || [])) {
    await db.prepare("UPDATE claims SET status='active' WHERE claim_id=? AND status='completing'").bind(claim.claim_id).run();
    out.push({ claim: claim.claim_id, job: claim.job_id, outcome: "RESET_COMPLETING_TO_ACTIVE" });
  }
  // ORPHAN LEASES: death between the atomic claim UPDATE and the claim-row
  // INSERT leaves a lease `claimed` with no claim row — unclaimable
  // (pickup needs state='active'), uncompletable (needs claim_id),
  // invisible to the claims sweep. Expire them after 15min, defer the job
  // for replan, write a receipt. The orphan row persists as evidence.
  const orphans = await db.prepare(
    `SELECT l.* FROM leases l LEFT JOIN claims c ON c.lease_id=l.id
     WHERE l.state='claimed' AND c.claim_id IS NULL
     AND l.created_at < datetime('now','-15 minutes') LIMIT 10`).all();
  for (const l of (orphans.results || [])) {
    await db.prepare("UPDATE leases SET state='expired' WHERE id=?").bind(l.id).run();
    const job = await db.prepare("SELECT defer_count FROM jobs WHERE id=?").bind(l.job_id).first();
    const dc = (job && job.defer_count) || 0;
    if (dc < 8) await db.prepare("UPDATE jobs SET state='deferred', defer_count=defer_count+1 WHERE id=?").bind(l.job_id).run();
    else await db.prepare("UPDATE jobs SET state='failed' WHERE id=?").bind(l.job_id).run();
    await db.prepare(
      `INSERT INTO receipts (id,job_id,execution_id,provider,resource,started_at,finished_at,
        input_hash,result,cost,policy_version,decision,errors)
       VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?)`
    ).bind(uid("rcpt"), l.job_id, null, null, l.resource_id, l.created_at, now(),
      null, dc < 8 ? "deferred" : "failed", 0, await policyVersion(db),
      JSON.stringify({ trace: [{ gate: "ORPHAN", pass: true, at: now(),
        reason: "lease claimed but no claim row — process died mid-claim; expired, job replanned",
        detail: { lease: l.id } }] }),
      `orphan lease ${l.id}: claimed with no claim row`).run();
    out.push({ orphan_lease: l.id, job: l.job_id, outcome: "EXPIRED" });
  }
  return out;
}

// Provider disappeared: every probeable resource of a provider fails with
// NETWORK on its latest observation → provider DOWN, its resources UNKNOWN
// (forced), deferred work replans elsewhere. Any success clears it.
async function providerDownCheck(db) {
  const out = [];
  const provs = await db.prepare("SELECT * FROM providers").all();
  for (const p of (provs.results || [])) {
    const res = await db.prepare("SELECT id FROM resources WHERE provider_id=?").bind(p.id).all();
    const ids = (res.results || []).map((r) => r.id);
    if (!ids.length) continue;
    let anyProbeable = false, allNetFail = true;
    for (const id of ids) {
      const r = await db.prepare("SELECT config,kind FROM resources WHERE id=?").bind(id).first();
      if (!adapterFor(r)) continue;
      anyProbeable = true;
      const last = await db.prepare(
        "SELECT ok,detail FROM health_observations WHERE resource_id=? ORDER BY observed_at DESC LIMIT 1"
      ).bind(id).first();
      if (!last || last.ok === 1 || !(last.detail || "").includes("NETWORK")) { allNetFail = false; break; }
    }
    if (anyProbeable && allNetFail && p.state !== "DOWN") {
      await db.prepare("UPDATE providers SET state='DOWN' WHERE id=?").bind(p.id).run();
      for (const id of ids)
        await setEvidence(db, id, "UNKNOWN", "provider-down", `provider ${p.id} unreachable (NETWORK on all probeable resources)`, true);
      out.push({ provider: p.id, down: true });
    } else if (p.state === "DOWN" && !allNetFail) {
      await db.prepare("UPDATE providers SET state='UNKNOWN' WHERE id=?").bind(p.id).run();
      out.push({ provider: p.id, recovered: true });
    }
  }
  return out;
}

/* ------------------------------------------------------------------ */
/* Resource discovery + health                                          */
/* ------------------------------------------------------------------ */

/* LEARNING / POLICY SEPARATION — INVARIANT, READ BEFORE TOUCHING observe().
 *
 * Observations may write ONLY these columns/tables:
 *   health_observations (append), resources(state, evidence_state,
 *   evidence_source, evidence_at, cost_basis, throttled_until,
 *   suspended_until, credential_state), quotas(used_window,
 *   window_started_at, last_checked_at, measured, measured_at),
 *   evidence_log (append), executions (attempt rows), receipts (append),
 *   leases (lifecycle), providers(state).
 *
 * Observations NEVER write the policies table. No probe, execution, score,
 * or learning step may insert/update/delete a policy row. Policy changes
 * require an explicit, separately authorized action outside the observation
 * path (currently: code change + redeploy; /v1/seed only INSERT OR IGNOREs
 * defaults on fresh databases).
 *
 * The system may learn a provider is unreliable. It may not learn that an
 * unauthorized provider is authorized. It may learn a resource is cheap. It
 * may not learn permission to spend money. Evidence updates knowledge;
 * policy controls permission.
 */
async function listResources(db) {
  const rs = await db.prepare("SELECT * FROM resources").all();
  const out = [];
  for (const r of rs.results) {
    const caps = await db.prepare("SELECT capability,detail FROM resource_capabilities WHERE resource_id=?").bind(r.id).all();
    const q = await db.prepare("SELECT * FROM quotas WHERE resource_id=?").bind(r.id).first();
    const h = await db.prepare(
      "SELECT COUNT(*) c, SUM(ok) ok, AVG(latency_ms) lat FROM (SELECT * FROM health_observations WHERE resource_id=? ORDER BY observed_at DESC LIMIT 20)"
    ).bind(r.id).first();
    out.push({
      id: r.id, provider: r.provider_id, name: r.name, kind: r.kind, state: r.state,
      cost_class: r.cost_class, cost_basis: r.cost_basis || "unknown", sovereignty: r.sovereignty,
      evidence_state: r.evidence_state || "UNKNOWN", evidence_source: r.evidence_source || null,
      usable: EVIDENCE_USABLE.includes(r.evidence_state),
      capabilities: (caps.results || []).map((c) => c.capability),
      quota: q ? { limit_per_min: q.limit_per_min, used_window: q.used_window, basis: q.measured ? "measured" : "asserted" } : null,
      health: h && h.c ? { probes: h.c, success_rate: +(h.ok / h.c).toFixed(3), avg_latency_ms: Math.round(h.lat || 0) } : null,
    });
  }
  return out;
}

async function observe(db, resource_id, ok, latency_ms, detail) {
  await db.prepare("INSERT INTO health_observations (resource_id,ok,latency_ms,detail) VALUES (?,?,?,?)")
    .bind(resource_id, ok ? 1 : 0, latency_ms || null, detail || null).run();
  const prev = (await db.prepare("SELECT state FROM resources WHERE id=?").bind(resource_id).first() || {}).state || "UNKNOWN";
  // Roll up state: 3 consecutive failures → QUARANTINED; any recent success → AVAILABLE/DEGRADED
  // ANTI-POISONING: worker-reported failures (detail "claimant: …") are evidence,
  // not verdicts. Three silent worker failures cap at DEGRADED — automatic
  // QUARANTINE requires at least one governor-observed (non-claimant) failure
  // as corroboration. A worker that believes capacity is dead must say so
  // explicitly via the QUARANTINED terminal outcome (attributed, auditable),
  // not smuggle it in through failure counts. Governor decision stays governor's.
  const recent = await db.prepare(
    "SELECT ok, detail FROM health_observations WHERE resource_id=? ORDER BY observed_at DESC LIMIT 3"
  ).bind(resource_id).all();
  const rows = recent.results || [];
  let state = null;
  const allFail = rows.length >= 3 && rows.every((r) => r.ok === 0);
  const anyGovernorObserved = rows.some((r) => !(r.detail || "").startsWith("claimant:"));
  if (allFail && anyGovernorObserved) state = "QUARANTINED";
  else if (allFail) state = "DEGRADED"; // worker-only failures: visible, capped
  else if (rows.length && rows[0].ok === 1) {
    const rate = rows.filter((r) => r.ok === 1).length / rows.length;
    state = rate >= 0.8 ? "AVAILABLE" : "DEGRADED";
  }
  // THROTTLED with exponential backoff: a 429 is not a failure of the
  // resource, it is the resource telling us to back off. 2^(n-1) minutes,
  // capped at 60. Overrides the failure rollup — throttling is not death.
  // Error-class detection uses substring matching, not prefix matching:
  // callers prepend context ("verify gauntlet: ", "integrity revalidation: ")
  // ahead of the "CLASS: detail" payload. A 429 is a 429 wherever it is seen.
  const hasClass = (cls) => !ok && detail && detail.includes(cls);
  const isWorkerReported = (detail || "").startsWith("claimant:");
  // CORROBORATION RULE (anti-DoS): a worker-asserted error_class is evidence,
  // not verdict. One lied AUTH_ERROR or RATE_LIMITED packet must never PARK
  // or THROTTLE a resource. Worker-reported signals require corroboration —
  // 2+ consecutive worker reports, or a governor observation agreeing —
  // before they drive disruptive transitions. Governor-observed signals act
  // immediately (the governor probed it directly).
  const corroborated = async (cls) => {
    if (!isWorkerReported) return true;
    const hist = await db.prepare(
      "SELECT detail FROM health_observations WHERE resource_id=? ORDER BY observed_at DESC LIMIT 5"
    ).bind(resource_id).all();
    const rows = hist.results || [];
    // 2+ consecutive worker reports of this class, or any governor report
    let consecWorker = 0;
    for (const h of rows) {
      const d = h.detail || "";
      if (d.startsWith("claimant:") && d.includes(cls)) consecWorker++;
      else break;
    }
    if (consecWorker >= 2) return true;
    return rows.some((h) => !(h.detail || "").startsWith("claimant:") && (h.detail || "").includes(cls));
  };
  if (hasClass("RATE_LIMITED")) {
    if (!(await corroborated("RATE_LIMITED"))) {
      // Uncorroborated worker 429: recorded as evidence, no throttle.
      // Falls through to the normal failure rollup below.
      await db.prepare("INSERT INTO evidence_log (resource_id,from_state,to_state,trigger,detail) VALUES (?,?,?,?,?)")
        .bind(resource_id, "health:" + prev, "health:" + prev, "uncorroborated-429",
          "worker-reported RATE_LIMITED without corroboration — recorded, not acted on").run();
    } else {
      const hist = await db.prepare(
        "SELECT detail FROM health_observations WHERE resource_id=? ORDER BY observed_at DESC LIMIT 10"
      ).bind(resource_id).all();
      let consec = 0;
      for (const h of (hist.results || [])) { if ((h.detail || "").includes("RATE_LIMITED")) consec++; else break; }
      const backoffMin = Math.min(60, Math.pow(2, Math.max(0, consec - 1)));
      const until = new Date(Date.now() + backoffMin * 60000).toISOString();
      await db.prepare("UPDATE resources SET state='THROTTLED', throttled_until=? WHERE id=?").bind(until, resource_id).run();
      state = "THROTTLED";
      await db.prepare("INSERT INTO evidence_log (resource_id,from_state,to_state,trigger,detail) VALUES (?,?,?,?,?)")
        .bind(resource_id, "health:" + prev, "health:THROTTLED", "throttled-backoff",
          `429 #${consec} in a row — backing off ${backoffMin}m until ${until}`).run();
      return { resource_id, state, throttled_until: until };
    }
  }
  // CREDENTIAL INVALID → disable. An expired/invalid credential is never
  // retried blindly; the resource is parked until explicitly re-verified.
  // But a single worker-asserted AUTH_ERROR is not proof the credential died.
  if (hasClass("AUTH_ERROR")) {
    if (!(await corroborated("AUTH_ERROR"))) {
      await db.prepare("INSERT INTO evidence_log (resource_id,from_state,to_state,trigger,detail) VALUES (?,?,?,?,?)")
        .bind(resource_id, "health:" + prev, "health:" + prev, "uncorroborated-auth-error",
          "worker-reported AUTH_ERROR without corroboration — recorded, not acted on").run();
    } else {
      await db.prepare("UPDATE resources SET credential_state='invalid' WHERE id=?").bind(resource_id).run();
      await setEvidence(db, resource_id, "PARKED", "credential-invalid", detail.slice(0, 200), true);
      return { resource_id, state: prev, credential_disabled: true };
    }
  }
  if (state) {
    if (state === "AVAILABLE" || state === "DEGRADED")
      await db.prepare("UPDATE resources SET state=?, throttled_until=NULL, suspended_until=NULL WHERE id=?").bind(state, resource_id).run();
    else
      await db.prepare("UPDATE resources SET state=? WHERE id=?").bind(state, resource_id).run();
  }
  const finalState = state || prev;
  // Every health transition is durable and auditable.
  if (state && state !== prev) {
    await db.prepare("INSERT INTO evidence_log (resource_id,from_state,to_state,trigger,detail) VALUES (?,?,?,?,?)")
      .bind(resource_id, "health:" + prev, "health:" + state, "health-transition", detail || null).run();
  }
  // Access collapsed on a usable resource: demote evidence (it was real once,
  // now unreachable — kept as proven history, not usable capacity).
  if (finalState === "QUARANTINED") {
    const r = await db.prepare("SELECT evidence_state FROM resources WHERE id=?").bind(resource_id).first();
    const ev = (r && r.evidence_state) || "UNKNOWN";
    if (ev === "AVAILABLE" || ev === "VERIFIED")
      await setEvidence(db, resource_id, "FREE", "quarantine-demote", "was usable, now unreachable", true);
    else if (ev !== "PARKED" && ev !== "UNKNOWN")
      await setEvidence(db, resource_id, "UNKNOWN", "quarantine-demote", "never proven; access collapsed", true);
  }
  return { resource_id, state: finalState };
}

/* ------------------------------------------------------------------ */
/* Governor — IDENTITY → AUTHORITY → DATA → CAPABILITY → EVIDENCE →     */
/* QUOTA → COST → INTEGRITY → (EFFICIENCY scores; scoring ≠ auth) →    */
/* LEASE → EXECUTE. Deterministic. Every gate recorded. No bypass.     */
/* ------------------------------------------------------------------ */
function validateJob(spec) {
  const errs = [];
  if (!spec || typeof spec !== "object") return ["job must be an object"];
  if (!spec.capability) errs.push("capability required");
  if (!spec.input) errs.push("input required");
  const dc = spec.data_class || "public";
  if (!["public", "internal", "sensitive", "secret"].includes(dc)) errs.push("bad data_class");
  if (typeof (spec.max_cost ?? 0) !== "number" || spec.max_cost < 0) errs.push("max_cost must be >= 0");
  if (spec.spend_authority !== undefined && (typeof spec.spend_authority !== "number" || spec.spend_authority < 0))
    errs.push("spend_authority must be >= 0");
  return errs;
}

async function govern(db, spec) {
  // HARD PRE-EXECUTION GATES, in order:
  //   IDENTITY → AUTHORITY → DATA → CAPABILITY → EVIDENCE → QUOTA → COST →
  //   INTEGRITY → (EFFICIENCY scores survivors; scoring is never authorization)
  // LEASE and EXECUTE are recorded later, per attempt, in runJob().
  // No score, retry, failover, urgency, or human convenience bypasses a gate.
  const trace = []; // every gate decision, in order
  // Each gate is timestamped: a receipt must show not just WHAT was decided
  // but WHEN, so "what was known at decision time" is reconstructible.
  const gate = (name, pass, reason, detail) => trace.push({ gate: name, pass, reason, detail: detail || null, at: now() });
  const rejected = [];
  // Gates not reached are recorded as vacuous — explicitly labeled, never
  // faked as passed. A verifier can distinguish "evaluated" from "vacuous".
  const GATE_ORDER = ["IDENTITY", "AUTHORITY", "DATA", "CAPABILITY", "EVIDENCE", "QUOTA", "COST", "INTEGRITY", "EFFICIENCY"];
  const vacuous = () => {
    const recorded = new Set(trace.map((x) => x.gate));
    for (const gn of GATE_ORDER) if (!recorded.has(gn)) gate(gn, true, "vacuous — no candidate reached this gate", {});
  };

  // AUTHORITY (job-level) — bearer key already validated by authed(); record it.
  gate("AUTHORITY", true, "bearer key valid");

  // CAPABILITY — candidate resources exposing the capability.
  const cands = await db.prepare(
    "SELECT r.* FROM resources r JOIN resource_capabilities c ON c.resource_id=r.id WHERE c.capability=?"
  ).bind(spec.capability).all();
  let candidates = cands.results || [];
  gate("CAPABILITY", candidates.length > 0,
    candidates.length ? `${candidates.length} resource(s) expose ${spec.capability}` : `no resource exposes ${spec.capability}`,
    { candidates: candidates.map((c) => c.id) });
  if (!candidates.length) { vacuous(); return { trace, selected: null, rejected, ranked: [] }; }

  // EVIDENCE — promotional or hypothetical capacity is not usable capacity.
  const usable = [];
  for (const r of candidates) {
    const ev = r.evidence_state || "UNKNOWN";
    if (EVIDENCE_USABLE.includes(ev)) usable.push(r);
    else rejected.push({ resource: r.id, gate: "EVIDENCE", reason: `evidence_state=${ev} — not verified usable capacity`, taxonomy: "CAPABILITY_FAILURE", retryable: false });
  }
  gate("EVIDENCE", usable.length > 0,
    usable.length ? `${usable.length} resource(s) with verified usable capacity` : "no candidate has verified usable capacity — listings and trials do not execute",
    { usable: usable.map((r) => r.id), refused: rejected.filter((x) => x.gate === "EVIDENCE").map((x) => x.resource) });
  candidates = usable;
  if (!candidates.length) { vacuous(); return { trace, selected: null, rejected, ranked: [] }; }

  const policies = await db.prepare("SELECT * FROM policies").all();
  const survivors = [];
  for (const r of candidates) {
    let cfg = null;
    try { cfg = JSON.parse(r.config || "{}"); } catch { cfg = null; }
    const adapter = cfg ? adapterFor(r) : null;

    // IDENTITY — adapter provenance known, config parses, registration
    // provenance recorded, provider row exists. Unknown identity → BLOCK.
    const prov = await db.prepare("SELECT id FROM providers WHERE id=?").bind(r.provider_id).first();
    const idProblem = !cfg ? "config unparsable"
      : !adapter ? `no adapter for kind=${r.kind} (unknown provenance)`
      : !r.evidence_source ? "no registration provenance"
      : !prov ? "provider row missing" : null;
    if (idProblem) {
      rejected.push({ resource: r.id, gate: "IDENTITY", reason: `unknown identity: ${idProblem}`, taxonomy: "CAPABILITY_FAILURE", retryable: false });
      continue;
    }
    // AUTHORITY (resource-level) — credential not known-invalid. No answer
    // from the credential system means no authorization: invalid → BLOCK.
    if (r.credential_state === "invalid") {
      rejected.push({ resource: r.id, gate: "AUTHORITY", reason: "credential invalid — resource disabled", taxonomy: "AUTH_FAILURE", retryable: false });
      continue;
    }
    // DATA — policy evaluation per candidate. Sensitive data to keyless
    // lanes dies here, every time, regardless of score.
    let blocked = null;
    for (const p of policies.results || []) {
      const rule = JSON.parse(p.rule);
      const dw = rule.deny_when || {};
      if (dw.resource_trust && cfg.trust === dw.resource_trust && (dw.data_class_in || []).includes(spec.data_class || "public"))
        blocked = `policy ${p.id}: ${p.name}`;
      if (dw.cost_gt === "job.max_cost" && costOf(r) > (spec.max_cost ?? 0))
        blocked = `policy ${p.id}: cost ${costOf(r)} > max_cost ${spec.max_cost ?? 0}`;
    }
    if (blocked) {
      rejected.push({ resource: r.id, gate: "DATA", reason: blocked, taxonomy: "POLICY_FAILURE", retryable: false });
      continue;
    }
    // QUOTA — insufficient or unknown quota → DEFER/BLOCK (retryable when
    // the constraint is time-bound: throttled, exhausted window, unknown
    // limit with rate-limit history).
    const q = await db.prepare("SELECT * FROM quotas WHERE resource_id=?").bind(r.id).first();
    const nowIso = now();
    let quotaReject = null, quotaRetryable = false;
    if (r.state === "THROTTLED" && r.throttled_until && r.throttled_until > nowIso) {
      quotaReject = `throttled until ${r.throttled_until}`; quotaRetryable = true;
    } else if (r.state === "EXHAUSTED" && r.suspended_until && r.suspended_until > nowIso) {
      quotaReject = `exhausted — suspended until recheck at ${r.suspended_until}`; quotaRetryable = true;
    } else if (q && q.limit_per_min && (q.used_window || 0) >= q.limit_per_min) {
      quotaReject = `quota exhausted (${q.used_window}/${q.limit_per_min} per min)`; quotaRetryable = true;
    } else if (!q || !q.limit_per_min) {
      // Rate-limit memory decays: only recent 429s count, and the match is
      // a substring (observers prepend context like "claimant: "). A single
      // ancient 429 must never permanently brick a resource.
      const rl = await db.prepare("SELECT COUNT(*) c FROM health_observations WHERE resource_id=? AND detail LIKE '%RATE_LIMITED%' AND observed_at>?").bind(r.id, minutesAgo(60)).first();
      if (((rl && rl.c) || 0) > 0) { quotaReject = "quota unknown and recent rate-limit history exists"; quotaRetryable = true; }
    }
    if (quotaReject) {
      rejected.push({ resource: r.id, gate: "QUOTA", reason: quotaReject, taxonomy: "QUOTA_FAILURE", retryable: quotaRetryable });
      continue;
    }
    // COST — paid resources can NEVER execute without explicit spend
    // authority. Default MAX_AUTHORIZED_SPEND = $0 stands. A cheap resource
    // never grants itself permission to spend.
    const effective_cost = costOf(r);
    const spend_auth = spec.spend_authority ?? 0;
    // Declared budget cannot exceed spend authority — even on free resources.
    // A max_cost without matching authority is a meaningless declaration;
    // fail closed rather than letting it pass silently (battery v4 QUOTA gap).
    if ((spec.max_cost ?? 0) > spend_auth) {
      rejected.push({ resource: r.id, gate: "COST",
        reason: `declared max_cost $${spec.max_cost} exceeds spend_authority $${spend_auth}`,
        taxonomy: "POLICY_FAILURE", retryable: false });
      continue;
    }
    if (effective_cost > 0 && !(spend_auth > 0 && effective_cost <= spend_auth && effective_cost <= (spec.max_cost ?? 0))) {
      rejected.push({ resource: r.id, gate: "COST",
        reason: `effective cost $${effective_cost} requires explicit spend_authority (authorized $${spend_auth}, max_cost $${spec.max_cost ?? 0})`,
        taxonomy: "POLICY_FAILURE", retryable: false });
      continue;
    }
    if (effective_cost > (spec.max_cost ?? 0)) {
      rejected.push({ resource: r.id, gate: "COST",
        reason: `cost $${effective_cost} exceeds authorized max $${spec.max_cost ?? 0}`,
        taxonomy: "POLICY_FAILURE", retryable: false });
      continue;
    }
    // INTEGRITY — re-read fresh state; quarantined/stale/integrity-invalid →
    // REVALIDATE (one bounded probe) or BLOCK. Never trust the shortlist.
    const fresh = await db.prepare("SELECT state,evidence_state FROM resources WHERE id=?").bind(r.id).first();
    let intReject = null;
    if (["QUARANTINED"].includes(fresh.state)) intReject = `state ${fresh.state}`;
    else if (!EVIDENCE_USABLE.includes(fresh.evidence_state)) intReject = `evidence ${fresh.evidence_state} (changed since shortlist)`;
    else {
      const last = await db.prepare(
        "SELECT observed_at FROM health_observations WHERE resource_id=? ORDER BY observed_at DESC LIMIT 1"
      ).bind(r.id).first();
      const ageMin = last ? (Date.now() - ts(last.observed_at)) / 60000 : 9999;
      if (ageMin > staleAfterEv(fresh.evidence_state)) {
        if (adapter.claimed) {
          // Claimed resources are observed by their claimant, never by the
          // Worker. The claimed probe ALWAYS fails (no credential held) —
          // running it here writes a false failure that demotes the resource
          // and fails every subsequent job at INTEGRITY until a claimant
          // re-promotes it (battery v4: this was the "non-stationary"
          // INTEGRITY boundary). Skip the probe; staleness demotion is the
          // cron's job via demoteCheck.
        } else {
          let p;
          try { p = await adapter.probe(r); } catch (e) { p = { ok: false, detail: String(e).slice(0, 160) }; }
          await observe(db, r.id, p.ok, p.latency_ms, "integrity revalidation: " + (p.detail || "probe"));
          if (!p.ok) intReject = `stale (${Math.round(ageMin)}m) and revalidation failed: ${p.detail}`;
        }
      }
    }
    if (intReject) {
      rejected.push({ resource: r.id, gate: "INTEGRITY", reason: intReject, taxonomy: "RETRYABLE_FAILURE", retryable: true });
      continue;
    }
    survivors.push({ r, cfg, quota: q, adapter, effective_cost });
  }

  // Record per-gate outcomes (aggregates; per-candidate detail in rejected[]).
  for (const gn of ["IDENTITY", "AUTHORITY", "DATA", "EVIDENCE", "QUOTA", "COST", "INTEGRITY"]) {
    const fails = rejected.filter((x) => x.gate === gn);
    const passes = survivors.length;
    if (fails.length) gate(gn, false, `${fails.length} candidate(s) rejected at ${gn}`, { resources: fails.map((x) => x.resource) });
    if (passes) gate(gn, true, `${passes} candidate(s) passed ${gn}`, { resources: survivors.map((s) => s.r.id) });
  }
  if (!survivors.length) { vacuous(); return { trace, selected: null, rejected, ranked: [] }; }

  // EFFICIENCY — deterministic scoring of gate survivors. Scoring ranks;
  // it never authorizes. Economic governor: effective_cost, expected_success
  // (reliability), and human-intervention cost shape the score. Free is not
  // automatically best: an unreliable free lane can lose to a reliable one.
  const scored = [];
  for (const s of survivors) {
    const h = await db.prepare(
      "SELECT COUNT(*) c, SUM(ok) ok, AVG(latency_ms) lat FROM (SELECT * FROM health_observations WHERE resource_id=? ORDER BY observed_at DESC LIMIT 20)"
    ).bind(s.r.id).first();
    const probes = (h && h.c) || 0;
    const reliability = probes ? h.ok / probes : 0.5; // no data → neutral, recorded
    const avgLat = (h && h.lat) || 20000;
    const latencyScore = Math.max(0, 1 - avgLat / 60000);
    const availScore = s.r.state === "AVAILABLE" ? 1 : s.r.state === "DEGRADED" ? 0.6 : 0;
    const quotaScore = s.quota && s.quota.limit_per_min ? 1 - (s.quota.used_window || 0) / s.quota.limit_per_min : 1;
    const costScore = s.r.cost_class === "free" ? 1 : s.r.cost_class === "credit-backed" ? 0.8 : 0.2;
    const sovScore = s.r.sovereignty === "local" ? 1 : s.r.sovereignty === "self-hosted" ? 0.8 : 0.5;
    const expected_success = +reliability.toFixed(3);
    const human_cost = +((s.cfg.trust === "untrusted" ? 0.2 : 0) + (s.r.sovereignty === "external" ? 0.05 : 0)).toFixed(2);
    const economic = +(expected_success * (1 - human_cost) * (s.effective_cost === 0 ? 1 : Math.max(0, 1 - s.effective_cost * 5))).toFixed(4);
    const score = +(0.20 * 1.0 + 0.15 * reliability + 0.10 * latencyScore + 0.10 * availScore
      + 0.10 * quotaScore + 0.10 * costScore + 0.05 * sovScore + 0.20 * economic).toFixed(4);
    scored.push({
      resource: s.r.id, score,
      breakdown: { capability_fit: 1.0, reliability: +reliability.toFixed(3), latency_ms: Math.round(avgLat), latency_score: +latencyScore.toFixed(3),
        availability: availScore, quota_remaining: +quotaScore.toFixed(3), cost_class: s.r.cost_class, cost_score: costScore,
        sovereignty: s.r.sovereignty, sovereignty_score: sovScore, health_probes: probes,
        effective_cost: s.effective_cost, expected_success, human_intervention_cost: human_cost, economic_score: economic },
    });
  }
  scored.sort((a, b) => b.score - a.score);
  const winner = scored[0];
  for (const sc of scored.slice(1)) rejected.push({ resource: sc.resource, gate: "EFFICIENCY", reason: `outscored by ${winner.resource} (${sc.score} < ${winner.score})`, taxonomy: null, retryable: false });
  gate("EFFICIENCY", true, `selected ${winner.resource} (score ${winner.score}), ${scored.length - 1} fallback(s)`, { candidates: scored });
  return { trace, selected: { resource_id: winner.resource, score: winner.score, breakdown: winner.breakdown }, rejected,
    ranked: scored.map((sc) => ({ resource_id: sc.resource, score: sc.score, breakdown: sc.breakdown })) };
}

function costOf(r) {
  // v0: free resources cost 0. Paid resources would carry config.usd_per_unit.
  if (r.cost_class === "free") return 0;
  const cfg = JSON.parse(r.config || "{}");
  return cfg.usd_per_unit || 0;
}

/* ------------------------------------------------------------------ */
/* Adapters — provider-neutral contract:                                */
/* discover/health/capabilities/quota/cost/availability/execute/cancel/ */
/* receipt. Selected by resource.config.adapter (kind as fallback), so  */
/* a new substrate registers without touching the governor.             */
/* Every execute() returns { ok, output?, latency_ms, error?,          */
/* error_class? }. Classes: RATE_LIMITED | TIMEOUT | SERVER_ERROR |     */
/* BAD_RESPONSE | NETWORK | POLICY_BLOCKED | UNKNOWN                    */
/* ------------------------------------------------------------------ */
function classifyHttpError(status, errText) {
  if (status === 429) return "RATE_LIMITED";
  if (status === 401 || status === 403) return "AUTH_ERROR";
  if (status >= 500) return "SERVER_ERROR";
  if (status === 408) return "TIMEOUT";
  if (status >= 400) return "BAD_RESPONSE";
  if (/abort|timeout|timed out/i.test(errText || "")) return "TIMEOUT";
  if (/fetch failed|network|ENOTFOUND|ECONN/i.test(errText || "")) return "NETWORK";
  return "UNKNOWN";
}

async function fetchWithTimeout(url, init, timeoutMs) {
  const ctrl = new AbortController();
  const t = setTimeout(() => ctrl.abort(), timeoutMs);
  const start = Date.now();
  try {
    const resp = await fetch(url, { ...init, signal: ctrl.signal });
    return { resp, latency_ms: Date.now() - start, timedOut: false };
  } catch (e) {
    const timedOut = /abort/i.test(String(e));
    return { resp: null, latency_ms: Date.now() - start, timedOut, error: String(e).slice(0, 200) };
  } finally { clearTimeout(t); }
}

const adapters = {
  // OpenAI-compatible inference lane (keyless free tiers)
  "ai-lane": {
    async execute(resource, spec, env) {
      const cfg = JSON.parse(resource.config || "{}");
      const body = {
        model: spec.input.model || cfg.model,
        messages: spec.input.messages,
        stream: false,
        max_tokens: spec.input.max_tokens || 512,
        temperature: spec.input.temperature ?? 0.7,
      };
      const timeoutMs = Math.min(cfg.timeoutMs || 20000, spec.timeout_ms || 60000);
      const { resp, latency_ms, timedOut, error } = await fetchWithTimeout(
        cfg.baseUrl.replace(/\/+$/, "") + "/chat/completions",
        { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify(body) },
        timeoutMs);
      if (!resp) return { ok: false, latency_ms, error, error_class: timedOut ? "TIMEOUT" : "NETWORK" };
      const text = await resp.text();
      if (!resp.ok) return { ok: false, latency_ms, error: `lane HTTP ${resp.status}: ${text.slice(0, 200)}`, error_class: classifyHttpError(resp.status, text) };
      let parsed;
      try { parsed = JSON.parse(text); } catch { return { ok: false, latency_ms, error: "lane returned non-JSON", error_class: "BAD_RESPONSE" }; }
      const content = parsed.choices && parsed.choices[0] && parsed.choices[0].message && parsed.choices[0].message.content;
      if (!content) return { ok: false, latency_ms, error: "lane returned no content", error_class: "BAD_RESPONSE" };
      return { ok: true, latency_ms, output: { content, model: parsed.model || cfg.model, usage: parsed.usage || null } };
    },
    async probe(resource) {
      const r = await this.execute(resource, { input: { messages: [{ role: "user", content: "Reply with exactly: ok" }], max_tokens: 8 }, timeout_ms: 15000 }, {});
      return { ok: r.ok, latency_ms: r.latency_ms, detail: r.ok ? "probe ok" : `${r.error_class}: ${r.error}` };
    },
  },

  // Generic HTTP POST — arbitrary computation behind an HTTP endpoint.
  // config: url, method, headers, timeoutMs, expect_status[], input_body (object merged with spec.input),
  //         verify_json_path (dot path), verify_equals
  "http-post": {
    async execute(resource, spec, env) {
      const cfg = JSON.parse(resource.config || "{}");
      const timeoutMs = Math.min(cfg.timeoutMs || 15000, spec.timeout_ms || 60000);
      const body = { ...(cfg.input_body || {}), ...(spec.input || {}) };
      const { resp, latency_ms, timedOut, error } = await fetchWithTimeout(
        cfg.url, { method: cfg.method || "POST", headers: { "Content-Type": "application/json", ...(cfg.headers || {}) }, body: JSON.stringify(body) },
        timeoutMs);
      if (!resp) return { ok: false, latency_ms, error, error_class: timedOut ? "TIMEOUT" : "NETWORK" };
      const text = await resp.text();
      const expected = cfg.expect_status || [200];
      if (!expected.includes(resp.status))
        return { ok: false, latency_ms, error: `HTTP ${resp.status}: ${text.slice(0, 200)}`, error_class: classifyHttpError(resp.status, text) };
      let parsed = null;
      try { parsed = text ? JSON.parse(text) : null; } catch { /* non-JSON ok if no verify path */ }
      if (cfg.verify_json_path && parsed) {
        const val = cfg.verify_json_path.split(".").reduce((o, k) => (o == null ? o : o[k]), parsed);
        if (cfg.verify_equals !== undefined && val !== cfg.verify_equals)
          return { ok: false, latency_ms, error: `verify failed: ${cfg.verify_json_path}=${JSON.stringify(val)}`, error_class: "VERIFICATION_FAILED", output: { raw: text.slice(0, 500) } };
      }
      return { ok: true, latency_ms, output: { status: resp.status, body: parsed !== null ? parsed : text.slice(0, 2000) } };
    },
    async probe(resource) {
      const r = await this.execute(resource, { input: {}, timeout_ms: 15000 }, {});
      return { ok: r.ok, latency_ms: r.latency_ms, detail: r.ok ? "probe ok" : `${r.error_class}: ${r.error}` };
    },
  },

  // Generic HTTP GET — checks and lightweight fetches. Same verify semantics.
  "http-get": {
    async execute(resource, spec, env) {
      const cfg = JSON.parse(resource.config || "{}");
      const url = cfg.url + (spec.input && spec.input.query ? (cfg.url.includes("?") ? "&" : "?") + spec.input.query : "");
      const timeoutMs = Math.min(cfg.timeoutMs || 15000, spec.timeout_ms || 60000);
      const { resp, latency_ms, timedOut, error } = await fetchWithTimeout(
        url, { method: "GET", headers: cfg.headers || {} }, timeoutMs);
      if (!resp) return { ok: false, latency_ms, error, error_class: timedOut ? "TIMEOUT" : "NETWORK" };
      const text = await resp.text();
      const expected = cfg.expect_status || [200];
      if (!expected.includes(resp.status))
        return { ok: false, latency_ms, error: `HTTP ${resp.status}: ${text.slice(0, 200)}`, error_class: classifyHttpError(resp.status, text) };
      let parsed = null;
      try { parsed = text ? JSON.parse(text) : null; } catch { /* non-JSON ok */ }
      if (cfg.verify_json_path && parsed) {
        const val = cfg.verify_json_path.split(".").reduce((o, k) => (o == null ? o : o[k]), parsed);
        if (cfg.verify_equals !== undefined && val !== cfg.verify_equals)
          return { ok: false, latency_ms, error: `verify failed: ${cfg.verify_json_path}=${JSON.stringify(val)}`, error_class: "VERIFICATION_FAILED", output: { raw: text.slice(0, 500) } };
      }
      return { ok: true, latency_ms, output: { status: resp.status, body: parsed !== null ? parsed : text.slice(0, 2000) } };
    },
    // NOTE (dogfood 2026-10-06): probe must use this adapter's own execute.
    // Delegating to http-post's probe sent POST to GET-only endpoints and
    // wrongly failed verification (HTTP 405) on resources that execute fine.
    async probe(resource) {
      const r = await this.execute(resource, { input: {}, timeout_ms: 15000 }, {});
      return { ok: r.ok, latency_ms: r.latency_ms, detail: r.ok ? "probe ok" : `${r.error_class}: ${r.error}` };
    },
  },

  // The exchange itself — no-op adapter for orchestration-only resources.
  "self": {
    async execute() { return { ok: false, error: "self adapter does not execute jobs", error_class: "POLICY_BLOCKED" }; },
    async probe() { return { ok: true, latency_ms: 0, detail: "self" }; },
  },

  // Delegated execution — the credential lives off-Worker (e.g. vault-held).
  // The Worker NEVER executes and never sees the key: it creates a lease and
  // parks the job as deferred; a trusted claimant polls /v1/claim, executes
  // with its own credential, and reports via /v1/complete. The claimant's
  // observations (via /v1/observe on completion) are this resource's probe —
  // the cron never probes claimed adapters directly.
  "claimed": {
    claimed: true,
    async execute() { return { ok: false, error: "delegated execution — awaiting claimant", error_class: "NO_ADAPTER" }; },
    async probe() { return { ok: false, latency_ms: 0, detail: "delegated — probed via claimant observations" }; },
  },
};

function adapterFor(resource) {
  const cfg = JSON.parse(resource.config || "{}");
  return adapters[cfg.adapter || resource.kind] || null;
}

async function quotaConsume(db, resource_id) {
  const q = await db.prepare("SELECT * FROM quotas WHERE resource_id=?").bind(resource_id).first();
  if (!q || !q.limit_per_min) return;
  const prev = (await db.prepare("SELECT state FROM resources WHERE id=?").bind(resource_id).first() || {}).state || "UNKNOWN";
  const nowMs = Date.now();
  const windowStart = q.window_started_at ? Date.parse(q.window_started_at) : 0;
  let used;
  if (nowMs - windowStart > 60000) {
    used = 1;
    await db.prepare("UPDATE quotas SET used_window=1, window_started_at=?, last_checked_at=? WHERE resource_id=?")
      .bind(new Date(nowMs).toISOString(), now().replace("T", " ").slice(0, 19), resource_id).run();
  } else {
    used = (q.used_window || 0) + 1;
    await db.prepare("UPDATE quotas SET used_window=used_window+1, last_checked_at=? WHERE resource_id=?")
      .bind(now().replace("T", " ").slice(0, 19), resource_id).run();
  }
  // Exhausted → suspend until recheck (5 min). The QUOTA/INTEGRITY gates
  // exclude it until then; the cron re-probes after the suspension lifts.
  if (used >= q.limit_per_min && prev !== "EXHAUSTED") {
    const until = new Date(nowMs + 5 * 60000).toISOString();
    await db.prepare("UPDATE resources SET state='EXHAUSTED', suspended_until=? WHERE id=?").bind(until, resource_id).run();
    await db.prepare("INSERT INTO evidence_log (resource_id,from_state,to_state,trigger,detail) VALUES (?,?,?,?,?)")
      .bind(resource_id, "health:" + prev, "health:EXHAUSTED", "quota-exhausted",
        `window ${used}/${q.limit_per_min} consumed — suspended until recheck at ${until}`).run();
  }
}

/* ------------------------------------------------------------------ */
/* Job lifecycle: submit → govern (10 hard gates) → lease → revalidate */
/* → execute (attempts with failure taxonomy) → verify → receipt.      */
/* Every material decision leaves a receipt or evidence_log row.        */
/* ------------------------------------------------------------------ */
// GOVERNOR SCALABILITY: backpressure threshold per resource.
// A delegated job waits in the unclaimed queue for at most the lease pickup
// window (10 min = 5 claimant polls). One claimant drains ~25-100 jobs per
// 120s poll (NIM ~1-5s/job). 40 caps the queue at ~1 poll's drain — no job
// waits long enough for its lease to expire while queued, and no producer
// can grow the queue without bound. Rejections are 429 (retryable), never
// silent drops.
const QUEUE_FULL_THRESHOLD = 40;

async function unclaimedQueueDepth(db, resource_id, nowIso) {
  const r = await db.prepare(
    `SELECT COUNT(*) c FROM leases l JOIN jobs j ON j.id=l.job_id
     LEFT JOIN claims c ON c.lease_id=l.id
     WHERE l.resource_id=? AND l.state='active' AND l.expires_at > ?
       AND j.state='deferred' AND c.claim_id IS NULL`
  ).bind(resource_id, nowIso).first();
  return (r && r.c) || 0;
}

async function submitJob(db, spec, env) {
  const errs = validateJob(spec);
  if (errs.length) return { error: errs.join("; "), status: 400 };

  // Deduplicate on idempotency key: retries of the same execution identity
  // must not create duplicate external effects.
  if (spec.idempotency_key) {
    const dup = await db.prepare("SELECT id,state FROM jobs WHERE id=?").bind("job_" + spec.idempotency_key).first();
    if (dup) return { job_id: dup.id, state: dup.state, deduplicated: true, status: 200 };
  }

  const job_id = spec.idempotency_key ? "job_" + spec.idempotency_key : uid("job");
  const input_hash = await sha256hex(JSON.stringify(spec.input));
  await db.prepare(
    "INSERT INTO jobs (id,organelle,capability,input_hash,spec,data_class,max_cost,priority,state,defer_count) VALUES (?,?,?,?,?,?,?,?,?,?)"
  ).bind(job_id, spec.organelle || "unknown", spec.capability, input_hash,
    JSON.stringify(spec), spec.data_class || "public", spec.max_cost ?? 0, spec.priority ?? 5, "submitted", 0).run();
  return runJob(db, job_id, spec, env);
}

async function runJob(db, job_id, spec, env, opts) {
  const policy_version = await policyVersion(db);
  const g = await govern(db, spec);
  // BACKPRESSURE: the winning resource's unclaimed delegation queue is full.
  // Reject with 429 — no lease minted, no job row kept. The producer backs
  // off and retries; the queue never grows without bound. Skipped for cron
  // replans (opts.skipBackpressure): they drain the queue, not fill it.
  if (!opts?.skipBackpressure && g.selected) {
    const depth = await unclaimedQueueDepth(db, g.selected.resource_id, now());
    if (depth >= QUEUE_FULL_THRESHOLD) {
      await db.prepare("DELETE FROM jobs WHERE id=?").bind(job_id).run();
      return { error: "queue-full", resource: g.selected.resource_id,
        queue_depth: depth, threshold: QUEUE_FULL_THRESHOLD,
        retry_after_s: 60, status: 429 };
    }
  }
  const trace = g.trace;
  const input_hash = (await db.prepare("SELECT input_hash FROM jobs WHERE id=?").bind(job_id).first() || {}).input_hash;

  // No candidate survived the gates → honest terminal receipt.
  // All-retryable rejections (throttled/exhausted/stale) → deferred: the
  // cron replans when reality changes. Anything policy/capability →
  // blocked: retrying would not help.
  if (!g.selected) {
    for (const gn of ["EFFICIENCY", "LEASE"])
      if (!trace.some((x) => x.gate === gn)) trace.push({ gate: gn, pass: true, reason: "vacuous — no execution attempted", detail: null });
    // Defer when ANY rejection is retryable: a permanently-dead candidate
    // must not starve the replan path for candidates that may recover.
    // defer_count<8 (checked by the cron) bounds the loop. All-permanent →
    // blocked: retrying would not help.
    const anyRetryable = g.rejected.length > 0 && g.rejected.some((x) => x.retryable);
    const result = anyRetryable ? "deferred" : "blocked";
    const receipt_id = uid("rcpt");
    await db.prepare(
      "INSERT INTO receipts (id,job_id,provider,resource,input_hash,result,cost,policy_version,decision,fallback_chain,errors) VALUES (?,?,?,?,?,?,?,?,?,?,?)"
    ).bind(receipt_id, job_id, null, null, input_hash, result, 0, policy_version,
      JSON.stringify({ trace, rejected: g.rejected, selected: null }),
      JSON.stringify([]),
      g.rejected.map((x) => `${x.resource} [${x.gate}/${x.taxonomy || "n/a"}]: ${x.reason}`).join(" | ") || "no valid resource").run();
    await db.prepare("UPDATE jobs SET state=? WHERE id=?").bind(result, job_id).run();
    return { job_id, state: result, governor_trace: trace, rejected: g.rejected, receipt_id, status: 200 };
  }

  await db.prepare("UPDATE jobs SET state='running' WHERE id=?").bind(job_id).run();
  const execution_id = uid("exec");
  const started = now();
  const fallback_chain = [];
  let attempt_no = 0;
  let finalOk = null, finalLeaseSnap = null, finalAttemptId = null;
  let health_before = null, health_after = null, quota_before = null, quota_after = null;
  let cost_before = 0, cost_after = 0;

  for (const cand of g.ranked) {
    attempt_no++;
    const attempt_id = `${execution_id}_a${attempt_no}`;
    let resource = await db.prepare("SELECT * FROM resources WHERE id=?").bind(cand.resource_id).first();
    const provider = await db.prepare("SELECT * FROM providers WHERE id=?").bind(resource.provider_id).first();
    const adapter = adapterFor(resource);
    const q0 = await db.prepare("SELECT used_window,limit_per_min FROM quotas WHERE resource_id=?").bind(resource.id).first();
    health_before = resource.state;
    quota_before = (q0 && q0.used_window) || 0;
    cost_before = costOf(resource);

    // DELEGATED resource (credential off-Worker): do not execute inline and
    // do NOT create an inline lease/execution first — delegateExecution owns
    // the lease. (Previously this created a duplicate lease + orphaned
    // execution.) Park as deferred, let the claimant pick it up. No inline
    // fallback — double execution is worse than waiting; the cron replans
    // after the pickup window expires unclaimed.
    if (adapter.claimed) {
      const d = await delegateExecution(db, job_id, spec, resource, provider, trace);
      return { job_id, state: "deferred", lease_id: d.lease_id, resource: resource.id,
        receipt_id: d.receipt_id, governor_trace: d.trace, status: 200 };
    }

    // LEASE — reserve after gates pass, before any side effect.
    const lease = await createLease(db, job_id, resource.id, costOf(resource),
      { limit_per_min: (q0 && q0.limit_per_min) || null, used_window: quota_before }, policy_version);
    trace.push({ gate: "LEASE", pass: true, reason: `lease ${lease.lease_id} active until ${lease.expires_at}`,
      detail: { resource: resource.id, expected_cost: costOf(resource), policy_version } });
    await db.prepare("INSERT INTO executions (id,job_id,resource_id,started_at,result,attempt_no,lease_id,request_hash) VALUES (?,?,?,?,?,?,?,?)")
      .bind(attempt_id, job_id, resource.id, now(), "running", attempt_no, lease.lease_id, input_hash).run();

    // Revalidate critical invariants immediately before dispatch. The world
    // may have changed between scoring and now; a stale decision does not
    // execute. Expired/lost lease → discard and replan, never execute.
    const lv = await validateLease(db, lease.lease_id);
    resource = await db.prepare("SELECT * FROM resources WHERE id=?").bind(cand.resource_id).first(); // fresh read
    const q1 = await db.prepare("SELECT used_window,limit_per_min FROM quotas WHERE resource_id=?").bind(resource.id).first();
    let invalid = null;
    if (!lv.ok) invalid = "lease " + lv.reason;
    else if (!adapter) invalid = "adapter lost since shortlist";
    else if (!EVIDENCE_USABLE.includes(resource.evidence_state || "UNKNOWN")) invalid = `evidence changed to ${resource.evidence_state}`;
    else if (["QUARANTINED", "EXHAUSTED"].includes(resource.state)) invalid = `state changed to ${resource.state}`;
    else if (resource.state === "THROTTLED" && resource.throttled_until > now()) invalid = "throttled since lease issued";
    else if (costOf(resource) !== cost_before) invalid = `cost changed $${cost_before} → $${costOf(resource)} since lease`;
    else if (q1 && q1.limit_per_min && (q1.used_window || 0) >= q1.limit_per_min) invalid = "quota exhausted since lease issued";

    const closeAttempt = async (taxonomy, errText, error_class) => {
      await db.prepare("UPDATE executions SET result=?, error=?, error_class=?, failure_taxonomy=?, finished_at=? WHERE id=?")
        .bind("error", errText || null, error_class || null, taxonomy, now(), attempt_id).run();
    };

    if (invalid) {
      await db.prepare("UPDATE leases SET state='invalidated' WHERE id=?").bind(lease.lease_id).run();
      await closeAttempt("RETRYABLE_FAILURE", "lease invalidated — not executed: " + invalid, null);
      fallback_chain.push({ attempt: attempt_id, resource: resource.id, lease: lease.lease_id, executed: false,
        taxonomy: "RETRYABLE_FAILURE", error: "lease invalidated — not executed: " + invalid });
      trace.push({ gate: "EXECUTE", pass: false, reason: "not executed — " + invalid, detail: { resource: resource.id, attempt: attempt_id } });
      continue; // replan → next candidate
    }

    // EXECUTE
    const r = await adapter.execute(resource, spec, env);
    const taxonomy = taxonomyOf(r.error_class);
    await observe(db, resource.id, r.ok, r.latency_ms, r.ok ? "job execution" : `${r.error_class || "UNKNOWN"}: ${r.error}`);
    await quotaConsume(db, resource.id);
    await db.prepare("UPDATE leases SET state=? WHERE id=?").bind(r.ok ? "consumed" : "released", lease.lease_id).run();
    const q2 = await db.prepare("SELECT used_window FROM quotas WHERE resource_id=?").bind(resource.id).first();
    const rs2 = await db.prepare("SELECT state FROM resources WHERE id=?").bind(resource.id).first();
    quota_after = (q2 && q2.used_window) || 0;
    health_after = rs2.state;
    cost_after = costOf(resource);

    if (r.ok) {
      // VERIFY before reporting success: an execution that returns ok but
      // carries no verifiable output is a verification failure, not success.
      const content = r.output && r.output.content;
      const verified = content === undefined || (typeof content === "string" && content.length > 0);
      if (!verified) {
        await closeAttempt("VERIFICATION_FAILURE", "adapter returned ok with empty content", "VERIFICATION_FAILED");
        fallback_chain.push({ attempt: attempt_id, resource: resource.id, lease: lease.lease_id, executed: true,
          taxonomy: "VERIFICATION_FAILURE", error_class: "VERIFICATION_FAILED", error: "adapter returned ok with empty content" });
        trace.push({ gate: "EXECUTE", pass: false, reason: "VERIFICATION_FAILURE: empty content", detail: { resource: resource.id, attempt: attempt_id } });
        continue;
      }
      const output_hash = await sha256hex(JSON.stringify(r.output));
      await db.prepare("UPDATE executions SET result='ok', output_hash=?, finished_at=? WHERE id=?")
        .bind(output_hash, now(), attempt_id).run();
      trace.push({ gate: "EXECUTE", pass: true, reason: `executed on ${resource.id} (verified)`,
        detail: { resource: resource.id, attempt: attempt_id, latency_ms: r.latency_ms } });
      finalOk = { resource, provider, output: r.output, output_hash };
      finalLeaseSnap = await db.prepare("SELECT * FROM leases WHERE id=?").bind(lease.lease_id).first();
      finalAttemptId = attempt_id;
      break;
    }
    await closeAttempt(taxonomy, r.error, r.error_class || "UNKNOWN");
    fallback_chain.push({ attempt: attempt_id, resource: resource.id, lease: lease.lease_id, executed: true,
      taxonomy, error_class: r.error_class || "UNKNOWN", error: r.error });
    trace.push({ gate: "EXECUTE", pass: false, reason: `${taxonomy}: ${r.error}`, detail: { resource: resource.id, attempt: attempt_id } });
    // AUTH_FAILURE already disabled the resource via observe(); keep replanning.
  }

  const finished = now();
  if (finalOk) {
    const receipt_id = uid("rcpt");
    await db.prepare(
      `INSERT INTO receipts (id,job_id,execution_id,attempt_id,provider,resource,started_at,finished_at,
        input_hash,output_hash,result,cost,cost_before,cost_after,quota_before,quota_after,
        health_before,health_after,policy_version,lease,decision,fallback_chain,errors)
       VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)`
    ).bind(receipt_id, job_id, execution_id, finalAttemptId, finalOk.provider.id, finalOk.resource.id,
      started, finished, input_hash, finalOk.output_hash, "ok", cost_after, cost_before, cost_after,
      String(quota_before), String(quota_after), health_before, health_after, policy_version,
      JSON.stringify(finalLeaseSnap),
      JSON.stringify({ trace, rejected: g.rejected, selected: g.selected, output: finalOk.output }),
      JSON.stringify(fallback_chain),
      fallback_chain.length ? fallback_chain.map((f) => `${f.resource} [${f.taxonomy}]: ${f.error}`).join(" | ") : null).run();
    // ACCOUNTING SYMMETRY: inline executions write the same books as
    // delegated ones. "Total measured spend" must not require UNIONing
    // receipts — the accounting table is the ledger, for every execution.
    await db.prepare("INSERT INTO accounting (id,execution_id,claim_id,job_id,resource_id,measured_cost,cost_basis,quota_effect) VALUES (?,?,?,?,?,?,?,?)")
      .bind(uid("acct"), execution_id, null, job_id, finalOk.resource.id, cost_after,
        finalOk.resource.cost_basis || "unknown",
        JSON.stringify({ quota_before, quota_after })).run();
    await db.prepare("UPDATE jobs SET state='done' WHERE id=?").bind(job_id).run();
    return { job_id, state: "done", receipt_id, execution_id, resource: finalOk.resource.id,
      output: finalOk.output, governor_trace: trace, status: 200 };
  }

  // Every attempt failed. Defer when ANY failure is time-bound (retryable/
  // quota) and the defer budget remains — the cron replans; a permanently
  // dead candidate must not starve recovery for the rest. Otherwise fail
  // honestly with the full taxonomy chain; never a bare "failed".
  const taxonomies = fallback_chain.map((f) => f.taxonomy);
  const allRetryable = taxonomies.length > 0 && taxonomies.some((t) => t === "RETRYABLE_FAILURE" || t === "QUOTA_FAILURE");
  const deferCount = ((await db.prepare("SELECT defer_count FROM jobs WHERE id=?").bind(job_id).first()) || {}).defer_count || 0;
  const result = allRetryable && deferCount < 8 ? "deferred" : "failed";
  const receipt_id = uid("rcpt");
  await db.prepare(
    `INSERT INTO receipts (id,job_id,execution_id,provider,resource,started_at,finished_at,
      input_hash,result,cost,policy_version,decision,fallback_chain,errors)
     VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?)`
  ).bind(receipt_id, job_id, execution_id, null, null, started, finished, input_hash, result, 0,
    policy_version, JSON.stringify({ trace, rejected: g.rejected, selected: g.selected }),
    JSON.stringify(fallback_chain),
    fallback_chain.map((f) => `${f.resource} [${f.taxonomy}]: ${f.error}`).join(" | ") || "no candidate executed").run();
  await db.prepare("UPDATE jobs SET state=? WHERE id=?").bind(result, job_id).run();
  return { job_id, state: result, receipt_id, execution_id,
    error: fallback_chain.map((f) => `${f.resource} [${f.taxonomy}]: ${f.error}`).join(" | ") || "no candidate executed",
    governor_trace: trace, status: 200 };
}

/* ------------------------------------------------------------------ */
/* Routes                                                              */
/* ------------------------------------------------------------------ */
// Isolate-level migrate guard: schema migration runs once per isolate
// lifetime, not per request (D1 read-budget defense; deploys get fresh
// isolates so schema changes still apply).
// FIX P7 (2026-10-10, torture campaign): migrate() cold-start race. The old
// `if (!__migrated) { await migrate(db); __migrated = true; }` flipped the
// flag only AFTER migrate() resolved, so N concurrent requests on a fresh
// isolate all entered migrate() and the check-then-act ensure() ALTERs
// collided ("duplicate column name" -> 500 "exchange fault"). The guard
// assigns the migrate promise SYNCHRONOUSLY before the first await, so all
// concurrent requests share one migration. On failure the guard resets so
// the next request retries instead of sticking on a rejected promise.
let __migrating = null;
async function ensureMigrated(db) {
  if (!__migrating) {
    __migrating = migrate(db).then(
      () => true,
      (e) => { __migrating = null; throw e; }
    );
  }
  return __migrating;
}
async function handle(request, env) {
  const url = new URL(request.url);
  const path = url.pathname;
  const db = env.EXCHANGE_DB;
  await ensureMigrated(db);

  if (path === "/" && request.method === "GET")
    return json({ name: "lucidota-exchange", version: VERSION, truth: "no evidence = UNKNOWN" });

  if (path === "/v1/health" && request.method === "GET") {
    const n = await db.prepare("SELECT COUNT(*) c FROM resources").first();
    return json({ ok: true, version: VERSION, resources: (n && n.c) || 0, at: now() });
  }

  // ---- Worker Protocol v1 endpoints -------------------------------------
  // Worker-scoped auth (x-worker-id + x-worker-secret), NOT the Exchange
  // master key. A worker holding the master key could mint jobs, read all
  // receipts, and change policy — that violates the doctrine. These routes
  // sit before the Exchange-key gate on purpose.
  if (path === "/v1/workers/register" && request.method === "POST") {
    // Admin: the governor mints worker identities. Requires Exchange key.
    const a2 = authed(request, env);
    if (!a2.ok) return json({ error: a2.error }, a2.status);
    let b;
    try { b = await request.json(); } catch { return json({ error: "invalid JSON" }, 400); }
    const worker_id = String(b.worker_id || "").slice(0, 64);
    if (!/^[A-Za-z0-9_.-]+$/.test(worker_id)) return json({ error: "worker_id required (alphanumeric, _ . -)" }, 400);
    const trust_class = b.trust_class === "local" ? "local" : "remote";
    // Worker secrets need cryptographic randomness — Math.random() is not
    // acceptable for credentials. crypto.getRandomValues is available in Workers.
    const randBytes = crypto.getRandomValues(new Uint8Array(24));
    const secret = "wsec_" + Array.from(randBytes, (x) => x.toString(36)).join("").slice(0, 32);
    const exists = await db.prepare("SELECT worker_id FROM workers WHERE worker_id=?").bind(worker_id).first();
    if (exists) return json({ error: "worker_id already registered" }, 409);
    await db.prepare("INSERT INTO workers (worker_id, secret_hash, trust_class, name) VALUES (?,?,?,?)")
      .bind(worker_id, await sha256hex(secret), trust_class, String(b.name || "").slice(0, 120)).run();
    // Minting an execution identity is authority-relevant: record it where
    // auditors look. No receipt (no job), but no silent minting either.
    await db.prepare("INSERT INTO evidence_log (resource_id,from_state,to_state,trigger,detail) VALUES (?,?,?,?,?)")
      .bind("worker:" + worker_id, "nonexistent", "registered", "worker-register",
        `trust_class=${trust_class} name=${String(b.name || "").slice(0, 80)}`).run();
    return json({ worker_id, worker_secret: secret, trust_class,
      note: "secret shown once — the Exchange stores only its sha256", at: now() });
  }

  if (path === "/v1/claim" && request.method === "POST") {
    let b;
    try { b = await request.json(); } catch { return json({ error: "invalid JSON" }, 400); }
    const a = await authWorker(db, request, b);
    if (!a.ok) return json({ error: a.error }, a.status);
    if (!b.resource_id) return json({ error: "resource_id required" }, 400);
    const res = await db.prepare("SELECT * FROM resources WHERE id=?").bind(b.resource_id).first();
    if (!res) return json({ error: "unknown resource" }, 404);
    // Trust gate: capacity whose credential the worker must never see
    // requires a local-trust claimant. An untrusted remote worker cannot
    // claim it — the governor decides eligibility, not the worker.
    const need = res.claimant_trust_required || "any";
    if (need === "local" && a.worker.trust_class !== "local")
      return json({ claimed: false, error: "resource requires local-trust claimant" }, 403);
    // Oldest unclaimed delegation for this resource.
    const d = await db.prepare(
      `SELECT l.id AS lease_id, l.job_id, l.expires_at, j.spec
       FROM leases l JOIN jobs j ON j.id=l.job_id
       LEFT JOIN claims c ON c.lease_id=l.id
       WHERE l.resource_id=? AND l.state='active' AND c.claim_id IS NULL AND j.state='deferred'
         AND l.expires_at > ?
       ORDER BY l.created_at ASC LIMIT 1`).bind(b.resource_id, now()).first();
    if (!d) return json({ claimed: false, at: now() });
    // ATOMIC WIN (W1): one conditional UPDATE decides the winner. D1 is
    // single-writer — the statement claims the lease or affects 0 rows.
    // Database-level atomicity, not application-level optimism.
    const upd = await db.prepare("UPDATE leases SET state='claimed' WHERE id=? AND state='active' AND expires_at > ?")
      .bind(d.lease_id, now()).run();
    const changed = (upd.meta && upd.meta.changes) || 0;
    if (!changed) return json({ claimed: false, note: "lost race or lease not active", at: now() });
    const spec = JSON.parse(d.spec);
    const policy_version = await policyVersion(db);
    const claim_id = uid("claim");
    const input_hash = await sha256hex(JSON.stringify(spec.input));
    const policy_snapshot = { policy_version, max_cost: spec.max_cost ?? 0,
      data_class: spec.data_class || "public", spend_authority: spec.spend_authority ?? 0 };
    const receipt_requirements = ["output_hash", "model_identity", "latency_ms", "worker_outcome"];
    const execution_id = uid("exec");
    const attempt_id = `${execution_id}_a1`;
    // ATOMIC PAIR: the claim row and execution row commit together via D1
    // batch. Death between them previously left half a claim; now both land
    // or neither does. (The lease UPDATE above stays a separate conditional
    // statement — its meta.changes is the race arbiter. The residual window
    // between UPDATE and batch is covered by the orphan-lease sweeper.)
    await db.batch([
      db.prepare(`INSERT INTO claims (claim_id,job_id,worker_id,lease_id,resource_id,attempt_no,status,
          lease_expiry,policy_snapshot,receipt_requirements,idempotency_key,input_hash)
          VALUES (?,?,?,?,?,?,?,?,?,?,?,?)`)
        .bind(claim_id, d.job_id, a.worker.worker_id, d.lease_id, b.resource_id, 1, "active",
          d.expires_at, JSON.stringify(policy_snapshot), JSON.stringify(receipt_requirements),
          spec.idempotency_key || d.job_id, input_hash),
      db.prepare(`INSERT INTO executions (id,job_id,resource_id,started_at,result,attempt_no,lease_id,request_hash,claim_id)
          VALUES (?,?,?,?,?,?,?,?,?)`)
        .bind(attempt_id, d.job_id, b.resource_id, now(), "running", 1, d.lease_id, input_hash, claim_id),
    ]);
    // The delegation object. The credential NEVER travels — a local-trust
    // worker resolves credential_ref against its own vault. The worker
    // receives a delegation, not permission to become the Exchange.
    const rconf = JSON.parse(res.config || "{}");
    return json({ claimed: true,
      delegation: {
        claim_id, resource_id: b.resource_id, capability: spec.capability,
        input: spec.input, policy_snapshot, lease_expiry: d.expires_at,
        attempt: attempt_id, idempotency_key: spec.idempotency_key || d.job_id,
        receipt_requirements,
        credential_ref: rconf.credential_ref || null,
        requested_model: rconf.model || null,
      },
      lease_id: d.lease_id, job_id: d.job_id, attempt_id, at: now() });
  }

  if (path === "/v1/complete" && request.method === "POST") {
    let b;
    try { b = await request.json(); } catch { return json({ error: "invalid JSON" }, 400); }
    const a = await authWorker(db, request, b);
    if (!a.ok) return json({ error: a.error }, a.status);
    const ev = await requireEnvelope(db, "complete", b, env);
    if (!ev.ok) return json({ error: ev.error }, ev.status);
    const r = await completeClaimed(db, b, a.worker);
    return json(r, r.status || 200);
  }

  if (path === "/v1/admin/sweep-expired-claims" && request.method === "POST") {
    const a2 = authed(request, env);
    if (!a2.ok) return json({ error: a2.error }, a2.status);
    return json({ swept: await sweepExpiredClaims(db), at: now() });
  }
  // ---- end Worker Protocol v1 endpoints ---------------------------------

  const auth = authed(request, env);
  if (!auth.ok) return json({ error: auth.error }, auth.status);

  if (path === "/v1/seed" && request.method === "POST") {
    const r = await seed(db);
    return json({ ...r, at: now() });
  }

  if (path === "/v1/resources" && request.method === "GET")
    return json({ resources: await listResources(db), at: now() });

  if (path === "/v1/resources" && request.method === "POST") {
    // Register a new resource without a code deploy. Registration is a LEAD,
    // not capacity: it enters as DISCOVERED and is unusable until verified.
    let b;
    try { b = await request.json(); } catch { return json({ error: "invalid JSON" }, 400); }
    const r = await registerCandidate(db, b, "api-register");
    if (r.error) return json({ error: r.error }, 400);
    return json({ ...r, at: now(), note: "DISCOVERED lead — run POST /v1/resources/:id/verify to prove it" });
  }

  if (path === "/v1/discover" && request.method === "POST") {
    // Register a candidate lead (provider-level or resource-level).
    let b;
    try { b = await request.json(); } catch { return json({ error: "invalid JSON" }, 400); }
    const r = await registerCandidate(db, b, b.evidence_source || "manual");
    if (r.error) return json({ error: r.error }, 400);
    return json({ ...r, at: now() });
  }

  const evm = path.match(/^\/v1\/resources\/([A-Za-z0-9_.-]+)\/evidence$/);
  if (evm && request.method === "GET") {
    const r = await db.prepare("SELECT id,evidence_state,evidence_source,evidence_at,cost_basis,state FROM resources WHERE id=?").bind(evm[1]).first();
    if (!r) return json({ error: "resource not found" }, 404);
    const log = await db.prepare("SELECT at,from_state,to_state,trigger,detail FROM evidence_log WHERE resource_id=? ORDER BY id DESC LIMIT 50").bind(evm[1]).all();
    return json({ resource: r, usable: EVIDENCE_USABLE.includes(r.evidence_state), log: log.results, at: now() });
  }

  const vfm = path.match(/^\/v1\/resources\/([A-Za-z0-9_.-]+)\/verify$/);
  if (vfm && request.method === "POST") {
    // The evidence gauntlet: probe → measure economics → measure quota →
    // check recency → check sustained. Each stage must be EARNED.
    const rid = vfm[1];
    const r = await db.prepare("SELECT * FROM resources WHERE id=?").bind(rid).first();
    if (!r) return json({ error: "resource not found" }, 404);
    const chain = [];
    const step = (stage, achieved, evidence) => chain.push({ stage, achieved, evidence });
    const adapter = adapterFor(r);
    if (adapter && adapter.claimed) {
      // Delegated gauntlet: the Worker cannot probe (no credential), so
      // verification itself is delegated. A real tiny job goes through
      // claim/complete; its observations and receipts feed the normal
      // evidence ladder. No shortcuts — every tier is earned from recorded
      // evidence, exactly like Worker-executed resources.
      const caps = await db.prepare("SELECT capability FROM resource_capabilities WHERE resource_id=?").bind(rid).all();
      const cap = (caps.results || [])[0];
      if (!cap) {
        step("ACCESSIBLE", false, "no capabilities registered — nothing to verify");
        return json({ resource: rid, evidence_state: r.evidence_state, chain, usable: false, at: now() });
      }
      const job_id = uid("job");
      const vinput = cap.capability === "inference"
        ? { messages: [{ role: "user", content: "Reply with exactly: ok" }], max_tokens: 64 }
        : {};
      const vspec = { organelle: "exchange-verifier", capability: cap.capability, input: vinput, data_class: "public", max_cost: 0, priority: 9 };
      await db.prepare("INSERT INTO jobs (id,organelle,capability,input_hash,spec,data_class,max_cost,priority,state,defer_count) VALUES (?,?,?,?,?,?,?,?,?,?)")
        .bind(job_id, vspec.organelle, vspec.capability, await sha256hex(JSON.stringify(vspec.input)),
          JSON.stringify(vspec), "public", 0, 9, "submitted", 0).run();
      const provider = await db.prepare("SELECT * FROM providers WHERE id=?").bind(r.provider_id).first();
      // BACKPRESSURE (battery v3 T1): the verify gauntlet mints a delegated
      // lease directly — it must respect the same queue cap as /v1/job.
      // No lease, no queue growth; the caller retries after retry_after_s.
      const vdepth = await unclaimedQueueDepth(db, rid, now());
      if (vdepth >= QUEUE_FULL_THRESHOLD) {
        await db.prepare("DELETE FROM jobs WHERE id=?").bind(job_id).run();
        step("ACCESSIBLE", "deferred", `queue full (depth ${vdepth}) — retry after 60s`);
        return json({ resource: rid, evidence_state: r.evidence_state, chain,
          usable: false, error: "queue-full", queue_depth: vdepth,
          threshold: QUEUE_FULL_THRESHOLD, retry_after_s: 60, at: now() }, 429);
      }
      const d = await delegateExecution(db, job_id, vspec, r, provider, []);
      step("ACCESSIBLE", "pending", `delegated verification job ${job_id} — claimant executes the probe`);
      step("FREE", "pending", "cost measured when the claimant execution completes at $0");
      step("QUOTA", "pending", "headroom measured from real executions, not pricing pages");
      step("CURRENTLY AVAILABLE", "pending", "earned by fresh successful executions, not by this call");
      step("VERIFIED", "pending", "earned by sustained evidence, not by this call");
      return json({ resource: rid, evidence_state: r.evidence_state, delegated: true, job_id, lease_id: d.lease_id,
        chain, usable: false, at: now(),
        note: "gauntlet delegated to claimant; watch GET /v1/resources/:id/evidence for promotion" });
    }
    if (!adapter || !adapter.probe) {
      step("ACCESSIBLE", false, "no probeable adapter/config — remains a DISCOVERED lead, not usable capacity");
      return json({ resource: rid, evidence_state: r.evidence_state, chain, usable: false, at: now() });
    }
    // 1. ACCESSIBLE — can we actually reach it?
    let p;
    try { p = await adapter.probe(r); } catch (e) { p = { ok: false, detail: String(e).slice(0, 160) }; }
    await observe(db, rid, p.ok, p.latency_ms, "verify gauntlet: " + (p.detail || "probe"));
    step("ACCESSIBLE", !!p.ok, p.ok ? `probe ok (${p.latency_ms}ms)` : `probe failed: ${p.detail}`);
    if (!p.ok) {
      // A failed explicit re-verification does not silently clear a
      // known-invalid credential: it stays PARKED/disabled, not UNKNOWN.
      const cs = (await db.prepare("SELECT credential_state FROM resources WHERE id=?").bind(rid).first() || {}).credential_state;
      if (cs !== "invalid") await setEvidence(db, rid, "UNKNOWN", "verify-failed", p.detail, true);
      else await db.prepare("INSERT INTO evidence_log (resource_id,from_state,to_state,trigger,detail) VALUES (?,?,?,?,?)")
        .bind(rid, "PARKED", "PARKED", "verify-failed", "explicit re-verify failed — credential stays invalid/disabled").run();
      const cur2 = await db.prepare("SELECT evidence_state FROM resources WHERE id=?").bind(rid).first();
      return json({ resource: rid, evidence_state: cur2.evidence_state, chain, usable: false, at: now() });
    }
    await setEvidence(db, rid, "ACCESSIBLE", "verify-probe", `probe ok, ${p.latency_ms}ms`);
    // Explicit re-verification clears a known-invalid credential; silent
    // probes never do (see cron auto-ingest guard).
    await db.prepare("UPDATE resources SET credential_state='valid' WHERE id=?").bind(rid).run();
    // 2. FREE — the probe executed real work; marginal cost measured at $0.
    await db.prepare("UPDATE resources SET cost_basis='measured' WHERE id=?").bind(rid).run();
    await setEvidence(db, rid, "FREE", "verify-economics", "executed real work at measured $0 marginal cost");
    step("FREE", true, "real execution completed; cost measured $0 (not claimed)");
    // COLD-START BRIDGE (dogfood 2026-10-06): the gauntlet probe IS a real
    // execution — real request, real response, content verified at measured
    // $0, under explicit bearer-key authority. warrantedTier requires
    // execOk>=1 for AVAILABLE, but the EVIDENCE gate only lets
    // AVAILABLE/VERIFIED execute — without recording this proven execution,
    // a brand-new resource could never leave QUOTA. Recording it is honest
    // accounting of work that genuinely happened, not gate-loosening.
    const gauntletJobId = "verify_" + rid + "_" + Date.now().toString(36);
    await db.prepare("INSERT INTO jobs (id,organelle,capability,input_hash,spec,data_class,max_cost,priority,state) VALUES (?,?,?,?,?,?,?,?,?)")
      .bind(gauntletJobId, "verify-gauntlet", "probe", await sha256hex("verify-gauntlet:" + rid),
        JSON.stringify({ verify: rid, probe: p.detail || "ok" }), "public", 0, 5, "done").run();
    await db.prepare("INSERT INTO executions (id,job_id,resource_id,started_at,finished_at,result,attempt_no,request_hash) VALUES (?,?,?,?,?,?,?,?)")
      .bind(uid("exec") + "_verify", gauntletJobId, rid, now(), now(), "ok", 0,
        await sha256hex("verify-gauntlet:" + rid + ":" + (p.detail || ""))).run();
    // 3. QUOTA — headroom actually measured, not read off a pricing page.
    const q = await db.prepare("SELECT * FROM quotas WHERE resource_id=?").bind(rid).first();
    if (q && q.limit_per_min) {
      const usedRow = await db.prepare("SELECT COUNT(*) c FROM executions WHERE resource_id=? AND result='ok' AND started_at>?").bind(rid, minutesAgo(1)).first();
      const used = (usedRow && usedRow.c) || 0;
      const headroom = q.limit_per_min - used;
      if (headroom > 0) {
        await db.prepare("UPDATE quotas SET measured=1, measured_at=?, used_window=? WHERE resource_id=?").bind(now(), used, rid).run();
        await setEvidence(db, rid, "QUOTA", "verify-quota", `headroom measured: ${headroom}/${q.limit_per_min} per min`);
        step("QUOTA", true, `measured headroom ${headroom}/${q.limit_per_min} per min`);
      } else step("QUOTA", false, `no headroom right now (${used}/${q.limit_per_min} used)`);
    } else step("QUOTA", false, "no quota row — limit unknown, cannot claim headroom");
    // 4-5. CURRENTLY AVAILABLE → VERIFIED, from recorded history only.
    const w = await warrantedTier(db, rid);
    if (w === "AVAILABLE" || w === "VERIFIED")
      await setEvidence(db, rid, w, "verify-sustained", "recent successful executions on record");
    step("CURRENTLY AVAILABLE", w === "AVAILABLE" || w === "VERIFIED",
      (w === "AVAILABLE" || w === "VERIFIED") ? "fresh successful executions on record" : "no fresh successful execution — not currently available");
    step("VERIFIED", w === "VERIFIED",
      w === "VERIFIED" ? "sustained: 3+ successes/7d, fresh probe, healthy" : "sustained evidence not yet on record");
    const cur = await db.prepare("SELECT evidence_state FROM resources WHERE id=?").bind(rid).first();
    return json({ resource: rid, evidence_state: cur.evidence_state, chain, usable: EVIDENCE_USABLE.includes(cur.evidence_state), at: now() });
  }

  if (path === "/v1/capacity" && request.method === "GET") {
    // The honest number: legitimate $0 compute the exchange can actually
    // command right now. Listings, trials, and marketing claims excluded.
    const rs = await db.prepare("SELECT * FROM resources WHERE evidence_state IN ('AVAILABLE','VERIFIED')").all();
    const items = [];
    let measuredHeadroom = 0, assertedHeadroom = 0;
    for (const r of (rs.results || [])) {
      const q = await db.prepare("SELECT * FROM quotas WHERE resource_id=?").bind(r.id).first();
      const ok24 = (await db.prepare("SELECT COUNT(*) c FROM health_observations WHERE resource_id=? AND ok=1 AND observed_at>?").bind(r.id, minutesAgo(24 * 60)).first() || {}).c || 0;
      const headroom = q && q.limit_per_min ? Math.max(0, q.limit_per_min - (q.used_window || 0)) : null;
      if (headroom !== null) { if (q.measured) measuredHeadroom += headroom; else assertedHeadroom += headroom; }
      items.push({
        id: r.id, kind: r.kind, evidence_state: r.evidence_state,
        cost_class: r.cost_class, cost_basis: r.cost_basis || "unknown",
        quota: q ? { limit_per_min: q.limit_per_min, used_window: q.used_window || 0, headroom, basis: q.measured ? "measured" : "asserted" } : null,
        success_24h: ok24,
      });
    }
    return json({
      at: now(), usable_resources: items.length,
      measured_headroom_per_min: measuredHeadroom, asserted_headroom_per_min: assertedHeadroom,
      items,
      note: "Only AVAILABLE/VERIFIED resources count. A provider listing is not capacity.",
    });
  }

  // ---- VOTING (protocol amendments A1/A2/A3, Zackary approved 2026-10-07) ----
  // Voting is a governor composition pattern, not a worker behavior. Each
  // vote is one ordinary delegated job; the quorum tally is a deterministic
  // reduction over N receipts. Workers never know they are voting.
  async function getBallotPolicy(db) {
    const p = await db.prepare("SELECT * FROM ballot_policy WHERE id='default'").first();
    return { voters: JSON.parse(p.voters), quorum_rule: p.quorum_rule,
      deadline_s: p.deadline_s, tie_policy: p.tie_policy };
  }
  function ballotPrompt(btype, proposition) {
    if (btype === "verdict") return `${proposition}\n\nEvaluate the proposition above. Think step by step, then give your\nfinal verdict on its own line at the very end, exactly one word:\nAUTHORIZED or UNKNOWN or DENIED.\n\nFinal verdict:`;
    return `${proposition}\n\nAnswer with the final answer only, no explanation:`;
  }
  function modelSlug(m) { return m.replace(/[^a-zA-Z0-9]/g, "_").slice(0, 40); }
  // Mechanical vote extraction — deterministic from receipts alone. No LLM judging.
  function extractVote(btype, output) {
    const content = (output && (output.content || output.reasoning_content)) || "";
    if (btype === "verdict") {
      const tail = String(content).slice(-300);
      const all = tail.match(/\b(AUTHORIZED|UNKNOWN|DENIED)\b/g);
      return all ? all[all.length - 1] : "INVALID";
    }
    const norm = String(content).trim().toLowerCase().replace(/\s+/g, " ").replace(/[.,;:!?]+$/, "");
    return norm || "INVALID";
  }
  function verdictOfVote(btype, vote) {
    if (btype !== "verdict") return null;
    return vote === "AUTHORIZED" ? 1 : vote === "DENIED" ? -1 : vote === "UNKNOWN" ? 0 : null;
  }
  async function tallyBallot(db, ballot) {
    const voters = JSON.parse(ballot.voters);
    // One vote per job: latest receipt per job_id (jobs write a "deferred"
    // receipt then a terminal one; only the terminal vote counts).
    const receipts = await db.prepare(
      `SELECT r.id, r.job_id, r.decision, r.worker_outcome FROM receipts r
       JOIN (SELECT job_id, MAX(created_at) AS mc FROM receipts
             WHERE ballot_id=? AND job_id NOT LIKE 'ballot:%' GROUP BY job_id) latest
         ON r.job_id = latest.job_id AND r.created_at = latest.mc
       WHERE r.ballot_id=? AND r.job_id NOT LIKE 'ballot:%'`).bind(ballot.id, ballot.id).all();
    const votes = [];
    for (const r of (receipts.results || [])) {
      let dec = null;
      try { dec = JSON.parse(r.decision || "{}"); } catch {}
      const out = dec && dec.output;
      const vote = extractVote(ballot.ballot_type, out);
      votes.push({ receipt_id: r.id, job_id: r.job_id,
        model: (dec && dec.vote_model) || "unknown",
        vote, valid: vote !== "INVALID", worker_outcome: r.worker_outcome });
    }
    const valid = votes.filter(v => v.valid);
    const counts = {};
    for (const v of valid) counts[v.vote] = (counts[v.vote] || 0) + 1;
    const quorum = Math.floor(voters.length / 2) + 1;
    const nowMs = Date.now();
    const expired = nowMs > Date.parse(ballot.deadline_at);
    let state = ballot.state, verdict = ballot.verdict;
    if (state === "open") {
      const sorted = Object.entries(counts).sort((a, b) => b[1] - a[1]);
      const top = sorted[0];
      if (top && top[1] > valid.length / 2 && valid.length >= quorum) {
        state = "decided"; verdict = top[0];
      } else if (expired) {
        state = valid.length >= quorum ? "inconclusive" : "expired";
        verdict = state === "inconclusive" ? "UNKNOWN" : null;
      } else if (valid.length >= quorum && sorted.length > 1 && sorted[0][1] === sorted[1][1]) {
        // quorum reached but tied and deadline not yet passed — keep waiting;
        // late votes may break the tie. Only INCONCLUSIVE at deadline.
      }
    }
    const tally = { votes, counts, valid: valid.length, quorum,
      voters: voters.length, expired, correlation_domain: ["nvidia-nim"] };
    if (state !== ballot.state) {
      await db.prepare("UPDATE ballots SET state=?, verdict=?, tally=? WHERE id=?")
        .bind(state, verdict, JSON.stringify(tally), ballot.id).run();
      await db.prepare(
        `INSERT INTO receipts (id,job_id,result,policy_version,decision,errors,ballot_id) VALUES (?,?,?,?,?,?,?)`
      ).bind(uid("rcpt"), "ballot:" + ballot.id, state, await policyVersion(db),
        JSON.stringify({ ballot: ballot.id, ballot_type: ballot.ballot_type,
          proposition: ballot.proposition, tally, verdict }),
        `ballot ${ballot.id}: ${state}${verdict ? " — " + verdict : ""}`, ballot.id).run();
    }
    return { ballot_id: ballot.id, ballot_type: ballot.ballot_type,
      proposition: ballot.proposition, state, verdict, tally };
  }

  if (path === "/v1/ballots" && request.method === "POST") {
    // A1: submit a ballot. Fans out one ordinary delegated job per voter.
    // All-or-nothing under backpressure: if N voters don't fit, 429.
    let b;
    try { b = await request.json(); } catch { return json({ error: "invalid JSON" }, 400); }
    const evb = await requireEnvelope(db, "ballots", b, env);
    if (!evb.ok) return json({ error: evb.error }, evb.status);
    const btype = b.ballot_type;
    if (!["verdict", "answer"].includes(btype))
      return json({ error: "ballot_type must be verdict|answer" }, 400);
    if (!b.proposition || typeof b.proposition !== "string" || b.proposition.length > 4000)
      return json({ error: "proposition required (max 4000 chars)" }, 400);
    const pol = await getBallotPolicy(db);
    const rawVoters = b.voters == null ? pol.voters : b.voters;
    if (!Array.isArray(rawVoters) || rawVoters.length < 2 || rawVoters.length > 7)
      return json({ error: "voters: array of 2-7 model ids" }, 400);
    const seen = new Set();
    const voters = [];
    for (const v of rawVoters) {
      if (typeof v !== "string" || !v.trim() || v.length > 120)
        return json({ error: "each voter must be a non-empty model id string (max 120 chars)" }, 400);
      const slug = modelSlug(v);
      if (seen.has(slug))
        return json({ error: "duplicate voter (by model slug): " + v }, 400);
      seen.add(slug); voters.push(v);
    }
    if (voters.length < 2)
      return json({ error: "at least 2 distinct voters required" }, 400);
    // Backpressure: all N votes must fit under the cap.
    const depth = await unclaimedQueueDepth(db, "nim-nvidia", now());
    if (depth + voters.length > QUEUE_FULL_THRESHOLD)
      return json({ error: "queue-full", queue_depth: depth,
        needed: voters.length, threshold: QUEUE_FULL_THRESHOLD,
        retry_after_s: 60, status: 429 }, 429);
    const ballot_id = uid("bal");
    const quorum = Math.floor(voters.length / 2) + 1;
    const deadline_s = b.deadline_s == null ? pol.deadline_s : b.deadline_s;
    if (typeof deadline_s !== "number" || !Number.isFinite(deadline_s) || deadline_s <= 0)
      return json({ error: "deadline_s must be a finite positive number of seconds" }, 400);
    if (deadline_s > 30 * 86400)
      return json({ error: "deadline_s exceeds 30-day maximum" }, 400);
    const deadline_at = new Date(Date.now() + deadline_s * 1000).toISOString();
    await db.prepare(
      "INSERT INTO ballots (id,ballot_type,proposition,voters,quorum,deadline_at) VALUES (?,?,?,?,?,?)"
    ).bind(ballot_id, btype, b.proposition, JSON.stringify(voters), quorum, deadline_at).run();
    const jobs = [];
    for (const model of voters) {
      const prompt = ballotPrompt(btype, b.proposition);
      const vspec = { organelle: "ballot-voter", capability: "inference",
        input: { model, messages: [{ role: "user", content: prompt }],
          temperature: 0, max_tokens: 256, ballot_type: btype },
        ballot_id, // A2: top-level so complete() stamps receipts.ballot_id
        data_class: "public", max_cost: 0, priority: 6,
        idempotency_key: `ballot_${ballot_id}_${modelSlug(model)}` };
      const job_id = "job_" + vspec.idempotency_key;
      const input_hash = await sha256hex(JSON.stringify(vspec.input));
      await db.prepare(
        "INSERT INTO jobs (id,organelle,capability,input_hash,spec,data_class,max_cost,priority,state,defer_count) VALUES (?,?,?,?,?,?,?,?,?,?)"
      ).bind(job_id, vspec.organelle, vspec.capability, input_hash, JSON.stringify(vspec),
        "public", 0, 6, "submitted", 0).run();
      const rr = await runJob(db, job_id, vspec, env);
      jobs.push({ model, job_id, state: rr.state || rr.error });
    }
    await db.prepare("INSERT INTO evidence_log (resource_id,from_state,to_state,trigger,detail) VALUES (?,?,?,?,?)")
      .bind("ballot:" + ballot_id, "none", "open", "ballot-submit",
        `${btype} ballot, ${voters.length} voters, quorum ${quorum}`).run();
    return json({ ballot_id, ballot_type: btype, state: "open",
      voters, quorum, deadline_at, jobs, at: now() });
  }

  const bm = path.match(/^\/v1\/ballots\/([A-Za-z0-9_.-]+)$/);
  if (bm && request.method === "GET") {
    // A1: get ballot + deterministic tally.
    const ballot = await db.prepare("SELECT * FROM ballots WHERE id=?").bind(bm[1]).first();
    if (!ballot) return json({ error: "ballot not found" }, 404);
    return json({ ...(await tallyBallot(db, ballot)), at: now() });
  }

  if (path === "/v1/ballot-policy" && request.method === "GET") {
    // A3: governor-side quorum policy object.
    return json({ policy: await getBallotPolicy(db), at: now() });
  }

  if (path === "/v1/job" && request.method === "POST") {
    let spec;
    try { spec = await request.json(); } catch { return json({ error: "invalid JSON" }, 400); }
    const r = await submitJob(db, spec, env);
    return json(r, r.status || 200);
  }

  const jm = path.match(/^\/v1\/job\/([A-Za-z0-9_.-]+)$/);
  if (jm && request.method === "GET") {
    const job = await db.prepare("SELECT * FROM jobs WHERE id=?").bind(jm[1]).first();
    if (!job) return json({ error: "job not found" }, 404);
    const execs = await db.prepare("SELECT * FROM executions WHERE job_id=? ORDER BY attempt_no").bind(jm[1]).all();
    const leases = await db.prepare("SELECT * FROM leases WHERE job_id=?").bind(jm[1]).all();
    const rcpts = await db.prepare("SELECT * FROM receipts WHERE job_id=?").bind(jm[1]).all();
    const claims = await db.prepare("SELECT claim_id,job_id,worker_id,lease_id,resource_id,attempt_no,status,claimed_at,lease_expiry,worker_outcome,completed_at FROM claims WHERE job_id=?").bind(jm[1]).all();
    const accts = await db.prepare("SELECT * FROM accounting WHERE job_id=?").bind(jm[1]).all();
    return json({ job, executions: execs.results, attempts: execs.results, leases: leases.results,
      claims: claims.results, receipts: rcpts.results, accounting: accts.results });
  }

  if (path === "/v1/jobs" && request.method === "GET") {
    // Deferred-queue visibility: which jobs are waiting on reality to change.
    const state = url.searchParams.get("state");
    const q = state
      ? "SELECT id,organelle,capability,data_class,state,defer_count,created_at FROM jobs WHERE state=? ORDER BY created_at DESC LIMIT 50"
      : "SELECT id,organelle,capability,data_class,state,defer_count,created_at FROM jobs ORDER BY created_at DESC LIMIT 50";
    const r = state ? await db.prepare(q).bind(state).all() : await db.prepare(q).all();
    return json({ jobs: r.results, at: now() });
  }

  if (path === "/v1/receipts" && request.method === "GET") {
    const job = url.searchParams.get("job");
    const q = job ? "SELECT * FROM receipts WHERE job_id=? ORDER BY created_at DESC LIMIT 50"
                  : "SELECT * FROM receipts ORDER BY created_at DESC LIMIT 50";
    const r = job ? await db.prepare(q).bind(job).all() : await db.prepare(q).all();
    return json({ receipts: r.results });
  }

  const vm = path.match(/^\/v1\/receipts\/([A-Za-z0-9_.-]+)\/verify$/);
  if (vm && request.method === "GET") {
    // Independent verification: recompute hashes, check trace structure.
    const rcpt = await db.prepare("SELECT * FROM receipts WHERE id=?").bind(vm[1]).first();
    if (!rcpt) return json({ error: "receipt not found" }, 404);
    const job = await db.prepare("SELECT * FROM jobs WHERE id=?").bind(rcpt.job_id).first();
    const checks = [];
    if (job) {
      // input_hash was computed over the raw input object; verify against stored spec
      const spec = JSON.parse(job.spec || "{}");
      const expect = await sha256hex(JSON.stringify(spec.input));
      checks.push({ check: "input_hash", pass: expect === rcpt.input_hash, expected: expect.slice(0, 16) + "…", actual: (rcpt.input_hash || "").slice(0, 16) + "…" });
    } else checks.push({ check: "input_hash", pass: false, reason: "job row missing" });
    // Output integrity: if the receipt binds an output, the recorded
    // output_hash must recompute from it. Trust the hash, not the prose.
    try {
      const dec0 = JSON.parse(rcpt.decision || "{}");
      const out0 = dec0.output;
      if (out0 != null && rcpt.output_hash) {
        const rehash = await sha256hex(JSON.stringify(out0));
        checks.push({ check: "output_hash", pass: rehash === rcpt.output_hash,
          expected: rcpt.output_hash.slice(0, 16) + "…", actual: rehash.slice(0, 16) + "…" });
      }
    } catch { checks.push({ check: "output_hash", pass: false, reason: "decision unparsable" }); }
    let decision = null, traceOk = false;
    try { decision = JSON.parse(rcpt.decision || "{}"); traceOk = Array.isArray(decision.trace) && decision.trace.length > 0; }
    catch { /* keep false */ }
    checks.push({ check: "governor_trace_present", pass: traceOk });
    // Delegated completions: the 10 governor gates live in the job's
    // delegation receipt (authorization), while this receipt carries the
    // completion trace (CLAIM/VERIFY/ACCOUNT). Verify across the chain —
    // a receipt must let another process reconstruct the whole decision.
    let authTrace = decision && decision.trace ? decision.trace : [];
    let isDelegatedCompletion = !!(decision && decision.delegated && rcpt.result === "ok");
    if (isDelegatedCompletion) {
      const siblings = await db.prepare("SELECT id,decision FROM receipts WHERE job_id=? AND id!=?").bind(rcpt.job_id, rcpt.id).all();
      for (const s of (siblings.results || [])) {
        try {
          const sd = JSON.parse(s.decision || "{}");
          if (sd.delegated && Array.isArray(sd.trace) && sd.trace.some((g) => g.gate === "LEASE")) { authTrace = sd.trace; break; }
        } catch { /* keep looking */ }
      }
      checks.push({ check: "delegation_chain_resolved", pass: authTrace !== (decision.trace || []), note: "governor trace from delegation receipt" });
      // Claim binding: the receipt's claim must exist, belong to the recorded
      // worker, and be completed. A receipt pointing at a phantom claim proves nothing.
      const cl = rcpt.claim_id ? await db.prepare("SELECT worker_id,status FROM claims WHERE claim_id=?").bind(rcpt.claim_id).first() : null;
      const decW = decision.worker;
      checks.push({ check: "claim_binding", pass: !!(cl && cl.status === "completed" && (!decW || cl.worker_id === decW)),
        claim: rcpt.claim_id, worker: cl && cl.worker_id });
      // Accounting binding: the governor's books must have a row for this claim.
      // The receipt's cost claim is "trust me" without it.
      const ac = rcpt.claim_id ? await db.prepare("SELECT measured_cost,cost_basis FROM accounting WHERE claim_id=?").bind(rcpt.claim_id).first() : null;
      checks.push({ check: "accounting_bound", pass: !!ac && (ac.measured_cost || 0) === (rcpt.cost || 0),
        measured_cost: ac && ac.measured_cost, cost_basis: ac && ac.cost_basis });
    }
    const gates = ["IDENTITY", "AUTHORITY", "DATA", "CAPABILITY", "EVIDENCE", "QUOTA", "COST", "INTEGRITY", "LEASE", "EFFICIENCY"];
    const present = authTrace.map((g) => g.gate);
    checks.push({ check: "all_gates_recorded", pass: gates.every((x) => present.includes(x)), missing: gates.filter((x) => !present.includes(x)) });
    checks.push({ check: "cost_within_policy", pass: (rcpt.cost || 0) <= (job ? JSON.parse(job.spec || "{}").max_cost ?? 0 : 0), cost: rcpt.cost });
    checks.push({ check: "policy_version_recorded", pass: !!rcpt.policy_version, policy_version: rcpt.policy_version || null });
    let fbOk = true;
    try { fbOk = rcpt.fallback_chain == null || Array.isArray(JSON.parse(rcpt.fallback_chain)); } catch { fbOk = false; }
    checks.push({ check: "fallback_chain_structured", pass: fbOk });
    if (rcpt.result === "ok") {
      let leaseOk = false;
      try { const l = JSON.parse(rcpt.lease || "null"); leaseOk = !!(l && l.id && l.expires_at); } catch { leaseOk = false; }
      checks.push({ check: "lease_snapshot_present", pass: leaseOk });
      checks.push({ check: "attempt_id_present", pass: !!rcpt.attempt_id });
      // Success without verification evidence is not reported: the trace
      // must contain a passing EXECUTE gate for the recorded attempt.
      // Delegated completions verify via their VERIFY gate instead.
      const execPass = isDelegatedCompletion
        ? Array.isArray(decision.trace) && decision.trace.some((g) => g.gate === "VERIFY" && g.pass)
        : present.includes("EXECUTE") && Array.isArray(decision.trace)
          && decision.trace.some((g) => g.gate === "EXECUTE" && g.pass);
      checks.push({ check: "execution_verified", pass: execPass });
    }
    const pass = checks.every((c) => c.pass);
    return json({ receipt_id: rcpt.id, job_id: rcpt.job_id, verified: pass, checks, at: now() });
  }

  if (path === "/v1/observe" && request.method === "POST") {
    let b;
    try { b = await request.json(); } catch { return json({ error: "invalid JSON" }, 400); }
    const evo = await requireEnvelope(db, "observe", b, env);
    if (!evo.ok) return json({ error: evo.error }, evo.status);
    if (!b.resource_id) return json({ error: "resource_id required" }, 400);
    return json(await observe(db, b.resource_id, !!b.ok, b.latency_ms || null, b.detail || null));
  }

  return json({ error: "not found" }, 404);
}

/* ------------------------------------------------------------------ */
export default {
  async fetch(request, env) {
    try { return await handle(request, env); }
    catch (e) { return json({ error: "exchange fault: " + String(e).slice(0, 300) }, 500); }
  },  async scheduled(event, env) {
    // REACTION LOOP: OBSERVE → CLASSIFY → UPDATE → REPLAN → (EXECUTE) →
    // VERIFY → LEARN → OBSERVE. Runs every 30 min (*/30 — burn-safe;
    // the 5-min loop was throttled 2026-10-09 after 5.3M D1 reads/day vs
    // the 5M free quota), no human prompt.
    // - tier-based probing (usable = tight, leads = slow, parked = daily)
    // - THROTTLED recheck after backoff lifts; EXHAUSTED recheck after suspension
    // - credential-invalid stays disabled until explicit re-verify
    // - provider-down detection → mark DOWN, force UNKNOWN, replan elsewhere
    // - deferred jobs replan against current reality (bounded)
    // - stale demotion, billing detection, discovery ingestion
    const db = env.EXCHANGE_DB;
    await ensureMigrated(db); // D1 read-budget defense
    const nowIso = now();
    const rs = await db.prepare("SELECT * FROM resources").all();
    const out = [];
    // v0.6.15 (d1-burn-codefixes): warrantedTierInfo's window computation is
    // memoized per resource per cron run — every consumer in this pass
    // (claimed ladder, probe path) shares one computation and it is never
    // recomputed for the same resource. Fresh-enough resources never reach
    // it: their staleness demotion needs only the cheap indexed ok24 COUNT
    // inside demoteCheck, never the full window.
    const tierInfoMemo = new Map();
    const tierInfoFor = async (rid) => {
      let ti = tierInfoMemo.get(rid);
      if (!ti) { ti = await warrantedTierInfo(db, rid); tierInfoMemo.set(rid, ti); }
      return ti;
    };
    for (const r of (rs.results || [])) {
      const cfg = JSON.parse(r.config || "{}");
      const adapterName = cfg.adapter || r.kind;
      if (adapterName === "self") { const ev = await autoPromote(db, r.id); out.push({ resource: r.id, self: true, ev }); continue; }
      // Claimed resources are probed by their claimant's observations, never
      // by the Worker (it holds no credential). Staleness demotion still applies.
      if (adapterName === "claimed") {
        const tierInfo = await tierInfoFor(r.id); // wave-29: one window computation shared below (v0.6.15: memoized per cron run)
        const prom = await autoPromote(db, r.id, tierInfo); // claimant observations + receipts feed the ladder
        const dev = await demoteCheck(db, r.id, tierInfo);
        out.push({ resource: r.id, skipped: "claimed — claimant observes", ev: dev || prom });
        continue;
      }
      const last = await db.prepare(
        "SELECT observed_at FROM health_observations WHERE resource_id=? ORDER BY observed_at DESC LIMIT 1"
      ).bind(r.id).first();
      const ageMin = last ? (Date.now() - ts(last.observed_at)) / 60000 : 9999;
      const ev = r.evidence_state || "UNKNOWN";
      const throttleLifted = r.state === "THROTTLED" && (!r.throttled_until || r.throttled_until <= nowIso);
      const exhaustLifted = r.state === "EXHAUSTED" && (!r.suspended_until || r.suspended_until <= nowIso);
      const needsProbe = r.state === "UNKNOWN" || r.state === "DEGRADED" || r.state === "QUARANTINED"
        || throttleLifted || exhaustLifted || ageMin > staleAfterEv(ev);
      if (!needsProbe) {
        const dev = await demoteCheck(db, r.id); // v0.6.15: fresh path deliberately skips warrantedTierInfo — staleness demotion needs only the cheap indexed ok24 COUNT inside demoteCheck
        out.push({ resource: r.id, skipped: "fresh", ev: dev || ev, state: r.state });
        continue;
      }
      try {
        const adapter = adapterFor(r);
        if (!adapter || !adapter.probe) { out.push({ resource: r.id, skipped: "not probeable", ev }); continue; }
        const p = await adapter.probe(r);
        const o = await observe(db, r.id, p.ok, p.latency_ms, p.detail);
        let tierInfo = null; // wave-29: computed lazily below, shared by autoPromote + demoteCheck
        if (p.ok) {
          // Ingest automatically — unless the credential is known-invalid,
          // which requires explicit re-verification, not a silent probe.
          const cs = (await db.prepare("SELECT credential_state FROM resources WHERE id=?").bind(r.id).first() || {}).credential_state;
          if ((ev === "DISCOVERED" || ev === "UNKNOWN" || ev === "PARKED") && cs !== "invalid")
            await setEvidence(db, r.id, "ACCESSIBLE", "cron-probe", `probe ok ${p.latency_ms}ms — auto-ingested`);
          tierInfo = await tierInfoFor(r.id); // v0.6.15: memoized per cron run
          await autoPromote(db, r.id, tierInfo);
        }
        await demoteCheck(db, r.id, tierInfo);
        const cur = await db.prepare("SELECT evidence_state,state FROM resources WHERE id=?").bind(r.id).first();
        out.push({ resource: r.id, ok: p.ok, latency_ms: p.latency_ms, state: cur.state, ev: cur.evidence_state });
      } catch (e) { out.push({ resource: r.id, ok: false, error: String(e).slice(0, 120) }); }
    }
    const providers = await providerDownCheck(db);
    // REAPER: jobs stuck in 'running' with no receipt are orphans of a
    // faulted run (a request cannot outlive the worker). Release their
    // leases and defer them for replan — never leave a job in limbo.
    const reaped = [];
    const stale = await db.prepare(
      "SELECT * FROM jobs WHERE state='running' AND created_at < datetime('now','-10 minutes') LIMIT 5"
    ).all();
    for (const j of (stale.results || [])) {
      const rc = await db.prepare("SELECT COUNT(*) c FROM receipts WHERE job_id=?").bind(j.id).first();
      if ((rc && rc.c || 0) === 0) {
        await db.prepare("UPDATE leases SET state='released' WHERE job_id=? AND state='active'").bind(j.id).run();
        await db.prepare("UPDATE jobs SET state='deferred' WHERE id=?").bind(j.id).run();
        await db.prepare("INSERT INTO evidence_log (resource_id,from_state,to_state,trigger,detail) VALUES (?,?,?,?,?)")
          .bind("job:" + j.id, "running", "deferred", "orphan-reap", "no receipt 10m after submit — faulted run, replan").run();
        reaped.push(j.id);
      }
    }
    // ZOMBIE SWEEP (W5/W10): claims that outran their leases with no
    // completion. Lease → TIMEOUT → REPLAN. The execution is marked
    // UNCERTAIN — the provider may have executed, so the Exchange
    // preserves the uncertainty instead of pretending retries are free.
    // No permanently stuck work.
    const zombies = await sweepExpiredClaims(db);
    if (zombies.length) out.push({ zombie_sweep: zombies });
    // REPLAN: deferred jobs (all failures were time-bound) get re-evaluated
    // against current reality. Bounded: 2 per run, 8 defers max per job.
    const replanned = [];
    const dq = await db.prepare(
      "SELECT * FROM jobs WHERE state='deferred' AND COALESCE(defer_count,0) < 8 ORDER BY created_at ASC LIMIT 2"
    ).all();
    for (const j of (dq.results || [])) {
      // Never replan a live delegation: an active unexpired lease — claimed
      // or still awaiting pickup — means a worker may own this job.
      // Replanning would double-execute.
      // TEMPORAL FIX (audit 2026-10-06): expires_at is ISO ("T") but
      // datetime('now') is space-separated; bind ISO now so same-day
      // expiries compare correctly (an expired lease must not look live).
      const live = await db.prepare(
        `SELECT COUNT(*) c FROM leases l LEFT JOIN claims c ON c.lease_id=l.id
         WHERE l.job_id=? AND l.state IN ('active','claimed') AND l.expires_at > ?
         AND (c.claim_id IS NULL OR c.status IN ('active','completing'))`).bind(j.id, nowIso).first();
      if ((live && live.c || 0) > 0) {
        replanned.push({ job: j.id, state: "deferred", note: "delegation live — awaiting claimant, not replanned" });
        continue;
      }
      await db.prepare("UPDATE jobs SET defer_count=COALESCE(defer_count,0)+1, state='running' WHERE id=?").bind(j.id).run();
      try {
        const rr = await runJob(db, j.id, JSON.parse(j.spec), env, { skipBackpressure: true });
        replanned.push({ job: j.id, state: rr.state });
      } catch (e) {
        await db.prepare("UPDATE jobs SET state='deferred' WHERE id=?").bind(j.id).run();
        const errText = String(e).slice(0, 300);
        await db.prepare(
          "INSERT INTO receipts (id,job_id,result,policy_version,decision,errors) VALUES (?,?,?,?,?,?)"
        ).bind(uid("rcpt"), j.id, "deferred", await policyVersion(db),
          JSON.stringify({ replan: "failed", trigger: "cron" }), errText).run();
        replanned.push({ job: j.id, state: "deferred", error: errText.slice(0, 120) });
      }
    }
    const billing = await billingCheck(db);
    const pruned = await pruneHealthObservations(db); // wave-29: bounded retention for health_observations
    const sealsPruned = await pruneSealRegistry(db); // hardening 2026-10-10: bounded retention for seal_registry
    const discovery = await runDiscovery(db);
    console.log("exchange cron:", JSON.stringify({ probes: out, providers, reaped, replanned, billing, pruned, sealsPruned, discovery }));
  },
};
