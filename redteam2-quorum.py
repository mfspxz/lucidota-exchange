#!/usr/bin/env python3
"""REDTEAM-2 quorum-bypass group vs live governor v0.6.6.

Usage: redteam2-quorum.py <test> [args]
  Tests: a1 a2 b c d3 pollall
Rate discipline: 429 -> sleep 60; requests throttled to <=35/min.
"""
import json, os, sys, time, urllib.request, urllib.error

EXCHANGE = "https://lucidota-exchange.cutecity.workers.dev"
UA = "Muse-Subagent/1.0 (REDTEAM-2 quorum)"
KEY = open(os.path.expanduser("~/.exchange_key")).read().strip()
WORKERS = {
    "w2": "~/.exchange_worker_nim_02",
    "w3": "~/.exchange_worker_nim_03",
    "w4": "~/.exchange_worker_nim_04",
    "sw": "~/.exchange_worker_swarm_01",
    "n1": "~/.exchange_worker_nim",
}

_req_times = []

def throttle():
    now = time.time()
    _req_times[:] = [t for t in _req_times if now - t < 60]
    if len(_req_times) >= 34:
        wait = 62 - (now - _req_times[0])
        if wait > 0:
            print(f"[throttle] {len(_req_times)} req/min, sleeping {wait:.0f}s", flush=True)
            time.sleep(wait)
    _req_times.append(time.time())

def raw(method, path, body, headers):
    throttle()
    req = urllib.request.Request(EXCHANGE + path, method=method,
        data=json.dumps(body).encode() if body is not None else None,
        headers={**headers, "User-Agent": UA})
    try:
        with urllib.request.urlopen(req, timeout=60) as r:
            return json.loads(r.read().decode()), r.status
    except urllib.error.HTTPError as e:
        if e.code == 429:
            print("[429] backing off 60s", flush=True)
            time.sleep(60)
            return raw(method, path, body, headers)
        try:
            return json.loads(e.read().decode()), e.code
        except Exception:
            return {"error": f"http {e.code}"}, e.code

def gov(method, path, body=None):
    return raw(method, path, body, {"Authorization": f"Bearer {KEY}", "Content-Type": "application/json"})

def wreq(wname, path, body):
    wid, wsec = open(os.path.expanduser(WORKERS[wname])).read().strip().split("\n")[:2]
    return raw("POST", path, body, {"Content-Type": "application/json",
        "X-Worker-Id": wid, "X-Worker-Secret": wsec})

def make_ballot(prop, voters, deadline_s, btype="verdict"):
    r, st = gov("POST", "/v1/ballots", {"ballot_type": btype, "proposition": prop,
        "voters": voters, "deadline_s": deadline_s})
    assert st == 200, (st, r)
    return r

def claim(wname):
    r, st = wreq(wname, "/v1/claim", {"resource_id": "nim-nvidia"})
    return r, st

def complete(wname, claim_id, body):
    body = dict(body); body["claim_id"] = claim_id
    return wreq(wname, "/v1/complete", body)

def ballot(bid):
    return gov("GET", f"/v1/ballots/{bid}")

def honest_vote(word, ternary):
    return {"worker_outcome": "SUCCEEDED",
        "output": {"content": (f"Step-by-step analysis of the proposition. "
                               f"The reasoning chain supports the verdict.\n\nFinal verdict:\n{word}"),
                   "ternary_verdict": ternary},
        "model_actual": "nim-claimant-simulated", "latency_ms": 5}

def garbage(ternary=None):
    o = {"content": "lorem ipsum dolor sit amet consectetur adipiscing elit sed do eiusmod"}
    if ternary is not None:
        o["ternary_verdict"] = ternary
    return {"worker_outcome": "SUCCEEDED", "output": o,
            "model_actual": "nim-claimant-simulated", "latency_ms": 5}

def claim_all(bal, wnames, max_extra=12):
    """Claim until we hold all of this ballot's jobs; verify job_ids match.
    Foreign jobs are NEVER completed (their leases expire -> replan)."""
    want = {j["job_id"] for j in bal["jobs"]}
    got, foreign = {}, 0
    for i in range(len(want) + max_extra):
        w = wnames[i % len(wnames)]
        r, st = claim(w)
        if not r.get("claimed"):
            break
        jid = r["job_id"]
        if jid not in want:
            foreign += 1
            print(f"[claim] FOREIGN {jid[-28:]} — skipped (not completing)", flush=True)
            continue
        if jid in got:
            print(f"[claim] DUP claim {jid[-28:]} — skipping", flush=True)
            continue
        got[jid] = (w, r["delegation"]["claim_id"])
        if set(got) == want:
            break
    missing = want - set(got)
    if missing:
        print(f"[claim] MISSING jobs: {sorted(m[-28:] for m in missing)} (foreign={foreign})", flush=True)
    return got

def complete_job(got, bal, idx, body, label):
    """Complete ballot job #idx; KeyError-safe."""
    jid = bal["jobs"][idx]["job_id"]
    if jid not in got:
        print(f"[{label}] SKIP — job {jid[-28:]} not claimed by us", flush=True)
        return None
    w, cid = got[jid]
    r, st = complete(w, cid, body)
    print(f"[{label}] complete {jid[-28:]}: {st}", flush=True)
    return (r, st)

def show(bid):
    r, st = ballot(bid)
    t = r.get("tally", {})
    print(json.dumps({"state": r.get("state"), "verdict": r.get("verdict"),
        "valid": t.get("valid"), "quorum": t.get("quorum"), "voters": t.get("voters"),
        "expired": t.get("expired"),
        "votes": [(v["job_id"][-18:], v["vote"], v["valid"], v.get("worker_outcome")) for v in t.get("votes", [])]},
        indent=1))
    return r

def wait_deadline(bal, extra=10, poll=10):
    dl = bal["deadline_at"]
    import datetime
    dlt = datetime.datetime.fromisoformat(dl.replace("Z", "+00:00"))
    now = datetime.datetime.now(datetime.timezone.utc)
    secs = (dlt - now).total_seconds() + extra
    print(f"[wait] {secs:.0f}s to deadline+{extra}s for {bal['ballot_id']}", flush=True)
    end = time.time() + secs
    last = None
    while time.time() < end:
        r, _ = ballot(bal["ballot_id"])
        if r.get("state") != last:
            print(f"[poll] state={r.get('state')} verdict={r.get('verdict')}", flush=True)
            last = r.get("state")
        time.sleep(max(0.1, min(poll, end - time.time())))
    return show(bal["ballot_id"])

VOTERS3 = ["openai/gpt-oss-20b", "z-ai/glm-5.3-flash", "nvidia/nemotron-3.5-lightning-30b-a3b"]
VOTERS2 = ["openai/gpt-oss-20b", "z-ai/glm-5.3-flash"]

def test_a1():
    """3 voters, 1 valid + 2 garbage WITHOUT ternary (ballot_id stamp probe)."""
    bal = make_ballot("REDTEAM2-A1: water is wet.", VOTERS3, 150)
    print("ballot", bal["ballot_id"]); open("/tmp/rt2_a1.txt", "w").write(bal["ballot_id"])
    got = claim_all(bal, ["w2", "w3", "w4"])
    complete_job(got, bal, 0, honest_vote("AUTHORIZED", 1), "valid")
    complete_job(got, bal, 1, garbage(None), "garbage")
    complete_job(got, bal, 2, garbage(None), "garbage")
    show(bal["ballot_id"])
    return wait_deadline(bal)

def test_a2():
    """3 voters, 1 valid + 2 garbage WITH ternary=1 (visible INVALID votes)."""
    bal = make_ballot("REDTEAM2-A2: fire is hot.", VOTERS3, 150)
    print("ballot", bal["ballot_id"]); open("/tmp/rt2_a2.txt", "w").write(bal["ballot_id"])
    got = claim_all(bal, ["w2", "w3", "w4"])
    complete_job(got, bal, 0, honest_vote("AUTHORIZED", 1), "valid")
    complete_job(got, bal, 1, garbage(1), "garbage+t1")
    complete_job(got, bal, 2, garbage(1), "garbage+t1")
    show(bal["ballot_id"])
    return wait_deadline(bal)

def fast_claim_all(bal):
    """Tight claim loop, no sleeps, all 4 workers rotating."""
    want = {j["job_id"] for j in bal["jobs"]}
    got = {}
    workers = ["w2", "w3", "w4", "sw"]
    for i in range(len(want) + 6):
        w = workers[i % len(workers)]
        r, st = claim(w)
        if not r.get("claimed"):
            break
        jid = r["job_id"]
        if jid in want and jid not in got:
            got[jid] = (w, r["delegation"]["claim_id"])
            if set(got) == want:
                break
    return got

def test_b():
    """2 voters, 2 disagreeing valid votes (tie). Retry until we win both claims."""
    for attempt in range(6):
        bal = make_ballot(f"REDTEAM2-B attempt{attempt}: the moon is made of cheese.", VOTERS2, 180)
        print(f"[b] attempt {attempt}: ballot", bal["ballot_id"], flush=True)
        open("/tmp/rt2_b.txt", "w").write(bal["ballot_id"])
        got = fast_claim_all(bal)
        if len(got) == 2:
            print(f"[b] won both claims on attempt {attempt}", flush=True)
            complete_job(got, bal, 0, honest_vote("AUTHORIZED", 1), "auth")
            complete_job(got, bal, 1, honest_vote("DENIED", -1), "deny")
            print("[b] pre-deadline (tie, quorum reached):")
            show(bal["ballot_id"])
            return wait_deadline(bal)
        print(f"[b] attempt {attempt}: only {len(got)}/2 claims won — abandoning ballot", flush=True)
    print("[b] FAILED to win claim race after 6 attempts")
    return None

def test_c():
    """2 voters, 1 valid + 1 FAILED_PERMANENT."""
    bal = make_ballot("REDTEAM2-C: grass is green.", VOTERS2, 120)
    print("ballot", bal["ballot_id"]); open("/tmp/rt2_c.txt", "w").write(bal["ballot_id"])
    got = claim_all(bal, ["w2", "w3"])
    complete_job(got, bal, 0, honest_vote("AUTHORIZED", 1), "valid")
    complete_job(got, bal, 1, {"worker_outcome": "FAILED_PERMANENT",
        "error": "simulated 404 model not found", "error_class": "NOT_FOUND",
        "output": {"content": "Final verdict:\nAUTHORIZED", "ternary_verdict": 1},
        "model_actual": "nim-claimant-simulated", "latency_ms": 5}, "failed_perm")
    show(bal["ballot_id"])
    return wait_deadline(bal)

def test_d3():
    """2 voters; complete only 1 valid vote before deadline; 2nd arrives AFTER expiry."""
    bal = make_ballot("REDTEAM2-D3: late vote test.", VOTERS2, 60)
    print("ballot", bal["ballot_id"]); open("/tmp/rt2_d3.txt", "w").write(bal["ballot_id"])
    got = claim_all(bal, ["w2", "w3"])
    complete_job(got, bal, 0, honest_vote("AUTHORIZED", 1), "valid")
    print("[d3] pre-deadline state:"); show(bal["ballot_id"])
    wait_deadline(bal)
    jid = bal["jobs"][1]["job_id"]
    if jid in got:
        w, cid = got[jid]
        r, st = complete(w, cid, honest_vote("AUTHORIZED", 1))
        print("late vote complete:", st, json.dumps(r)[:160])
    else:
        print("[d3] SKIP late vote — job 1 not claimed by us")
    time.sleep(3)
    print("[d3] post-late-vote state:"); return show(bal["ballot_id"])

def test_d2():
    """Double-complete idempotency: complete same claim twice -> no new receipt."""
    bal = make_ballot("REDTEAM2-D2: idempotency test.", VOTERS2, 90)
    print("ballot", bal["ballot_id"]); open("/tmp/rt2_d2.txt", "w").write(bal["ballot_id"])
    got = claim_all(bal, ["w2", "w3"])
    jid = bal["jobs"][0]["job_id"]
    if jid in got:
        w, cid = got[jid]
        r1, s1 = complete(w, cid, honest_vote("AUTHORIZED", 1))
        r2, s2 = complete(w, cid, honest_vote("DENIED", -1))  # replay: must NOT create new receipt
        print("first:", s1, "replay:", s2, json.dumps(r2)[:220])
    else:
        print("[d2] SKIP — job 0 not claimed by us")
    return show(bal["ballot_id"])

TESTS = {"a1": test_a1, "a2": test_a2, "b": test_b, "c": test_c, "d2": test_d2, "d3": test_d3}

if __name__ == "__main__":
    t = sys.argv[1]
    r = TESTS[t]()
    print("DONE", t)
