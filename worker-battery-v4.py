#!/usr/bin/env python3
"""Battery v4 concurrency races vs governor v0.6.2.
R1: parallel /v1/complete on the same claim (idempotency under race).
R2: parallel /v1/claim from 2 workers (atomic win, one winner).
R3: backpressure threshold under parallel submit (soft-cap overshoot bound).
$0, tiny inputs, respects 429s.
"""
import json, urllib.request, concurrent.futures, time, sys

BASE = "https://lucidota-exchange.cutecity.workers.dev"
KEY = open("/home/hatch/.exchange_key").read().strip()
def load_worker(path):
    lines = open(path).read().strip().split("\n")
    return lines[0].strip(), lines[1].strip()
W1 = load_worker("/home/hatch/.exchange_worker_nim")
W2 = load_worker("/home/hatch/.exchange_worker_nim_02")
UA = {"User-Agent": "lucidota-battery-v4/1.0"}

def call(path, body, worker=None):
    data = json.dumps(body).encode()
    h = {"Content-Type": "application/json", **UA}
    if worker:
        wid, sec = worker
        h["X-Worker-Id"] = wid
        h["X-Worker-Secret"] = sec
    else:
        h["Authorization"] = f"Bearer {KEY}"
    req = urllib.request.Request(BASE + path, data=data, headers=h, method="POST")
    try:
        with urllib.request.urlopen(req, timeout=30) as r:
            return r.status, json.loads(r.read())
    except urllib.error.HTTPError as e:
        try: return e.code, json.loads(e.read())
        except Exception: return e.code, {"http_error": e.code}

def submit(i):
    st, r = call("/v1/job", {"organelle": "battery-v4", "capability": "inference",
        "input": {"messages": [{"role": "user", "content": "ok"}], "max_tokens": 8},
        "data_class": "public", "max_cost": 0,
        "idempotency_key": f"b4-r3-{i}"})
    return st, r

def claim(worker):
    return call("/v1/claim", {"resource_id": "nim-nvidia"}, worker=worker)

results = {}

# R2: two workers race to claim. First drain to a known job.
print("R2: claim race...", flush=True)
st, r = submit(1000)
jid = r.get("job_id")
time.sleep(1)
with concurrent.futures.ThreadPoolExecutor(max_workers=2) as ex:
    f1 = ex.submit(claim, W1)
    f2 = ex.submit(claim, W2)
    c1, c2 = f1.result(), f2.result()
winners = [c for c in (c1, c2) if c[1].get("claimed")]
results["R2_claim_race"] = {
    "winners": len(winners),
    "verdict": "PASS" if len(winners) == 1 else "FAIL",
    "detail": f"c1_claimed={c1[1].get('claimed')} c2_claimed={c2[1].get('claimed')}",
}
# Complete the winner's claim so it doesn't strand.
if winners:
    wbody = winners[0][1]
    # find which worker won by matching claim
    for c, w in ((c1, W1), (c2, W2)):
        if c[1].get("claimed"):
            call("/v1/complete", {"claim_id": c[1]["delegation"]["claim_id"],
                "worker_outcome": "SUCCEEDED", "output": "ok",
                "model_identity": "test", "latency_ms": 1}, worker=w)
            break

# R1: parallel double-complete on the same claim.
print("R1: double-complete race...", flush=True)
st, r = submit(1001)
time.sleep(2)  # let a claimant pick it up? No — claim it ourselves.
c1 = claim(W1)
if not c1[1].get("claimed"):
    # queue may be busy; try once more
    time.sleep(3)
    c1 = claim(W1)
if c1[1].get("claimed"):
    cid = c1[1]["delegation"]["claim_id"]
    body = {"claim_id": cid, "worker_outcome": "SUCCEEDED", "output": "ok",
            "model_identity": "test", "latency_ms": 1}
    with concurrent.futures.ThreadPoolExecutor(max_workers=2) as ex:
        f1 = ex.submit(call, "/v1/complete", body, W1)
        f2 = ex.submit(call, "/v1/complete", body, W1)
        r1, r2 = f1.result(), f2.result()
    idem = r2[1].get("idempotent", False)
    results["R1_double_complete"] = {
        "verdict": "PASS" if (r1[0] == 200 and r2[0] == 200 and idem) else "CHECK",
        "detail": f"r1={r1[0]} r2={r2[0]} idempotent={idem}",
    }
else:
    results["R1_double_complete"] = {"verdict": "SKIP", "detail": "no claim won"}

# R3: 32 parallel submits vs backpressure.
print("R3: parallel submit vs backpressure...", flush=True)
with concurrent.futures.ThreadPoolExecutor(max_workers=8) as ex:
    outs = list(ex.map(submit, range(2000, 2032)))
accepted = sum(1 for st, r in outs if st == 200 and r.get("job_id") and not r.get("error"))
rejected = sum(1 for st, r in outs if st == 429)
depths = [r.get("queue_depth") for st, r in outs if st == 429 and r.get("queue_depth")]
results["R3_backpressure_race"] = {
    "accepted": accepted, "rejected_429": rejected,
    "max_observed_depth": max(depths) if depths else None,
    "verdict": "PASS" if (max(depths) if depths else 0) <= 42 else "CHECK",
}

print(json.dumps(results, indent=2))
open("/tmp/battery-v4-races.json", "w").write(json.dumps(results, indent=2))
