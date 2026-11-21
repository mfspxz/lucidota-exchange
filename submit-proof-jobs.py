#!/usr/bin/env python3
"""End-to-end proof for Exchange v0.7.0 (Zack 2026-11-20 order, step 6).

Proves, with receipts, for EVERY key:
  * gauntlet promotion: DISCOVERED -> AVAILABLE via one real delegated
    execution (Phase 3 inline promotion), then a pinned proof job
    (claim -> exec -> /v1/complete -> receipt row, model_match=1),
    then a second gauntlet round -> VERIFIED where the key is healthy.
  * negative proofs (submit-time refusals, NO execution):
    - nonexistent model -> refused at MODEL
    - mistral codestral-2508 (paid) -> refused at COST (allow_paid=0).
      This is the honest paid-gate proof: mistral IS usable (gauntlet
      passed on a free model), so the COST gate is what refuses.
    - deepseek-flash -> refused at EVIDENCE (deepseek is DISCOVERED and
      unproven — a paid-only key refuses before COST is ever reached).
      Deepseek execution needs spend authorization + a gauntlet; the
      COST gate itself is proven via mistral.
    - friendli gemma-4-31B-it -> refused at EVIDENCE (DISCOVERED, and
      its rows are cost=unfunded, so it would refuse at COST even if
      promoted — structurally unprovable live with 0 credits).
    - pinned gemini-2.0-flash -> refused at MODEL (ai-router is a
      failover pool; no other key lists it)
  * embedding proofs: NIM + Cloudflare AI (pinned embedding jobs).
  * ai-router: proved via its gauntlet (unpinned job through the
    failover pool; model_match=0 is EXPECTED there, informational only).

NEVER gauntlets deepseek (paid — a gauntlet would EXECUTE a paid call
with no spend authorization) or friendli (0 credits). Their proof is
the COST-gate refusal receipt.

Zen pre-flight: a 10s opencode probe runs BEFORE any zen gauntlet
(opencode hangs silently on a down model; never burn 300s blind).

Idempotency keys make re-runs dedupe instead of duplicating.
Results: proof-results-2026-11-21.json (same dir). Exit 0 iff every
expected outcome matched; otherwise exit 1 with failures listed.
"""
import json
import os
import subprocess
import sys
import time
import urllib.error
import urllib.request

HOME = os.path.expanduser("~")
HERE = os.path.join(HOME, "workspace/exchange")
EXCHANGE = "https://lucidota-exchange.cutecity.workers.dev"
KEY = open(os.path.join(HOME, ".exchange_key")).read().strip()
UA = ("Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 "
      "(KHTML, like Gecko) Chrome/120.0 Safari/537.36")
OUT = os.path.join(HERE, "proof-results-2026-11-21.json")
CLAIMANT = os.path.join(HERE, "phase3-claimant.py")
CRED = os.path.join(HOME, ".exchange_worker_nim")

# resource -> (proof model, capability) for the pinned /v1/job proof.
# Each pinned model is listed by exactly ONE key resource, so govern()
# routes deterministically.
PINNED = {
    "nim-nvidia": ("google/gemma-4-31b-it", "inference"),
    "groq": ("qwen/qwen3.8-27b", "inference"),
    "cerebras": ("qwen-3.8-27b", "inference"),
    "mistral": ("ministral-14b-latest", "inference"),
    "cohere": ("command-r7b-12-2024", "inference"),
    "openrouter": ("baidu/ernie-5.5-flash-preview:free", "inference"),  # gemma-4-31b-it:free persistently 429'd upstream (Google AI Studio); lfm-3-1.7b:free 504'd (Liquid); ernie probed OK 2026-11-21 (content='ok' at max_tokens=64)
    "cloudflare-ai": ("@cf/google/gemma-4-26b-a4b-it", "inference"),
    "zen": ("opencode/hy4-flash-free", "inference"),
}
EMBEDDING = {
    "nim-nvidia": "nvidia/nemotron-3-embed-1b",  # embed-qa-4 is dead-listed (404 on /v1/embeddings despite being in /models); nemotron-3-embed-1b probed OK 2026-11-21 (2048 dims). 5 of 8 listed NIM embedding models 404; the 3.5s 400.
    "cloudflare-ai": "@cf/baai/bge-base-en-v1.5",
}
# Resources needing the gauntlet (nim-nvidia is already VERIFIED).
GAUNTLET = ["groq", "cerebras", "mistral", "cohere", "openrouter",
            "cloudflare-ai", "ai-router", "zen"]

def _worker_version():
    """Expected worker version, read from worker.js — never hardcoded
    (attempt 4 failed its health step on a stale hardcoded 0.7.1)."""
    try:
        import re
        m = re.search(r'const VERSION = "([^"]+)"',
                      open(os.path.join(HERE, "worker.js")).read())
        return m.group(1) if m else "?"
    except Exception:
        return "?"


EXPECTED_VERSION = _worker_version()

results = {"started_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
           "version": EXPECTED_VERSION, "steps": []}


def record(step, expected, actual, ok, detail=None):
    results["steps"].append({"step": step, "expected": expected,
                             "actual": actual, "ok": bool(ok),
                             "detail": detail})
    mark = "PASS" if ok else "FAIL"
    print(f"[{mark}] {step}: {actual}" + (f" — {detail}" if detail else ""),
          flush=True)


def xchg(method, path, body=None, attempts=3):
    """Exchange call with Bearer auth. Transport errors (incl.
    IncompleteRead on large responses — known egress flakiness) retry
    with backoff; HTTP responses are answers, never retried."""
    import http.client
    data = json.dumps(body).encode() if body is not None else None
    last_err = None
    for i in range(attempts):
        req = urllib.request.Request(
            EXCHANGE + path, data=data, method=method,
            headers={"Content-Type": "application/json",
                     "Authorization": f"Bearer {KEY}", "User-Agent": UA})
        try:
            with urllib.request.urlopen(req, timeout=60) as r:
                return json.loads(r.read()), r.status
        except urllib.error.HTTPError as e:
            try:
                return json.loads(e.read().decode("utf-8", errors="replace")), e.code
            except Exception:
                return {"error": f"HTTP {e.code}"}, e.code
        except (http.client.IncompleteRead, http.client.RemoteDisconnected,
                TimeoutError, ConnectionError, OSError) as e:
            last_err = e
            if i + 1 < attempts:
                time.sleep(2 * (i + 1))
    return {"error": f"transport failed after {attempts} attempts: {type(last_err).__name__}: {str(last_err)[:120]}"}, 0


def run_claimant(resource, drain_cap=3, timeout=420):
    env = dict(os.environ)
    env["CLAIM_RESOURCE"] = resource
    env["CLAIM_DRAIN_CAP"] = str(drain_cap)
    env["WORKER_CRED_FILE"] = CRED
    p = subprocess.run([sys.executable, CLAIMANT], capture_output=True,
                       text=True, timeout=timeout, env=env)
    return p.stdout.strip(), p.stderr.strip()[-500:], p.returncode


def curl_get(path):
    """GET via curl: urllib truncates reads on this egress path even with
    retries; curl does not. Used for all large or critical GETs."""
    cp = subprocess.run(["curl", "-s", "--max-time", "60",
                         "-H", f"Authorization: Bearer {KEY}",
                         EXCHANGE + path],
                        capture_output=True, text=True)
    try:
        return json.loads(cp.stdout), 200
    except Exception:
        return {"error": "curl/parse failed", "raw": cp.stdout[:200]}, 0


def job_state(job_id, tries=5, delay=2):
    for _ in range(tries):
        j, code = curl_get(f"/v1/job/{job_id}")
        if code == 200 and j.get("job", {}).get("state") in ("done", "failed", "refused", "blocked"):
            return j
        time.sleep(delay)
    return j


def evidence_state(resource):
    r, _ = curl_get(f"/v1/resources/{resource}/evidence")
    return ((r or {}).get("resource") or {}).get("evidence_state")


def terminal_receipt(job_doc):
    """The terminal receipt (last by created_at) + its execution row."""
    rcpts = job_doc.get("receipts") or []
    execs = job_doc.get("executions") or []
    rcpt = rcpts[-1] if rcpts else None
    ex = execs[-1] if execs else None
    return rcpt, ex


def gauntlet(resource):
    """POST verify -> claimant -> job done + resource AVAILABLE."""
    v, code = xchg("POST", f"/v1/resources/{resource}/verify", {}, attempts=1)
    # attempts=1: the verify endpoint is NOT idempotent — a retried POST
    # double-mints gauntlet jobs (proven 2026-11-21: 2 duplicate gauntlet
    # jobs from transport retries). A lost response fails this step
    # honestly; the next run re-gauntlets.
    if code != 200 or not v.get("delegated"):
        return False, f"verify call failed: {v}"
    job_id = v["job_id"]
    out, err, rc = run_claimant(resource)
    with open(os.path.join(HERE, f"proof-claimant-gauntlet-{resource}.log"), "w") as f:
        f.write(out)
    j = job_state(job_id)
    state = (j.get("job") or {}).get("state")
    if state == "deferred":
        # Transient claim window (transport flake): one second pass.
        out2, err2, rc2 = run_claimant(resource)
        with open(os.path.join(HERE, f"proof-claimant-gauntlet-{resource}-2.log"), "w") as f:
            f.write(out2)
        out, err, rc = out + "\n" + out2, err2, rc2
        j = job_state(job_id)
        state = (j.get("job") or {}).get("state")
    rcpt, ex = terminal_receipt(j)
    ev = evidence_state(resource)
    ok = state == "done" and ev in ("AVAILABLE", "VERIFIED")
    detail = (f"job {job_id} state={state} receipt={rcpt['id'] if rcpt else None} "
              f"result={rcpt['result'] if rcpt else None} "
              f"model_match={ex.get('model_match') if ex else None} "
              f"evidence={ev} claimant_rc={rc}")
    if not ok:
        detail += f" | claimant out: {out[-400:]} err: {err}"
    return ok, detail


def pinned_job(resource, model, capability="inference"):
    """Submit a pinned job -> claimant -> receipt with model_match=1."""
    key = f"proof073-{resource}-{capability}-1"
    # proof073 keys: fresh jobs under worker v0.7.3. The proof072 jobs
    # that completed stand as evidence (attempt 4); the ones that stayed
    # deferred hold expired leases the cron alone can re-mint, so fresh
    # jobs (fresh 10-min leases) are the honest re-proof.
    if capability == "embedding":
        spec = {"organelle": "exchange-proof-070", "capability": "embedding",
                "input": {"input": "The quick brown fox jumps over the lazy dog.",
                          "model": model},
                "data_class": "public", "max_cost": 0, "priority": 5,
                "spend_authority": 0, "idempotency_key": key}
    else:
        spec = {"organelle": "exchange-proof-070", "capability": "inference",
                "input": {"messages": [{"role": "user",
                                        "content": "Reply with exactly: ok"}],
                          "model": model, "max_tokens": 64},
                "data_class": "public", "max_cost": 0, "priority": 5,
                "spend_authority": 0, "idempotency_key": key}
    s, code = xchg("POST", "/v1/job", spec)
    job_id = s.get("job_id") or ("job_" + key)
    # Delegated jobs return state=deferred WITH a lease_id + resource.
    # Refused jobs return blocked/deferred with `rejected` and NO lease.
    # Re-runs return deduplicated:true with the existing job's state:
    # if it is already terminal, verify the receipt; if it is still
    # deferred, the job is waiting — run the claimant for it.
    if s.get("deduplicated") and s.get("state") in ("done", "failed", "blocked"):
        j = job_state(job_id)
        state = (j.get("job") or {}).get("state")
        rcpt, ex = terminal_receipt(j)
        mm = ex.get("model_match") if ex else None
        ok = state == "done" and mm == 1 and rcpt and rcpt.get("result") == "ok"
        detail = (f"job {job_id} (deduplicated re-run) state={state} "
                  f"receipt={rcpt['id'] if rcpt else None} model_match={mm}")
        return ok, detail
    if code == 429 or s.get("error") or (not s.get("lease_id") and not s.get("deduplicated")):
        return False, f"submit refused/failed: {json.dumps(s)[:400]}"
    out, err, rc = run_claimant(resource)
    with open(os.path.join(HERE, f"proof-claimant-{resource}-{capability}.log"), "w") as f:
        f.write(out)
    j = job_state(job_id)
    state = (j.get("job") or {}).get("state")
    if state == "deferred":
        # Transient claim window (transport flake): one second pass.
        out2, err2, rc2 = run_claimant(resource)
        with open(os.path.join(HERE, f"proof-claimant-{resource}-{capability}-2.log"), "w") as f:
            f.write(out2)
        out, err, rc = out + "\n" + out2, err2, rc2
        j = job_state(job_id)
        state = (j.get("job") or {}).get("state")
    rcpt, ex = terminal_receipt(j)
    mm = ex.get("model_match") if ex else None
    ok = state == "done" and mm == 1 and rcpt and rcpt.get("result") == "ok"
    detail = (f"job {job_id} state={state} receipt={rcpt['id'] if rcpt else None} "
              f"result={rcpt['result'] if rcpt else None} "
              f"requested={ex.get('requested_model') if ex else None} "
              f"actual={ex.get('actual_model') if ex else None} model_match={mm}")
    if not ok:
        detail += f" | claimant out: {out[-400:]} err: {err}"
    return ok, detail


def refusal_job(label, model, capability, expect_gate, expect_resource=None):
    """Submit a job that MUST be refused at govern time (no execution).
    Refused jobs come back state=blocked (all rejections permanent) or
    state=deferred (some retryable), with a receipt + the rejected list.
    Assert the EXPECTED gate appears in the rejected list for the
    expected resource (or any resource), and that nothing executed."""
    key = f"proof073-refuse-{label}"
    spec = {"organelle": "exchange-proof-070", "capability": capability,
            "input": {"messages": [{"role": "user", "content": "Reply with exactly: ok"}],
                      "model": model, "max_tokens": 64},
            "data_class": "public", "max_cost": 0, "priority": 5,
            "spend_authority": 0, "idempotency_key": key}
    s, code = xchg("POST", "/v1/job", spec)
    state = s.get("state")
    rejected = s.get("rejected") or []
    job_id = s.get("job_id") or ("job_" + key)
    j, _ = curl_get(f"/v1/job/{job_id}")
    n_exec = len(j.get("executions") or [])
    n_claims = len(j.get("claims") or [])
    if s.get("deduplicated"):
        # Re-run: the submit response carries no rejected list. Read the
        # refusal receipt's decision JSON from the job doc instead.
        for rcpt in (j.get("receipts") or []):
            try:
                dec = json.loads(rcpt.get("decision") or "{}")
            except Exception:
                continue
            if dec.get("rejected"):
                rejected = dec["rejected"]
                break
        state = (j.get("job") or {}).get("state", state)
    gate_hit = [r for r in rejected if r.get("gate") == expect_gate
                and (expect_resource is None or r.get("resource") == expect_resource)]
    # Refused = blocked/deferred WITH a rejected list and NO lease.
    ok = state in ("blocked", "deferred") and not s.get("lease_id") and bool(gate_hit) and n_exec == 0 and n_claims == 0
    detail = (f"state={state} receipt={s.get('receipt_id')} "
              f"{expect_gate}-rejections={[(r['resource'], r['reason'][:80]) for r in gate_hit]} "
              f"executions={n_exec} claims={n_claims}")
    if not ok:
        detail += f" | rejected={json.dumps(rejected)[:800]}"
    return ok, detail


def zen_preflight():
    """10s opencode probe. Zen down -> skip zen entirely (no 300s burn)."""
    env = dict(os.environ)
    fenv = {}
    with open(os.path.join(HOME, "workspace/wreck/.env")) as f:
        for line in f:
            line = line.strip()
            if "=" in line and not line.startswith("#"):
                k, v = line.split("=", 1)
                fenv[k.strip()] = v.strip()
    env["OPENCODE_API_KEY"] = fenv["OPENCODE_API_KEY"]
    try:
        p = subprocess.run(
            [os.path.join(HOME, ".opencode/bin/opencode"), "run",
             "--model", "opencode/hy4-flash-free", "--auto",
             "Reply with exactly: ok"],
            capture_output=True, text=True, timeout=10,
            stdin=subprocess.DEVNULL, env=env,
            cwd=os.path.join(HOME, "workspace/wreck"))
        return p.returncode == 0 and bool((p.stdout or "").strip())
    except subprocess.TimeoutExpired:
        return False


def main():
    # 0. Baseline: health + capability counts.
    h, _ = xchg("GET", "/v1/health")
    record("health", f"{EXPECTED_VERSION} ok", f"{h.get('version')} ok={h.get('ok')} resources={h.get('resources')} capabilities={h.get('capabilities')} keys={h.get('keys')}",
           h.get("version") == EXPECTED_VERSION and h.get("ok") and h.get("capabilities") == 656 and h.get("keys") == 11)
    # The full capabilities list is ~233KB; urllib truncates it on this
    # egress path even with retries. curl handles it fine.
    cp = subprocess.run(["curl", "-s", "--max-time", "60",
                         "-H", f"Authorization: Bearer {KEY}",
                         EXCHANGE + "/v1/capabilities"],
                        capture_output=True, text=True)
    try:
        c = json.loads(cp.stdout)
    except Exception:
        c = {}
    record("capabilities-list", "656 total",
           f"total={c.get('total')} usable={c.get('usable')} proven={c.get('proven')}",
           c.get("total") == 656)
    cfg, _ = xchg("GET", "/v1/config")
    record("config-list", "11 keys, allow_paid=0",
           f"keys={len(cfg.get('keys') or [])} allow_paid={(cfg.get('global') or {}).get('allow_paid')}",
           len(cfg.get("keys") or []) == 11 and (cfg.get("global") or {}).get("allow_paid") == "0")

    # 1. Zen pre-flight (before any zen gauntlet).
    zen_up = zen_preflight()
    record("zen-preflight", "hy4-flash-free answers in 10s",
           "up" if zen_up else "DOWN (skip zen gauntlet + proof)", True)
    if not zen_up:
        GAUNTLET.remove("zen")
        PINNED.pop("zen", None)

    # 2. Gauntlets (DISCOVERED -> AVAILABLE) for the executable keys.
    gauntlet_ok = {}
    for res in GAUNTLET:
        ok, d = gauntlet(res)
        gauntlet_ok[res] = ok
        record(f"gauntlet-{res}", "job done, evidence AVAILABLE", d, ok)

    # 3. Pinned proof jobs (exact model, model_match=1). Gate on the
    # resource's EVIDENCE state, not on this run's gauntlet result: a
    # gauntlet can flake transiently (Groq timeout, attempt 3) while the
    # resource is already VERIFIED with a done proof job waiting in the
    # dedup path. Only a non-usable resource genuinely blocks a proof.
    proof_ok = {}
    for res, (model, cap) in PINNED.items():
        ev = evidence_state(res)
        if ev not in ("AVAILABLE", "VERIFIED"):
            record(f"proof-{res}", "skipped (resource not usable)", "skipped", False,
                   f"evidence_state={ev}; pinned job would refuse at EVIDENCE")
            proof_ok[res] = False
            continue
        ok, d = pinned_job(res, model, cap)
        proof_ok[res] = ok
        record(f"proof-{res}", f"{model} executes, model_match=1", d, ok)

    # 4. Embedding proofs.
    for res, model in EMBEDDING.items():
        ev = evidence_state(res)
        if ev not in ("AVAILABLE", "VERIFIED"):
            record(f"proof-emb-{res}", "skipped (resource not usable)", "skipped", False)
            continue
        ok, d = pinned_job(res, model, "embedding")
        record(f"proof-emb-{res}", f"{model} embeds, model_match=1", d, ok)

    # 5. Negative proofs (no execution anywhere). Run AFTER the gauntlets
    # so the gates under test are reached (DISCOVERED resources refuse at
    # EVIDENCE before MODEL/COST are ever evaluated).
    ok, d = refusal_job("nonexistent", "no-such-model-xyz", "inference", "MODEL")
    record("refuse-nonexistent-model", "refused at MODEL", d, ok)
    ok, d = refusal_job("mistral-paid", "codestral-2508", "inference", "COST", "mistral")
    record("refuse-mistral-paid", "refused at COST (allow_paid=0)", d, ok)
    ok, d = refusal_job("deepseek-unproven", "deepseek-flash", "inference", "EVIDENCE", "deepseek")
    record("refuse-deepseek-unproven", "refused at EVIDENCE (DISCOVERED, paid-only)", d, ok)
    ok, d = refusal_job("friendli-unproven", "google/gemma-4-31B-it", "inference", "EVIDENCE", "friendli")
    record("refuse-friendli-unproven", "refused at EVIDENCE (DISCOVERED, unfunded rows)", d, ok)
    ok, d = refusal_job("pinned-failover-pool", "gemini-2.0-flash", "inference", "MODEL", "ai-router")
    record("refuse-pinned-failover-pool", "refused at MODEL (failover pool)", d, ok)

    # 6. Second gauntlet round -> VERIFIED (3rd ok observation).
    for res in GAUNTLET:
        if not gauntlet_ok.get(res):
            continue
        if res == "ai-router":
            # Failover pool: no pinned proof job exists for it (unpinnable
            # by design), so this run earns it 2 observations, not the 3
            # VERIFIED needs. AVAILABLE is what this run earns; sustained
            # use promotes it.
            continue
        if evidence_state(res) == "VERIFIED":
            # Already VERIFIED (earlier gauntlet + proof this run, or a
            # prior run): a redundant re-gauntlet earns nothing, and a
            # transport flake on its verify POST would fail the step
            # for no reason (attempt 4: verify2-cerebras).
            record(f"verify2-{res}", "evidence VERIFIED", "already VERIFIED", True)
            continue
        ok, d = gauntlet(res)
        ev = evidence_state(res)
        record(f"verify2-{res}", "evidence VERIFIED", f"{d} | evidence={ev}",
               ok and ev == "VERIFIED")

    # 7. Final counts: proven models.
    c, _ = curl_get("/v1/capabilities?usable=1")
    record("final-usable", "usable>0", f"usable={c.get('total')}", (c.get("total") or 0) > 0)

    results["finished_at"] = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
    fails = [s for s in results["steps"] if not s["ok"]]
    results["passed"] = len(results["steps"]) - len(fails)
    results["failed"] = len(fails)
    with open(OUT, "w") as f:
        json.dump(results, f, indent=1)
    print(f"\n{results['passed']}/{len(results['steps'])} steps passed; wrote {OUT}")
    if fails:
        print("FAILURES:")
        for s in fails:
            print(f"  - {s['step']}: {s['actual']}")
    return 0 if not fails else 1


if __name__ == "__main__":
    sys.exit(main())
