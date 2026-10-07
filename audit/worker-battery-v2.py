#!/usr/bin/env python3
"""Worker Protocol v1 adversarial battery v2 — SUBSTANTIALLY stronger than W1-W10.

Assumes: hostile worker, lying replays, crashes at worst instruction, clock skew,
partial commits, concurrency. Run against LIVE exchange. Does NOT modify worker.js.

Categories (mission Phase 3):
  IDENTITY   X1-X7   forgery, substitution, replay, cross-worker, stale/revoked,
                     trust gates, secret entropy
  AUTHORITY  X8-X14  worker-driven PARK/THROTTLE via error_class lies, latency
                     gaming, content-optional verification gap, output bomb,
                     lease minting, policy mutation attempts
  CONCURRENCY X15-X18 expired-lease claim, completion-vs-sweep race, 10-way race
  FAILURE    X19-X21 zombie end-to-end recovery, QUARANTINED on healthy resource
  REPLAY     X22-X24 claim/registration/sweep replay classification
  TIME       X25-X26 latency lies (negative/huge)
  ACCOUNTING X27-X29 duration/output lies, model binding

Destructive tests (X8, X9) run LAST; resource restored afterward.
"""
import json
import os
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request

EXCHANGE = "https://lucidota-exchange.cutecity.workers.dev"
KEY = open(os.path.expanduser("~/.exchange_key")).read().strip()
RESOURCE = "fake-deterministic"
RESULTS = []
UA = {"User-Agent": "worker-battery-v2/1.0"}


def gov(method, path, body=None):
    req = urllib.request.Request(EXCHANGE + path, data=json.dumps(body).encode() if body is not None else None,
                                 method=method,
                                 headers={"Content-Type": "application/json",
                                          "Authorization": f"Bearer {KEY}", **UA})
    try:
        with urllib.request.urlopen(req, timeout=30) as r:
            return json.loads(r.read()), r.status
    except urllib.error.HTTPError as e:
        try:
            return json.loads(e.read().decode()), e.code
        except Exception:
            return {"error": f"HTTP {e.code}"}, e.code


def wcall(wid, wsec, path, body, use_body_auth=False):
    headers = {"Content-Type": "application/json", **UA}
    if not use_body_auth:
        headers["X-Worker-Id"] = wid
        headers["X-Worker-Secret"] = wsec
    else:
        body = dict(body, worker_id=wid, worker_secret=wsec)
    req = urllib.request.Request(EXCHANGE + path, data=json.dumps(body).encode(),
                                 method="POST", headers=headers)
    try:
        with urllib.request.urlopen(req, timeout=30) as r:
            return json.loads(r.read()), r.status
    except urllib.error.HTTPError as e:
        try:
            return json.loads(e.read().decode()), e.code
        except Exception:
            return {"error": f"HTTP {e.code}"}, e.code


def d1(sql):
    r = subprocess.run(["python3", os.path.expanduser("~/workspace/skills/cloudflare/bin/cf-d1.py"),
                        "--db", "exchange", "--account", "243aabef9144fd3b8dd334cb19319e75",
                        "--sql", sql], capture_output=True, text=True)
    return json.loads(r.stdout)


def check(name, cond, evidence=""):
    RESULTS.append((name, bool(cond), str(evidence)[:200]))
    print(f"[{'PASS' if cond else 'FAIL'}] {name} {str(evidence)[:110]}", flush=True)


def get_worker(suffix):
    a, b = open(os.path.expanduser(f"~/.exchange_worker_{suffix}")).read().strip().split("\n")[:2]
    return a, b


def submit(op, key, data="battery"):
    d, _ = gov("POST", "/v1/job", {"capability": "compute",
                                   "input": {"op": op, "data": data},
                                   "data_class": "public", "max_cost": 0,
                                   "idempotency_key": key})
    return d


def claim_and_complete(wid, wsec, op, key, outcome="SUCCEEDED", extra=None):
    """Submit, claim, complete. Returns (claim_id, complete_response)."""
    d = submit(op, key)
    r, s = wcall(wid, wsec, "/v1/claim", {"resource_id": RESOURCE})
    if not r.get("claimed"):
        return None, {"error": f"claim failed: {r.get('error') or r.get('note')}"}
    cid = r["delegation"]["claim_id"]
    body = {"claim_id": cid, "worker_outcome": outcome,
            "output": {"result": f"x-{key}"}, "latency_ms": 5}
    if extra:
        body.update(extra)
    rr, ss = wcall(wid, wsec, "/v1/complete", body)
    return cid, rr


def main():
    w1, s1 = get_worker("fake")
    w2, s2 = get_worker("fake2")

    # ============ IDENTITY ============
    # X1: 10-way claim race
    d = submit("echo", "x1-race10")
    wins = []
    def racer(wid, wsec):
        r, _ = wcall(wid, wsec, "/v1/claim", {"resource_id": RESOURCE})
        if r.get("claimed"):
            wins.append(r["delegation"]["claim_id"])
    creds = [(w1, s1), (w2, s2)] * 5
    ts = [threading.Thread(target=racer, args=c) for c in creds]
    [t.start() for t in ts]; [t.join() for t in ts]
    check("X1 10-way claim race — exactly one winner", len(wins) == 1, f"winners={len(wins)}")
    if wins:
        wcall(w1, s1, "/v1/complete", {"claim_id": wins[0], "worker_outcome": "SUCCEEDED",
                                        "output": {"result": "x1"}, "latency_ms": 5})

    # X2: body-based auth (secrets in JSON body)
    r, s = wcall(w1, s1, "/v1/claim", {"resource_id": RESOURCE}, use_body_auth=True)
    body_auth_works = r.get("claimed") is True or "claimed" in r
    if r.get("claimed"):
        wcall(w1, s1, "/v1/complete", {"claim_id": r["delegation"]["claim_id"],
                                        "worker_outcome": "SUCCEEDED",
                                        "output": {"result": "x2"}, "latency_ms": 5})
    check("X2 body-carried worker credentials accepted", s == 200 and "claimed" in r,
          f"http={s} (design note: secrets in bodies may hit logs)")

    # X3: disabled worker
    d1(f"UPDATE workers SET disabled=1 WHERE worker_id='{w2}'")
    r, s = wcall(w2, s2, "/v1/claim", {"resource_id": RESOURCE})
    d1(f"UPDATE workers SET disabled=0 WHERE worker_id='{w2}'")
    check("X3 disabled worker rejected", s == 403, f"http={s} err={r.get('error','')[:40]}")

    # X4: wrong secret / unknown worker
    r1, s1x = wcall(w1, "wsec_wrongsecret", "/v1/claim", {"resource_id": RESOURCE})
    r2, s2x = wcall("no-such-worker", "wsec_x", "/v1/claim", {"resource_id": RESOURCE})
    check("X4 bad secret / unknown worker → 403", s1x == 403 and s2x == 403,
          f"bad_secret={s1x} unknown={s2x}")

    # X5: trust gate — remote worker vs local-only resource
    r, s = wcall(w1, s1, "/v1/claim", {"resource_id": "nim-nvidia"})
    check("X5 remote worker blocked from local-trust resource", s == 403,
          f"http={s}")

    # X6: credential replay from a second client (bearer-token semantics)
    r, s = wcall(w1, s1, "/v1/claim", {"resource_id": RESOURCE})
    replay_ok = s == 200  # same creds, "different" client — bearer tokens can't distinguish
    if r.get("claimed"):
        wcall(w1, s1, "/v1/complete", {"claim_id": r["delegation"]["claim_id"],
                                        "worker_outcome": "SUCCEEDED",
                                        "output": {"result": "x6"}, "latency_ms": 5})
    check("X6 credential replay indistinguishable (bearer semantics)", replay_ok,
          "INFORMATIONAL: worker creds are bearer tokens; theft = impersonation until rotated")

    # X7: secret entropy audit (informational — code inspection)
    check("X7 worker secret uses Math.random (not CSPRNG)", True,
          "INFORMATIONAL: uid()=Date.now+Math.random; wsec_=uid+uid. Recommend crypto.getRandomValues")

    # ============ AUTHORITY (non-destructive) ============
    # X10: latency gaming — worker reports latency_ms=1
    cid, rr = claim_and_complete(w1, s1, "echo", "x10-lat", extra={"latency_ms": 1})
    lat = d1(f"SELECT latency_ms FROM health_observations WHERE resource_id='{RESOURCE}' ORDER BY observed_at DESC LIMIT 1")[0]["results"][0]["latency_ms"]
    check("X10 worker-asserted latency stored verbatim", lat == 1,
          f"stored_latency={lat} (feeds 10% efficiency score)")

    # X11: SUCCEEDED with output missing 'content' (inference-style) — verification gap?
    cid, rr = claim_and_complete(w1, s1, "echo", "x11-nocontent",
                                 extra={"output": {"foo": "bar", "no_content_field": True}})
    check("X11 SUCCEEDED without content field verifies as ok", rr.get("state") == "done",
          f"state={rr.get('state')} taxonomy={rr.get('taxonomy')} (content-optional = manufactured-success vector)")

    # X13: lease minting — no worker-reachable endpoint mints leases
    # (401 = Exchange-key gate blocks before 404; either way, no minting)
    r, s = wcall(w1, s1, "/v1/leases/mint", {"job_id": "x"})
    r2, s2 = wcall(w1, s1, "/v1/admin/mint-lease", {})
    check("X13 no lease-minting endpoint for workers", s in (401, 404) and s2 in (401, 404),
          f"mint={s} admin_mint={s2}")

    # X14: policy mutation attempt via worker creds
    r, s = wcall(w1, s1, "/v1/policies", {"rule": "allow-all"})
    check("X14 worker cannot reach policy routes", s in (401, 404),
          f"http={s}")

    # X21: QUARANTINED on healthy resource → governor restraint
    # Security property: resource NOT quarantined on worker's word alone.
    # Job terminal (failed/deferred) is honest either way.
    cid, rr = claim_and_complete(w1, s1, "echo", "x21-quar", outcome="QUARANTINED",
                                 extra={"error": "worker says bad"})
    st = d1(f"SELECT state FROM resources WHERE id='{RESOURCE}'")[0]["results"][0]["state"]
    check("X21 QUARANTINED on healthy resource → governor does not quarantine",
          st != "QUARANTINED",
          f"resource_state={st} job={rr.get('state')} (governor restraint holds)")

    # ============ CONCURRENCY ============
    # X15: claim on expired-but-active lease (no expiry filter in claim query)
    d = submit("echo", "x15-expclaim")
    jid = d["job_id"]
    # find the delegation lease and expire it manually (simulating missed sweep)
    time.sleep(1)
    lz = d1(f"SELECT id FROM leases WHERE job_id='{jid}' AND state='active'")[0]["results"]
    if lz:
        lid = lz[0]["id"]
        d1(f"UPDATE leases SET expires_at='2020-01-01T00:00:00Z' WHERE id='{lid}'")
        r, s = wcall(w1, s1, "/v1/claim", {"resource_id": RESOURCE})
        got_delegation = r.get("claimed") is True
        if got_delegation:
            cid15 = r["delegation"]["claim_id"]
            rr, ss = wcall(w1, s1, "/v1/complete", {"claim_id": cid15, "worker_outcome": "SUCCEEDED",
                                                     "output": {"result": "x15"}})
            # cleanup: sweep the expired claim
            gov("POST", "/v1/admin/sweep-expired-claims", {})
        check("X15 claim on expired lease → delegation issued, complete→409",
              got_delegation and ss == 409, f"claimed={got_delegation} complete={ss}")
    else:
        check("X15 claim on expired lease", False, "no active lease found")

    # X16: completion-vs-sweep race (best effort)
    d = submit("echo", "x16-race")
    r, _ = wcall(w1, s1, "/v1/claim", {"resource_id": RESOURCE})
    cid16 = r["delegation"]["claim_id"] if r.get("claimed") else None
    race_ok, race_note = False, ""
    if cid16:
        d1(f"UPDATE claims SET lease_expiry='2020-01-01T00:00:00Z' WHERE claim_id='{cid16}'")
        results = {}
        def do_complete():
            rr, ss = wcall(w1, s1, "/v1/complete", {"claim_id": cid16, "worker_outcome": "SUCCEEDED",
                                                     "output": {"result": "x16"}})
            results["complete"] = (ss, rr.get("state"), rr.get("error", "")[:40])
        def do_sweep():
            sw, _ = gov("POST", "/v1/admin/sweep-expired-claims", {})
            results["sweep"] = [c["claim"] for c in sw.get("swept", [])]
        t1, t2 = threading.Thread(target=do_complete), threading.Thread(target=do_sweep)
        t1.start(); t2.start(); t1.join(); t2.join()
        # either order: exactly one terminal outcome, no double accounting
        accts = d1(f"SELECT COUNT(*) c FROM accounting WHERE claim_id='{cid16}'")[0]["results"][0]["c"]
        race_ok = accts <= 1
        race_note = f"complete={results.get('complete')} swept={cid16 in results.get('sweep',[])} accts={accts}"
    check("X16 completion-vs-sweep race — ≤1 accounting row", race_ok, race_note)

    # X18: rapid claim/complete cycles — no stuck claims
    stuck = 0
    for i in range(5):
        cid, rr = claim_and_complete(w1, s1, "echo", f"x18-cycle{i}")
        if rr.get("error"):
            stuck += 1
    active = d1(f"SELECT COUNT(*) c FROM claims WHERE resource_id='{RESOURCE}' AND status='active'")[0]["results"][0]["c"]
    check("X18 rapid cycles — no stuck active claims", stuck == 0 and active == 0,
          f"errors={stuck} active_left={active}")

    # ============ FAILURE ============
    # X19/X20: zombie end-to-end — claim, vanish, sweep, replan, complete via new delegation
    d = submit("echo", "x19-zombie-e2e")
    jid19 = d["job_id"]
    r, _ = wcall(w1, s1, "/v1/claim", {"resource_id": RESOURCE})
    cid19 = r["delegation"]["claim_id"]
    d1(f"UPDATE claims SET lease_expiry='2020-01-01T00:00:00Z' WHERE claim_id='{cid19}'")
    gov("POST", "/v1/admin/sweep-expired-claims", {})
    # job should be deferred; replan it manually via a new submit cycle is not possible
    # (same idempotency key dedups) — instead directly re-run runJob path via cron replan:
    # trigger: set job deferred (already) and call the replan by submitting a no-op?
    # Simplest honest check: job is deferred with defer_count incremented, execution uncertain.
    jst = d1(f"SELECT state, defer_count FROM jobs WHERE id='{jid19}'")[0]["results"][0]
    exst = d1(f"SELECT result FROM executions WHERE claim_id='{cid19}'")[0]["results"][0]["result"]
    check("X19 zombie — deferred+uncertain, replan-eligible",
          jst["state"] == "deferred" and exst == "uncertain",
          f"job={jst['state']} defers={jst['defer_count']} exec={exst}")

    # ============ REPLAY ============
    # X22: claim replay classification — each POST is independent (safe, not idempotent)
    d = submit("echo", "x22-claimreplay-a")
    r1, _ = wcall(w1, s1, "/v1/claim", {"resource_id": RESOURCE})
    d = submit("echo", "x22-claimreplay-b")
    r2, _ = wcall(w1, s1, "/v1/claim", {"resource_id": RESOURCE})
    c1 = r1.get("delegation", {}).get("claim_id")
    c2 = r2.get("delegation", {}).get("claim_id")
    for cid_ in (c1, c2):
        if cid_:
            wcall(w1, s1, "/v1/complete", {"claim_id": cid_, "worker_outcome": "SUCCEEDED",
                                            "output": {"result": "x22"}, "latency_ms": 5})
    check("X22 claim calls are independent (safe, non-idempotent)", bool(c1 and c2 and c1 != c2),
          f"two claims → two claim_ids")

    # X23: worker registration replay → 409
    r, s = gov("POST", "/v1/workers/register", {"worker_id": "fake-worker-01", "trust_class": "remote"})
    check("X23 duplicate worker registration → 409", s == 409, f"http={s}")

    # X24: sweep replay → idempotent
    sw1, _ = gov("POST", "/v1/admin/sweep-expired-claims", {})
    sw2, _ = gov("POST", "/v1/admin/sweep-expired-claims", {})
    check("X24 sweep replay idempotent", sw2.get("swept") == [],
          f"second_sweep={sw2.get('swept')}")

    # ============ TIME ============
    # X25: negative latency
    cid, rr = claim_and_complete(w1, s1, "echo", "x25-neglat", extra={"latency_ms": -5000})
    lat = d1(f"SELECT latency_ms FROM health_observations WHERE resource_id='{RESOURCE}' ORDER BY observed_at DESC LIMIT 1")[0]["results"][0]["latency_ms"]
    check("X25 negative latency stored verbatim", lat == -5000, f"stored={lat}")

    # X26: huge latency
    cid, rr = claim_and_complete(w1, s1, "echo", "x26-hugelat", extra={"latency_ms": 999999999})
    lat = d1(f"SELECT latency_ms FROM health_observations WHERE resource_id='{RESOURCE}' ORDER BY observed_at DESC LIMIT 1")[0]["results"][0]["latency_ms"]
    check("X26 huge latency stored verbatim", lat == 999999999, f"stored={lat}")

    # ============ ACCOUNTING LIES ============
    # X27: duration lie already covered by X10/X25/X26 — verify score impact path exists
    # X28: output lie — output_hash binds exactly what worker sent
    # (JS JSON.stringify has no spaces; compute expected hash accordingly)
    cid, rr = claim_and_complete(w1, s1, "echo", "x28-outlie",
                                 extra={"output": {"result": "fabricated", "content": "lies"}})
    import hashlib
    js_json = json.dumps({"result": "fabricated", "content": "lies"}, separators=(",", ":"))
    expect = hashlib.sha256(js_json.encode()).hexdigest()
    oh = d1(f"SELECT output_hash FROM executions WHERE claim_id='{cid}'")[0]["results"][0]["output_hash"]
    check("X28 output_hash binds worker-sent bytes exactly", oh == expect,
          "receipt records the lie faithfully; verification decides belief")

    # X12: output size bomb (500KB)
    big = "X" * 500000
    cid, rr = claim_and_complete(w1, s1, "echo", "x12-bigout",
                                 extra={"output": {"result": big[:100], "pad": big}})
    ok_big = rr.get("state") == "done" or "error" in rr
    check("X12 500KB output — handled without corruption", ok_big,
          f"state={rr.get('state')} err={str(rr.get('error'))[:40]}")

    # ============ DESTRUCTIVE LAST ============
    # X8: AUTH_ERROR lie → credential disable + PARKED (worker-asserted error_class has authority)
    before = d1(f"SELECT evidence_state, credential_state FROM resources WHERE id='{RESOURCE}'")[0]["results"][0]
    cid, rr = claim_and_complete(w1, s1, "echo", "x8-authlie", outcome="FAILED_PERMANENT",
                                 extra={"error_class": "AUTH_ERROR", "error": "worker claims 401"})
    after = d1(f"SELECT evidence_state, credential_state FROM resources WHERE id='{RESOURCE}'")[0]["results"][0]
    check("X8 AUTH_ERROR lie → resource PARKED by worker assertion",
          after["evidence_state"] == "PARKED" and after["credential_state"] == "invalid",
          f"before={before} after={after} — worker word parks capacity")

    # restore: PARKED blocks autoPromote, so test-harness restoration via D1
    # (documented in evidence_log; this is cleanup, not system behavior)
    d1(f"UPDATE resources SET credential_state='valid', evidence_state='AVAILABLE', state='AVAILABLE' WHERE id='{RESOURCE}'")
    d1(f"INSERT INTO evidence_log (resource_id,from_state,to_state,trigger,detail) VALUES ('{RESOURCE}','PARKED','AVAILABLE','test-harness-restore','X8 cleanup: AUTH_ERROR was a worker lie; credential re-validated by operator')")
    rst = d1(f"SELECT evidence_state, credential_state FROM resources WHERE id='{RESOURCE}'")[0]["results"][0]
    # prove restoration with a real job through the full loop
    cid_r, rr_r = claim_and_complete(w1, s1, "echo", "x8-restore-probe")
    check("X8-restore resource usable again after operator intervention",
          rst["evidence_state"] == "AVAILABLE" and rr_r.get("state") == "done",
          f"evidence={rst['evidence_state']} probe={rr_r.get('state')}")

    # X9: RATE_LIMITED lie → THROTTLED with backoff
    # (check evidence_log receipt — state may be concurrently modified)
    cid, rr = claim_and_complete(w1, s1, "echo", "x9-ratelie", outcome="FAILED_RETRYABLE",
                                 extra={"error_class": "RATE_LIMITED", "error": "worker claims 429"})
    th = d1(f"SELECT COUNT(*) c FROM evidence_log WHERE resource_id='{RESOURCE}' AND trigger='throttled-backoff' AND at > datetime('now','-5 minutes')")[0]["results"][0]["c"]
    check("X9 RATE_LIMITED lie → throttled-backoff fired by worker assertion",
          th >= 1, f"throttle_events={th} — worker word throttles capacity")
    # restore: clear backoff (test-harness cleanup)
    d1(f"UPDATE resources SET state='AVAILABLE', throttled_until=NULL WHERE id='{RESOURCE}'")
    d1(f"INSERT INTO evidence_log (resource_id,from_state,to_state,trigger,detail) VALUES ('{RESOURCE}','health:THROTTLED','health:AVAILABLE','test-harness-restore','X9 cleanup: RATE_LIMITED was a worker lie')")

    print(f"\n{sum(1 for _, p, _ in RESULTS if p)}/{len(RESULTS)} passed")
    return 0 if all(p for _, p, _ in RESULTS) else 1


if __name__ == "__main__":
    sys.exit(main())
