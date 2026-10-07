#!/usr/bin/env python3
"""INTEGRITY-GROUP battery v4: live verify v0.6.3 INTEGRITY + COST gates.
- QUOTA/COST check: max_cost=1 (no spend_authority) -> blocked at COST.
- 10 inference jobs (idempotency b4-int-1..10), 2 min apart, max_cost=0.
- For each job: fetch /v1/job/<id> receipts; parse gate trace for INTEGRITY failures.
- Rolling window: /v1/receipts latest 50 scanned each round for INTEGRITY failures;
  if any, capture resource state/evidence_state/observed_at via /v1/resources.
- Respects 429s with 60s backoff.
$0. Log: /tmp/integrity-battery-v4.log ; results: /tmp/integrity-battery-v4.jsonl
"""
import json, time, urllib.request, urllib.error, datetime

BASE = "https://lucidota-exchange.cutecity.workers.dev"
KEY = open("/home/hatch/.exchange_key").read().strip()
UA = {"User-Agent": "lucidota-battery-v4-integrity/1.0"}
LOG = "/tmp/integrity-battery-v4.log"
OUT = "/tmp/integrity-battery-v4.jsonl"

def log(msg):
    line = f"{datetime.datetime.now(datetime.timezone.utc).isoformat()} {msg}"
    print(line, flush=True)
    open(LOG, "a").write(line + "\n")

def call(path, body=None, method=None):
    data = json.dumps(body).encode() if body is not None else None
    h = {"Content-Type": "application/json", **UA, "Authorization": f"Bearer {KEY}"}
    req = urllib.request.Request(BASE + path, data=data, headers=h, method=method or ("POST" if body is not None else "GET"))
    try:
        with urllib.request.urlopen(req, timeout=40) as r:
            return r.status, json.loads(r.read())
    except urllib.error.HTTPError as e:
        try: return e.code, json.loads(e.read())
        except Exception: return e.code, {"http_error": e.code}

def emit(rec):
    open(OUT, "a").write(json.dumps(rec) + "\n")

def submit_with_backoff(body, label, max_tries=3):
    for t in range(max_tries):
        st, r = call("/v1/job", body)
        if st == 429:
            log(f"{label}: 429 rate-limited, backing off 65s (try {t+1})")
            time.sleep(65)
            continue
        return st, r
    log(f"{label}: FAILED after 429 retries")
    return 429, {"error": "rate-limited"}

def trace_integrity_fails(trace):
    return [e for e in (trace or []) if e.get("gate") == "INTEGRITY" and not e.get("pass")]

def rejected_integrity(rej):
    return [e for e in (rej or []) if e.get("gate") == "INTEGRITY"]

results = {"quota_cost_check": None, "jobs": [], "rolling_window_scans": 0, "rolling_integrity_fails": 0}

# ---- 1. COST gate check: max_cost=1 with no spend_authority ----
log("COST-CHECK: submit max_cost=1 (no spend_authority)")
st, r = submit_with_backoff({
    "organelle": "integrity-battery-v4", "capability": "inference",
    "input": {"messages": [{"role": "user", "content": "cost gate probe"}], "max_tokens": 4},
    "data_class": "public", "max_cost": 1, "idempotency_key": "b4-cost-probe"},
    "cost-probe")
rej = r.get("rejected", [])
cost_blocks = [x for x in rej if x.get("gate") == "COST"]
ok = (st == 200 and r.get("state") == "blocked" and cost_blocks and
      cost_blocks[0].get("reason") == "declared max_cost $1 exceeds spend_authority $0")
results["quota_cost_check"] = {
    "http": st, "job_id": r.get("job_id"), "state": r.get("state"),
    "rejected": rej, "verdict": "PASS" if ok else "FAIL"}
log(f"COST-CHECK: http={st} state={r.get('state')} rejected={json.dumps(rej)} -> {'PASS' if ok else 'FAIL'}")
emit({"check": "cost", **results["quota_cost_check"]})

# ---- 2. Ten jobs, 2 min apart ----
for i in range(1, 11):
    key = f"b4-int-{i}"
    log(f"JOB {i}/10: submit {key} max_cost=0")
    st, r = submit_with_backoff({
        "organelle": "integrity-battery-v4", "capability": "inference",
        "input": {"messages": [{"role": "user", "content": f"integrity probe {i}"}], "max_tokens": 4},
        "data_class": "public", "max_cost": 0, "idempotency_key": key},
        f"job-{key}")
    rec = {"n": i, "key": key, "http": st, "job_id": r.get("job_id"), "state": r.get("state"),
           "deduplicated": r.get("deduplicated", False),
           "submit_rejected": r.get("rejected", []), "integrity_reject_at_submit": 0,
           "receipt_integrity_fails": 0, "gate_trace": None}
    rec["integrity_reject_at_submit"] = len(rejected_integrity(r.get("rejected", [])))
    jid = r.get("job_id")
    if jid and st == 200 and not r.get("error"):
        time.sleep(4)  # let receipt land
        st2, jr = call(f"/v1/job/{jid}")
        if st2 == 200:
            fails = []
            for rcpt in jr.get("receipts", []):
                try:
                    dec = json.loads(rcpt.get("decision") or "{}")
                except Exception:
                    dec = {}
                fails += trace_integrity_fails(dec.get("trace", []))
                fails += rejected_integrity(dec.get("rejected", []))
            rec["receipt_integrity_fails"] = len(fails)
            rec["n_receipts"] = len(jr.get("receipts", []))
            if fails:
                rec["fail_detail"] = fails[:5]
                # capture resource state/evidence_state/observed_at at moment of failure
                st3, res = call("/v1/resources")
                rec["resources_snapshot"] = (res.get("resources") if st3 == 200 else None)
    rec["verdict"] = "PASS" if (rec["integrity_reject_at_submit"] == 0 and rec["receipt_integrity_fails"] == 0) else "FAIL"
    results["jobs"].append(rec)
    emit({"job": rec})
    log(f"JOB {i}/10: http={st} state={rec['state']} int_reject_submit={rec['integrity_reject_at_submit']} int_fail_receipts={rec['receipt_integrity_fails']} -> {rec['verdict']}")
    # rolling window scan
    stw, rw = call("/v1/receipts")
    if stw == 200:
        results["rolling_window_scans"] += 1
        wfails = []
        for rcpt in rw.get("receipts", []):
            try: dec = json.loads(rcpt.get("decision") or "{}")
            except Exception: dec = {}
            for e in trace_integrity_fails(dec.get("trace", [])):
                wfails.append({"receipt": rcpt.get("id"), "job": rcpt.get("job_id"), "gate_entry": e})
            for e in rejected_integrity(dec.get("rejected", [])):
                wfails.append({"receipt": rcpt.get("id"), "job": rcpt.get("job_id"), "rejected_entry": e})
        if wfails:
            results["rolling_integrity_fails"] += len(wfails)
            log(f"ROLLING-WINDOW: {len(wfails)} INTEGRITY failures found: {json.dumps(wfails[:3])}")
            st3, res = call("/v1/resources")
            emit({"rolling_window_failure": {"fails": wfails[:5],
                  "resources": (res.get("resources") if st3 == 200 else None)}})
        else:
            log(f"ROLLING-WINDOW scan {results['rolling_window_scans']}: 0 INTEGRITY failures in latest 50 receipts")
    if i < 10:
        time.sleep(120)

results["verdict"] = "PASS" if all(j["verdict"] == "PASS" for j in results["jobs"]) \
    and results["quota_cost_check"]["verdict"] == "PASS" \
    and results["rolling_integrity_fails"] == 0 else "FAIL"
log(f"FINAL VERDICT: {results['verdict']}")
open("/tmp/integrity-battery-v4-summary.json", "w").write(json.dumps(results, indent=2))
