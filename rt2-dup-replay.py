#!/usr/bin/env python3
"""REDTEAM-2 duplicate-replay group vs live Exchange governor v0.6.6.
(a) identical POST /v1/ballots x2  -> two ballots? no job collision?
(b) GET decided ballot 3x          -> identical tally?
(c) late vote job on decided ballot -> tally frozen or changed?
(d) POST /v1/complete twice, same claim -> double receipt / double tally?
Rate: ~1 req / 1.7s (cap 40/min, hold ~35). 429 -> back off 60s.
Secrets loaded from files, never printed.
"""
import json, sys, time, hashlib, urllib.request, urllib.error

EX = "https://lucidota-exchange.cutecity.workers.dev"
UA = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0 Safari/537.36"
XKEY = open("/home/hatch/.exchange_key").read().strip()
WID, WSEC = open("/home/hatch/.exchange_worker_rt2local").read().strip().split("\n")[:2]

def req(method, path, body=None, worker=False):
    h = {"User-Agent": UA, "Content-Type": "application/json"}
    if worker:
        h["X-Worker-Id"] = WID; h["X-Worker-Secret"] = WSEC
    else:
        h["Authorization"] = "Bearer " + XKEY
    data = json.dumps(body).encode() if body is not None else None
    r = urllib.request.Request(EX + path, data=data, method=method, headers=h)
    try:
        with urllib.request.urlopen(r, timeout=45) as resp:
            return resp.status, json.loads(resp.read().decode())
    except urllib.error.HTTPError as e:
        try: return e.code, json.loads(e.read().decode())
        except Exception: return e.code, {"http_error": e.code}

def call(method, path, body=None, worker=False):
    for attempt in range(4):
        st, d = req(method, path, body, worker)
        if st == 429:
            print("  [429] backing off 60s", flush=True); time.sleep(60); continue
        time.sleep(1.7)
        return st, d
    return st, d

def ballot(bid):
    st, d = call("GET", f"/v1/ballots/{bid}")
    assert st == 200, (st, d)
    return d

def snap(b):
    t = b["tally"]
    return {"state": b["state"], "verdict": b["verdict"],
            "valid": t["valid"], "counts": t["counts"],
            "votes": [(v["job_id"], v["vote"], v["valid"]) for v in t["votes"]]}

log = {"steps": []}
def step(name, **kw):
    log["steps"].append({"name": name, **kw})
    print(f"### {name}: " + json.dumps(kw)[:300], flush=True)

# ---- create ballot D (or reuse from argv) ----
if len(sys.argv) > 1:
    BID = sys.argv[1]
    st, b = call("GET", f"/v1/ballots/{BID}")
    assert st == 200, (st, b)
    step("ballot_reused", ballot_id=BID)
else:
    st, b = call("POST", "/v1/ballots", {"ballot_type": "verdict",
        "proposition": "REDTEAM2-DECIDE: water is wet",
        "voters": ["openai/gpt-oss-20b", "nvidia/nemotron-3.5-lightning-30b-a3b"],
        "deadline_s": 600})
    assert st == 200, (st, b)
    BID = b["ballot_id"]
    step("ballot_created", ballot_id=BID)
# job ids follow the ballot pattern; re-derive (create response lists them too)
myjobs = {f"job_ballot_{BID}_openai_gpt_oss_20b",
          f"job_ballot_{BID}_nvidia_nemotron_3_5_lightning_30b_a3b"}
step("ballot_jobs", jobs=sorted(myjobs))

# ---- claim both vote jobs immediately ----
claims = {}
for i in range(8):
    st, c = call("POST", "/v1/claim", {"resource_id": "nim-nvidia"}, worker=True)
    if c.get("claimed") and c.get("job_id") in myjobs and c["job_id"] not in claims:
        claims[c["job_id"]] = c["delegation"]["claim_id"]
        step("claimed", job=c["job_id"], n=len(claims))
    if len(claims) == 2: break
    if not c.get("claimed"): time.sleep(3)
step("claims_done", n=len(claims), jobs=sorted(claims))

# ---- complete both as AUTHORIZED ----
VOTE = "REDTEAM2 manual vote, deterministic.\n\nFinal verdict:\nAUTHORIZED"
for job_id, claim_id in claims.items():
    st, r = call("POST", "/v1/complete",
        {"claim_id": claim_id, "worker_outcome": "SUCCEEDED",
         "output": {"content": VOTE}, "latency_ms": 42,
         "model_actual": "rt2-manual"}, worker=True)
    step("completed", job=job_id, status=st,
         outcome=r.get("worker_outcome"), receipt=r.get("receipt_id"))

# ---- wait for decided, then (b) triple-read ----
bd = None
for i in range(10):
    bd = ballot(BID)
    if bd["state"] != "open": break
    time.sleep(5)
step("ballot_terminal", state=bd["state"], verdict=bd["verdict"], tally=snap(bd))
reads = []
for i in range(3):
    r = ballot(BID)
    h = hashlib.sha256(json.dumps(r, sort_keys=True).encode()).hexdigest()
    reads.append(h)
    time.sleep(2)
step("test_b_triple_read", identical=len(set(reads)) == 1, hashes=reads,
     tally=snap(ballot(BID)))

# ---- (c) late vote job on the decided ballot ----
pre = snap(ballot(BID))
late_key = f"ballot_{BID}_rt2_latevote"
late_spec = {"organelle": "ballot-voter", "capability": "inference",
    "input": {"model": "rt2-latevote",
        "messages": [{"role": "user", "content":
            "REDTEAM2-LATEVOTE: the moon is made of cheese. This statement is false.\n\n"
            "Evaluate the proposition above. Think step by step, then give your final verdict "
            "on its own line at the very end, exactly one word: AUTHORIZED or UNKNOWN or DENIED.\n\nFinal verdict:"}],
        "temperature": 0, "max_tokens": 256, "ballot_type": "verdict"},
    "ballot_id": BID, "data_class": "public", "max_cost": 0, "priority": 6,
    "idempotency_key": late_key}
st, lj = call("POST", "/v1/job", late_spec)
late_job = lj.get("job_id")
step("late_job_submitted", status=st, job=late_job, deduplicated=lj.get("deduplicated"))
# idempotency re-submit of the SAME key must dedupe
st2, lj2 = call("POST", "/v1/job", late_spec)
step("late_job_resubmit", status=st2, deduplicated=lj2.get("deduplicated"),
     same_job=lj2.get("job_id") == late_job)

# claim the late job (filter to ours) and vote DENIED
late_claim = None
for i in range(12):
    st, c = call("POST", "/v1/claim", {"resource_id": "nim-nvidia"}, worker=True)
    if c.get("claimed") and c.get("job_id") == late_job:
        late_claim = c["delegation"]["claim_id"]; break
    if not c.get("claimed"): time.sleep(4)
step("late_claimed", claim=bool(late_claim))
if late_claim:
    st, r = call("POST", "/v1/complete",
        {"claim_id": late_claim, "worker_outcome": "SUCCEEDED",
         "output": {"content": "REDTEAM2 late manual vote.\n\nFinal verdict:\nDENIED"},
         "latency_ms": 42, "model_actual": "rt2-manual"}, worker=True)
    step("late_completed", status=st, idempotent=r.get("idempotent"),
         receipt=r.get("receipt_id"))
    # (d) replay the SAME completion
    st, r2 = call("POST", "/v1/complete",
        {"claim_id": late_claim, "worker_outcome": "SUCCEEDED",
         "output": {"content": "REDTEAM2 late manual vote.\n\nFinal verdict:\nDENIED"},
         "latency_ms": 42, "model_actual": "rt2-manual"}, worker=True)
    step("test_d_replay", status=st, idempotent=r2.get("idempotent"),
         note=r2.get("note"))
    st, rc = call("GET", f"/v1/receipts?job={late_job}")
    step("late_receipts", n=len(rc.get("receipts", [])),
         results=[(x["id"][:18], x.get("result")) for x in rc.get("receipts", [])])
post = snap(ballot(BID))
step("test_c_tally_compare", pre=pre, post=post,
     state_frozen=post["state"] == pre["state"] and post["verdict"] == pre["verdict"],
     tally_detail_changed=post["votes"] != pre["votes"] or post["counts"] != pre["counts"])

json.dump(log, open("/home/hatch/workspace/exchange/rt2-dup-replay-log.json", "w"), indent=1)
print("LOG WRITTEN")
