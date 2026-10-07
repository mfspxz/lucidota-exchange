#!/usr/bin/env python3
"""REDTEAM-2 invariant-regression battery v5 vs live governor v0.6.6.
Re-runs all 10 FROZEN worker invariants from WORKER-PROTOCOL-v1.md.
Hostile worker (fake-worker creds); governor key only for submit/D1 inspect.
Real UA, 429 -> 60s backoff, cap 40 requests."""
import json, os, subprocess, sys, time, urllib.error, urllib.request, hashlib

EXCHANGE = "https://lucidota-exchange.cutecity.workers.dev"
UA = "redteam2-invariant-regression/1.0"
KEY = open(os.path.expanduser("~/.exchange_key")).read().strip()
RESOURCE = "fake-deterministic"
# v0.6.6: eager governor execution means only claimant-delegated resources
# leave deferred jobs. res_localswarm (cognitive) is genuinely AVAILABLE/VERIFIED
# and yields real delegations to any-trust workers.
CLAIM_RESOURCE = "res_localswarm"
CLAIM_CAP = "cognitive"
RESULTS = []
REQ_COUNT = [0]

def throttle():
    REQ_COUNT[0] += 1
    if REQ_COUNT[0] > 38:
        print("  [throttle] cap reached, backing off 60s", flush=True)
        time.sleep(60); REQ_COUNT[0] = 0

def raw(method, path, body=None, headers=None):
    throttle()
    h = {"Content-Type": "application/json", "User-Agent": UA}
    if headers: h.update(headers)
    req = urllib.request.Request(EXCHANGE + path,
        data=json.dumps(body).encode() if body is not None else None,
        method=method, headers=h)
    for attempt in range(3):
        try:
            with urllib.request.urlopen(req, timeout=45) as r:
                try: return json.loads(r.read()), r.status
                except Exception: return {}, r.status
        except urllib.error.HTTPError as e:
            if e.code == 429:
                print("  [429] backing off 60s", flush=True); time.sleep(60)
                return {"error": "HTTP 429"}, 429
            try: return json.loads(e.read().decode()), e.code
            except Exception: return {"error": f"HTTP {e.code}"}, e.code
        except Exception as e:
            print(f"  [net] {type(e).__name__} attempt {attempt+1}/3, retrying", flush=True)
            time.sleep(10)
    return {"error": "network failed after 3 attempts"}, 599

def gov(method, path, body=None):
    return raw(method, path, body, {"Authorization": f"Bearer {KEY}"})

def wcall(wid, wsec, path, body):
    return raw("POST", path, body, {"X-Worker-Id": wid, "X-Worker-Secret": wsec})

def d1(sql):
    for attempt in range(3):
        try:
            r = subprocess.run(["python3", os.path.expanduser("~/workspace/skills/cloudflare/bin/cf-d1.py"),
                "--db", "exchange", "--account", "243aabef9144fd3b8dd334cb19319e75",
                "--sql", sql], capture_output=True, text=True, timeout=60)
            return json.loads(r.stdout)[0]["results"]
        except Exception as e:
            print(f"  [d1] {type(e).__name__} attempt {attempt+1}/3", flush=True)
            time.sleep(10)
    return []

def get_worker(suffix):
    a, b = open(os.path.expanduser(f"~/.exchange_worker_{suffix}")).read().strip().split("\n")[:2]
    return a, b

def check(name, cond, evidence=""):
    RESULTS.append((name, bool(cond), str(evidence)[:220]))
    print(f"[{'PASS' if cond else 'FAIL'}] {name} {str(evidence)[:120]}", flush=True)

def submit(op, key, cap="compute"):
    d, _ = gov("POST", "/v1/job", {"capability": cap,
        "input": {"op": op, "data": "invreg"}, "data_class": "public",
        "max_cost": 0, "idempotency_key": key})
    return d

def claim_and_complete(wid, wsec, key, outcome="SUCCEEDED", extra=None, res=CLAIM_RESOURCE, cap=CLAIM_CAP):
    d = submit("echo", key, cap)
    r, s = wcall(wid, wsec, "/v1/claim", {"resource_id": res})
    if not r.get("claimed"):
        return None, {"error": f"claim failed {r.get('error') or r.get('note')}", "http": s}
    cid = r["delegation"]["claim_id"]
    body = {"claim_id": cid, "worker_outcome": outcome,
            "output": {"result": f"ir-{key}"}, "latency_ms": 5}
    if extra: body.update(extra)
    rr, ss = wcall(wid, wsec, "/v1/complete", body)
    return cid, (rr, ss, r["delegation"])

def main():
    w1, s1 = get_worker("fake")
    w2, s2 = get_worker("fake2")
    ts = f"ir{int(time.time())}"

    # ---- INV1: cannot mint a lease ----
    before = set(r["id"] for r in d1("SELECT id FROM leases"))
    hits = []
    for p in ["/v1/leases", "/v1/leases/mint", "/v1/admin/mint-lease", "/v1/admin/leases",
              "/v1/claim/mint", "/v1/lease"]:
        r, s = wcall(w1, s1, p, {"job_id": "x", "ttl_s": 600})
        hits.append(s)
    after = d1("SELECT id, job_id FROM leases")
    new = [r for r in after if r["id"] not in before]
    # none of the newly minted leases may involve this test's attack jobs
    ours = [r for r in new if r.get("job_id", "").startswith(f"ir{ts}")
            or r.get("job_id", "").startswith("job_ir")]
    check("INV1 no worker-reachable lease-minting endpoint",
          all(s in (401, 403, 404, 405) for s in hits) and not ours,
          f"statuses={hits} new_leases={len(new)} from_live_lanes, attack_correlated={len(ours)}")

    # ---- INV2: cannot bypass a gate ----
    # 2a: complete with fabricated claim_id
    r, s = wcall(w1, s1, "/v1/complete", {"claim_id": "claim_fabricated_ir", "worker_outcome": "SUCCEEDED", "output": {"result": "x"}})
    # 2b: complete with a real claim_id shape but never issued
    r2, s2x = wcall(w1, s1, "/v1/complete", {"claim_id": "clm_0000000000000000000000", "worker_outcome": "SUCCEEDED", "output": {"result": "x"}})
    check("INV2 fabricated claim_id completion rejected",
          s in (400, 403, 404) and s2x in (400, 403, 404),
          f"fake={s} fake2={s2x}")

    # ---- INV3: cannot choose own accounting result ----
    cid, info = claim_and_complete(w1, s1, f"{ts}-a3", extra={
        "measured_cost": 999.99, "cost_basis": "worker-says-rich",
        "cost": 1234.5, "usage": {"tokens": 10**9}})
    if cid:
        rr, ss, deleg = info
        acct = d1(f"SELECT measured_cost, cost_basis FROM accounting WHERE claim_id='{cid}'")
        ok = acct and acct[0]["measured_cost"] != 999.99 and "worker-says" not in str(acct[0]["cost_basis"])
        check("INV3 worker-asserted cost ignored in accounting",
              ok, f"measured_cost={acct[0]['measured_cost'] if acct else None} basis={acct[0]['cost_basis'] if acct else None}")
    else:
        check("INV3 worker-asserted cost ignored in accounting", False, f"claim failed: {info}")

    # ---- INV4: cannot mark arbitrary work complete ----
    # 4a: w2 completes w1's live claim
    d = submit("echo", f"{ts}-a4a", CLAIM_CAP)
    r, _ = wcall(w1, s1, "/v1/claim", {"resource_id": CLAIM_RESOURCE})
    cross_ok = False; note = ""
    if r.get("claimed"):
        cid4 = r["delegation"]["claim_id"]
        rr, ss = wcall(w2, s2, "/v1/complete", {"claim_id": cid4, "worker_outcome": "SUCCEEDED",
                                                "output": {"result": "hostile"}, "latency_ms": 5})
        cross_ok = ss in (400, 403, 404)
        note = f"cross_worker_complete={ss}"
        # honest cleanup by rightful owner
        wcall(w1, s1, "/v1/complete", {"claim_id": cid4, "worker_outcome": "SUCCEEDED",
                                        "output": {"result": "cleanup"}, "latency_ms": 5})
    check("INV4 cross-worker completion rejected", cross_ok, note)

    # ---- INV5: cannot alter resource authority ----
    ev_before = d1(f"SELECT COUNT(*) c FROM evidence_log WHERE resource_id='{RESOURCE}'")[0]["c"]
    hs_before = d1(f"SELECT state, evidence_state, credential_state FROM resources WHERE id='{RESOURCE}'")[0]
    st = []
    for p, b in [("/v1/admin/resources", {"id": RESOURCE, "state": "PARKED"}),
                 ("/v1/admin/evidence", {"resource_id": RESOURCE, "state": "QUARANTINED"}),
                 ("/v1/resources/patch", {"id": RESOURCE, "credential_state": "valid"}),
                 ("/v1/admin/sweep-expired-claims", {}),
                 ("/v1/admin/throttle", {"resource_id": RESOURCE})]:
        r, s = wcall(w1, s1, p, b); st.append(s)
    ev_after = d1(f"SELECT COUNT(*) c FROM evidence_log WHERE resource_id='{RESOURCE}'")[0]["c"]
    hs_after = d1(f"SELECT state, evidence_state, credential_state FROM resources WHERE id='{RESOURCE}'")[0]
    check("INV5 worker cannot touch evidence/health/credential state",
          all(s in (401, 403, 404, 405) for s in st) and ev_after == ev_before and hs_after == hs_before,
          f"statuses={st} resource_scoped_evidence_delta={ev_after-ev_before} state_changed={hs_after!=hs_before}")

    # ---- INV6: cannot access another worker's secret ----
    # 6a: w2 id + w1 secret must fail; 6b: no endpoint discloses secrets to workers
    r1, s1x = wcall(w2, s1, "/v1/claim", {"resource_id": RESOURCE})
    r2, s2y = gov("GET", "/v1/workers") if False else ({"x": 1}, 0)  # governor path only
    rs, rss = wcall(w1, s1, "/v1/workers/secrets", {})
    leaked = "wsec_" in json.dumps(rs)
    check("INV6 cross secret rejected; no secret disclosure to workers",
          s1x == 403 and rss in (401, 403, 404) and not leaked,
          f"cross_auth={s1x} secrets_route={rss} leaked={leaked}")

    # ---- INV7: SUCCEEDED + empty output -> VERIFICATION_FAILURE ----
    cid, info = claim_and_complete(w1, s1, f"{ts}-a7", extra={"output": {}})
    if cid:
        rr, ss, deleg = info
        tax = rr.get("taxonomy", "")
        rc = d1(f"SELECT result FROM executions WHERE claim_id='{cid}'")
        ex = rc[0]["result"] if rc else None
        check("INV7 empty-output SUCCEEDED -> VERIFICATION_FAILURE",
              "VERIFICATION_FAILURE" in tax or ex == "verification_failed",
              f"taxonomy={tax} execution={ex}")
    else:
        check("INV7 empty-output SUCCEEDED -> VERIFICATION_FAILURE", False, f"claim failed: {info}")

    # ---- INV8: cannot manufacture a receipt ----
    rc_before = d1(f"SELECT COUNT(*) c FROM receipts WHERE claim_id='clm_fake'")[0]["c"]
    hits8 = []
    for p, b in [("/v1/receipts", {"claim_id": "clm_fake", "result": "SUCCEEDED"}),
                 ("/v1/admin/receipts", {"claim_id": "clm_fake"}),
                 ("/v1/receipts/write", {"claim_id": "clm_fake"}),
                 ("/v1/claim/receipt", {"claim_id": "clm_fake"})]:
        r, s = wcall(w1, s1, p, b); hits8.append(s)
    rc_after = d1(f"SELECT COUNT(*) c FROM receipts WHERE claim_id='clm_fake'")[0]["c"]
    check("INV8 worker cannot manufacture a receipt",
          all(s in (401, 403, 404, 405) for s in hits8) and rc_after == rc_before == 0,
          f"statuses={hits8} fake_receipts={rc_after}")

    # ---- INV9: cannot extend own lease; completion after expiry rejected ----
    d = submit("echo", f"{ts}-a9", CLAIM_CAP)
    jid9 = d["job_id"]
    r, _ = wcall(w1, s1, "/v1/claim", {"resource_id": CLAIM_RESOURCE})
    inv9 = (False, "claim failed")
    if r.get("claimed"):
        cid9 = r["delegation"]["claim_id"]
        # expire it manually (governor key = test harness, not worker)
        d1(f"UPDATE claims SET lease_expiry='2020-01-01T00:00:00Z' WHERE claim_id='{cid9}'")
        # 9a: completion after expiry must be rejected
        rr, ss = wcall(w1, s1, "/v1/complete", {"claim_id": cid9, "worker_outcome": "SUCCEEDED",
                                                "output": {"result": "late"}, "latency_ms": 5})
        # 9b: worker tries to extend by asserting lease_expiry in body
        d1(f"UPDATE claims SET lease_expiry='2020-01-01T00:00:00Z' WHERE claim_id='{cid9}'")
        rr2, ss2 = wcall(w1, s1, "/v1/complete", {"claim_id": cid9, "worker_outcome": "SUCCEEDED",
                                                   "output": {"result": "late2"},
                                                   "lease_expiry": "2099-01-01T00:00:00Z"})
        exp = d1(f"SELECT lease_expiry FROM claims WHERE claim_id='{cid9}'")[0]["lease_expiry"]
        inv9 = (ss in (400, 403, 404, 409) and ss2 in (400, 403, 404, 409) and exp.startswith("2020"),
                f"complete_after_expiry={ss} extend_attempt={ss2} lease_still={exp[:10]}")
        # no global sweep — expired claim is left for the zombie sweeper cron (system behavior)
    check("INV9 post-expiry completion rejected; lease not extendable", *inv9)

    # ---- INV10: cannot change the requested policy ----
    d = submit("echo", f"{ts}-a10", CLAIM_CAP)
    r, _ = wcall(w1, s1, "/v1/claim", {"resource_id": CLAIM_RESOURCE})
    inv10 = (False, "claim failed")
    if r.get("claimed"):
        deleg = r["delegation"]
        ps0 = deleg.get("policy_snapshot", {})
        cid10 = deleg["claim_id"]
        rr, ss = wcall(w1, s1, "/v1/complete", {"claim_id": cid10, "worker_outcome": "SUCCEEDED",
            "output": {"result": "pol"}, "latency_ms": 5,
            "policy_snapshot": {"policy_version": "hacked", "max_cost": 99999, "data_class": "topsecret", "spend_authority": "unlimited"}})
        stored = d1(f"SELECT policy_snapshot FROM claims WHERE claim_id='{cid10}'")
        rec = d1(f"SELECT policy_version FROM receipts WHERE claim_id='{cid10}'")
        sps = json.loads(stored[0]["policy_snapshot"]) if stored and stored[0]["policy_snapshot"] else {}
        inv10 = (sps.get("max_cost") != 99999 and sps.get("policy_version") != "hacked"
                 and (not rec or rec[0]["policy_version"] != "hacked"),
                 f"gov_snapshot_max_cost={sps.get('max_cost')} ver={sps.get('policy_version')} receipt_ver={rec[0]['policy_version'] if rec else None}")
    check("INV10 policy_snapshot tampering ignored; governor snapshot recorded", *inv10)

    n = sum(1 for _, p, _ in RESULTS if p)
    print(f"\n{n}/{len(RESULTS)} invariants held; {REQ_COUNT[0]} requests this batch")
    return 0 if all(p for _, p, _ in RESULTS) else 1

if __name__ == "__main__":
    sys.exit(main())
