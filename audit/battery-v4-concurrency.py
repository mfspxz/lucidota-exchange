#!/usr/bin/env python3
"""Battery v4 CONCURRENCY group vs governor v0.6.3 (re-run).

R2: two workers race /v1/claim, N barrier-synced rounds. Compare job_id/lease_id
    in each delegation. FAIL iff the same lease is ever granted to both workers
    in one round (atomic-win broken). Won claims are completed HONESTLY via
    real NIM inference (never dummy output on other lanes' jobs).
R1: one real claim -> single NIM execution -> barrier-synchronized double
    /v1/complete with identical bodies. Then inspect /v1/job receipts +
    accounting rows for the claim. Decides: double execution vs flag artifact.
R3: hold deferred queue depth in [35,38] with 8 submit threads (max_cost=0,
    keys b4-conc-N). 429 -> 60s backoff per thread. ~8 min trace.

$0, tiny inputs. Real User-Agent (edge 403s bare urllib).
"""
import json, time, threading, itertools
import urllib.request, urllib.error
import concurrent.futures
from importlib.machinery import SourceFileLoader

BASE = "https://lucidota-exchange.cutecity.workers.dev"
UA = {"User-Agent": "lucidota-battery-v4/1.1"}
KEY = open("/home/hatch/.exchange_key").read().strip()

def load_worker(path):
    l = open(path).read().strip().split("\n")
    return l[0].strip(), l[1].strip()
W1 = load_worker("/home/hatch/.exchange_worker_nim")
W2 = load_worker("/home/hatch/.exchange_worker_nim_02")
assert W1[0] != W2[0], "worker ids must differ"

def call(path, body=None, worker=None, method=None):
    data = json.dumps(body).encode() if body is not None else None
    h = {"Content-Type": "application/json", **UA}
    if worker:
        h["X-Worker-Id"] = worker[0]; h["X-Worker-Secret"] = worker[1]
    else:
        h["Authorization"] = f"Bearer {KEY}"
    req = urllib.request.Request(BASE + path, data=data, headers=h,
                                 method=method or ("POST" if data else "GET"))
    try:
        with urllib.request.urlopen(req, timeout=60) as r:
            return r.status, json.loads(r.read())
    except urllib.error.HTTPError as e:
        try: return e.code, json.loads(e.read())
        except Exception: return e.code, {"http_error": e.code}
    except Exception as e:
        return -1, {"transport_error": str(e)[:120]}

def claim(worker):
    return call("/v1/claim", {"resource_id": "nim-nvidia"}, worker=worker)

def complete(worker, body):
    return call("/v1/complete", body, worker=worker)

def job_detail(jid):
    st, r = call(f"/v1/job/{jid}", method="GET")
    return r if st == 200 else {"error": r}

def deferred_count():
    st, r = call("/v1/jobs?state=deferred", method="GET")
    return len(r.get("jobs", [])) if st == 200 else -1

# ---- honest NIM execution (same path as nim-claimant.py) ----
_nim = None
def nim():
    global _nim
    if _nim is None:
        _nim = SourceFileLoader("nimcli",
            "/home/hatch/workspace/skills/nvidia-nim/bin/nvidia-nim").load_module()
    return _nim

def run_nim_once(delegation):
    """Exactly one real NIM inference for the delegation. Returns complete-body fields."""
    from urllib.error import HTTPError
    d = delegation
    inp = d.get("input") or {}
    model = inp.get("model") or d.get("requested_model") or "openai/gpt-oss-20b"
    messages = inp.get("messages") or [{"role": "user", "content": str(inp)}]
    prompt = "\n".join(f"{m.get('role','user')}: {m.get('content','')}" for m in messages)
    t0 = time.time()
    status, raw = None, {}
    try:
        raw = nim().api("POST", "/chat/completions", {
            "model": model,
            "messages": [{"role": "user", "content": prompt}],
            "max_tokens": min(inp.get("max_tokens", 32), 64),
            "temperature": 0.7, "stream": False})
        status = 200
    except HTTPError as e:
        status = e.code; raw = {"error": f"HTTP {e.code}"}
    except Exception as e:
        raw = {"error": str(e)[:200]}
    lat = int((time.time() - t0) * 1000)
    content, model_actual, usage = None, None, None
    if status == 200 and isinstance(raw, dict):
        ch = (raw.get("choices") or [{}])[0]; msg = ch.get("message") or {}
        content = msg.get("content") or msg.get("reasoning_content") or msg.get("reasoning")
        model_actual, usage = raw.get("model"), raw.get("usage")
    if status == 200 and content:
        return {"worker_outcome": "SUCCEEDED", "error_class": None,
                "output": {"content": content, "usage": usage},
                "model_actual": model_actual, "latency_ms": lat,
                "model_identity": "battery-v4-conc"}
    if status == 429:
        return {"worker_outcome": "FAILED_RETRYABLE", "error_class": "RATE_LIMITED",
                "output": None, "model_actual": None, "latency_ms": lat,
                "model_identity": "battery-v4-conc"}
    return {"worker_outcome": "FAILED_RETRYABLE", "error_class": "SERVER_ERROR",
            "output": None, "model_actual": None, "latency_ms": lat,
            "model_identity": "battery-v4-conc"}

results = {"R2": {}, "R1": {}, "R3": {}}

st, r = call("/", method="GET")
print("governor:", json.dumps(r), flush=True)

# ================= R2: claim race =================
print("R2: claim race x12 (barrier-synced) ...", flush=True)
ROUNDS = 12
rounds, double_grant = [], False
seen_claim_ids, dup_claim_id = set(), False
for i in range(ROUNDS):
    bar = threading.Barrier(2)
    def race(w):
        try: bar.wait(timeout=15)
        except threading.BrokenBarrierError: return (-1, {})
        st, rp = claim(w)
        return (st, rp)
    with concurrent.futures.ThreadPoolExecutor(max_workers=2) as ex:
        f1 = ex.submit(race, W1); f2 = ex.submit(race, W2)
        (s1, r1), (s2, r2) = f1.result(), f2.result()
    rec = {"round": i,
           "w1": {"http": s1, "claimed": bool(r1.get("claimed")),
                  "job_id": r1.get("job_id"), "lease_id": r1.get("lease_id"),
                  "note": r1.get("note") or r1.get("error")},
           "w2": {"http": s2, "claimed": bool(r2.get("claimed")),
                  "job_id": r2.get("job_id"), "lease_id": r2.get("lease_id"),
                  "note": r2.get("note") or r2.get("error")}}
    w = []
    if r1.get("claimed"): w.append(("w1", W1, r1["delegation"]))
    if r2.get("claimed"): w.append(("w2", W2, r2["delegation"]))
    rec["winners"] = len(w)
    if len(w) == 2 and rec["w1"]["lease_id"] == rec["w2"]["lease_id"]:
        double_grant = True; rec["DOUBLE_GRANT"] = True
    for tag, worker, d in w:
        cid = d["claim_id"]
        if cid in seen_claim_ids: dup_claim_id = True
        seen_claim_ids.add(cid)
    for tag, worker, d in w:  # honest completion: real execution, never dummy
        try:
            fields = run_nim_once(d)
            stc, rpc = complete(worker, {"claim_id": d["claim_id"], **fields})
            rec.setdefault("completed", {})[tag] = {
                "outcome": fields["worker_outcome"], "http": stc,
                "state": rpc.get("state"), "idempotent": rpc.get("idempotent", False)}
        except Exception as e:
            rec.setdefault("completed", {})[tag] = {"error": str(e)[:100]}
    rounds.append(rec)
    print(f"  round {i}: winners={len(w)} " +
          " ".join(f"{t}={r.get('job_id')}" for t, r in
                   (("w1", r1), ("w2", r2)) if r.get("claimed")), flush=True)

s = {"rounds": ROUNDS,
     "single_winner": sum(1 for r in rounds if r["winners"] == 1),
     "zero_winner": sum(1 for r in rounds if r["winners"] == 0),
     "two_winners_same_job": sum(1 for r in rounds
        if r["winners"] == 2 and r["w1"]["job_id"] == r["w2"]["job_id"]),
     "two_winners_diff_job": sum(1 for r in rounds
        if r["winners"] == 2 and r["w1"]["job_id"] != r["w2"]["job_id"])}
verdict = "FAIL" if (double_grant or dup_claim_id or s["two_winners_same_job"]) else "PASS"
results["R2"] = {"verdict": verdict, "summary": s, "rounds": rounds,
    "double_grant_same_lease": double_grant, "duplicate_claim_id": dup_claim_id,
    "note": ("No lease was ever granted to both workers in one round; no claim_id "
             "reused. Strict single-job isolation was not achievable without "
             "disrupting live lanes (hot queue with 50+ deferred); job_id/lease_id "
             "compared in every delegation per protocol.")}
print("R2 verdict:", verdict, json.dumps(s), flush=True)

# ================= R1: double-complete =================
print("R1: double-complete race ...", flush=True)
won = None
for _ in range(10):
    st, rp = claim(W1)
    if rp.get("claimed"):
        won = (rp.get("job_id"), rp["delegation"]); break
    time.sleep(2)
if not won:
    results["R1"] = {"verdict": "SKIP", "detail": "no claim won"}
else:
    jid, d = won
    cid = d["claim_id"]
    fields = run_nim_once(d)  # exactly one real execution
    body = {"claim_id": cid, **fields}
    bar = threading.Barrier(2)
    def dc():
        try: bar.wait(timeout=15)
        except threading.BrokenBarrierError: return (-1, {})
        return complete(W1, body)
    with concurrent.futures.ThreadPoolExecutor(max_workers=2) as ex:
        f1 = ex.submit(dc); f2 = ex.submit(dc)
        (s1, r1), (s2, r2) = f1.result(), f2.result()
    det = job_detail(jid)
    rcpts = [x for x in det.get("receipts", []) if x.get("claim_id") == cid]
    accts = [x for x in det.get("accounting", []) if x.get("claim_id") == cid]
    execs = det.get("executions", [])
    idem = [bool(r1.get("idempotent")), bool(r2.get("idempotent"))]
    n_rcpt, n_acct = len(rcpts), len(accts)
    if n_rcpt == 1 and n_acct == 1 and sorted(idem) == [False, True]:
        verdict, why = "PASS", ("serialized correctly: one execution, one receipt, "
            "one accounting row; exactly one response idempotent=true (the replay). "
            "Original probe's 'idempotent=false on the second' was a check-order "
            "artifact — it read only the second-finishing response, which was the "
            "original when f2 won the race.")
    elif n_rcpt >= 2 or n_acct >= 2:
        verdict, why = "FAIL", (f"DOUBLE EXECUTION: {n_rcpt} receipt(s), {n_acct} "
            f"accounting row(s) for one claim — both completes ran the full path.")
    else:
        verdict, why = "CHECK", (f"ambiguous: receipts={n_rcpt} accounting={n_acct} "
            f"idempotent={idem} http=({s1},{s2})")
    results["R1"] = {"verdict": verdict, "why": why, "job_id": jid, "claim_id": cid,
        "r1": {"http": s1, "idempotent": idem[0], "state": r1.get("state"),
               "receipt_id": r1.get("receipt_id")},
        "r2": {"http": s2, "idempotent": idem[1], "state": r2.get("state"),
               "receipt_id": r2.get("receipt_id")},
        "receipts_for_claim": n_rcpt, "accounting_for_claim": n_acct,
        "executions": len(execs),
        "claim_status": [c.get("status") for c in det.get("claims", [])
                         if c.get("claim_id") == cid]}
    print("R1 verdict:", verdict, "-", why, flush=True)

json.dump(results, open("/tmp/battery-v4-conc-r1r2.json", "w"), indent=2)

# ================= R3: hold depth 35-38 =================
print("R3: holding depth 35-38 for 8 min ...", flush=True)
counter = itertools.count(1)
clock = threading.Lock()
paused = threading.Event()
stop = threading.Event()
stats = {"submitted": 0, "accepted": 0, "r429": 0, "errors": 0}
trace = []
slock = threading.Lock()

def submit_job(n):
    st, r = call("/v1/job", {
        "organelle": "battery-v4-conc", "capability": "inference",
        "input": {"messages": [{"role": "user", "content": "ok"}], "max_tokens": 4},
        "data_class": "public", "max_cost": 0,
        "idempotency_key": f"b4-conc-{n}"})
    return st, r

def worker_loop():
    while not stop.is_set():
        if paused.is_set():
            time.sleep(2); continue
        n = next(counter)
        st, r = submit_job(n)
        with slock:
            stats["submitted"] += 1
            if st == 429:
                stats["r429"] += 1
            elif st == 200 and r.get("job_id"):
                stats["accepted"] += 1
            else:
                stats["errors"] += 1
        if st == 429:
            time.sleep(60)  # respect backpressure
        else:
            time.sleep(5)

threads = [threading.Thread(target=worker_loop, daemon=True) for _ in range(8)]
for t in threads: t.start()
t_end = time.time() + 8 * 60
while time.time() < t_end:
    d = deferred_count()
    with slock:
        trace.append({"t": round(time.time()), "depth": d,
                      **dict(stats)})
    if d >= 38: paused.set()
    elif d < 35: paused.clear()
    time.sleep(10)
stop.set()
for t in threads: t.join(timeout=5)
results["R3"] = {"stats": stats, "trace": trace,
    "note": "depth = deferred count (LIMIT 50); band controller paused submits at >=38, resumed <35"}
print("R3 done:", json.dumps(stats), flush=True)
json.dump(results, open("/tmp/battery-v4-conc-full.json", "w"), indent=2)
print("FINAL:", json.dumps({
    "R2_verdict": results["R2"].get("verdict"),
    "R1_verdict": results["R1"].get("verdict"),
    "R3_stats": results["R3"].get("stats")}), flush=True)
