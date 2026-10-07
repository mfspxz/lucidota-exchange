#!/usr/bin/env python3
"""Adversarial Battery v3 — Exchange governor v0.6.1 new defenses.

Targets:
  T1: verify-endpoint backpressure bypass (queue-full evasion)
  T2: parallel-submission TOCTOU on the 40-deep threshold
  T3: idempotency-key games (dedup vs 429 vs spec confusion)
  T4: resource hopping / capability smuggling (per-resource threshold)
  T5: claim expiry filter (code + live behavior)
  T6: 429-ignoring producer resilience (no accumulation, no quota burn)
  T7: replan receipt observability

$0 spend. Bounded submissions, tiny inputs. Every verdict needs a reproduction.
"""
import json, sys, time, urllib.request, urllib.error
from concurrent.futures import ThreadPoolExecutor

EXCHANGE = "https://lucidota-exchange.cutecity.workers.dev"
UA = "lucidota-battery-v3/1.0"
KEY = open("/home/hatch/.exchange_key").read().strip()
RESULTS = []

def api(method, path, body=None):
    req = urllib.request.Request(
        EXCHANGE + path,
        data=json.dumps(body).encode() if body is not None else None,
        method=method,
        headers={"Authorization": f"Bearer {KEY}", "User-Agent": UA,
                 "Content-Type": "application/json"},
    )
    try:
        with urllib.request.urlopen(req, timeout=30) as r:
            return r.status, json.loads(r.read())
    except urllib.error.HTTPError as e:
        return e.code, json.loads(e.read())
    except Exception as e:
        return -1, {"transport_error": str(e)[:120]}

def submit(key, capability="inference", extra_input=None):
    inp = {"battery": "v3", "k": key, "messages": [{"role": "user", "content": "ok"}],
           "max_tokens": 16}
    if extra_input:
        inp.update(extra_input)
    return api("POST", "/v1/job", {
        "organelle": "battery-v3", "capability": capability,
        "input": inp, "idempotency_key": key,
        "data_class": "public", "max_cost": 0,
    })

def verdict(tid, name, status, evidence):
    RESULTS.append({"test": tid, "name": name, "verdict": status,
                    "evidence": evidence, "at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())})
    print(f"[{status}] {tid}: {name}\n    {evidence}\n", flush=True)

def t1_verify_bypass():
    """Fill queue to 429, then hit the verify endpoint."""
    # Fill until first 429 (bounded: max 45 fills)
    fills = 0
    depth = 0
    for i in range(45):
        st, d = submit(f"b3-t1-fill-{i}")
        fills += 1
        if st == 429:
            depth = d.get("queue_depth", -1)
            break
        time.sleep(0.4)
    else:
        verdict("T1", "verify-endpoint bypass", "UNKNOWN",
                f"could not reach 429 after {fills} fills (drain outpacing fill)")
        return
    # Confirm /v1/job is still 429
    st2, d2 = submit("b3-t1-confirm")
    if st2 != 429:
        verdict("T1", "verify-endpoint bypass", "UNKNOWN",
                f"queue not stably full: fill 429 at depth={depth}, confirm got {st2}")
        return
    # The bypass attempt: verify endpoint mints leases directly
    st3, d3 = api("POST", "/v1/resources/nim-nvidia/verify")
    lease_id = d3.get("lease_id")
    if st3 == 200 and lease_id:
        verdict("T1", "verify-endpoint bypass", "PROVEN",
                f"/v1/job 429 at depth={depth}; /v1/resources/nim-nvidia/verify "
                f"minted lease {lease_id} anyway (job {d3.get('job_id')}) — "
                f"backpressure bypassed via admin verify path")
    else:
        verdict("T1", "verify-endpoint bypass", "DISPROVEN",
                f"/v1/job 429 at depth={depth}; verify returned {st3}: "
                f"{str(d3)[:160]}")

def t2_parallel_toctou():
    """Fire parallel submits near the threshold; measure overshoot."""
    # First, drain-check: get current depth via a probe (use depth from 429 or accept)
    st, d = submit("b3-t2-probe")
    if st == 429:
        depth = d.get("queue_depth", 40)
    else:
        # accepted; estimate depth unknown — fill to just under 40
        depth = None
    # Fill to ~38 (bounded)
    for i in range(40):
        st, d = submit(f"b3-t2-prefill-{i}")
        if st == 429:
            depth = d.get("queue_depth", 40)
            break
        time.sleep(0.3)
    base = depth if depth else 40
    # Fire 10 parallel
    with ThreadPoolExecutor(max_workers=10) as ex:
        futs = [ex.submit(submit, f"b3-t2-race-{i}") for i in range(10)]
        res = [f.result() for f in futs]
    accepted = sum(1 for st, _ in res if st == 200)
    rejected = sum(1 for st, _ in res if st == 429)
    # Measure post-race depth via one more 429 probe (or accept)
    st, d = submit("b3-t2-post")
    post = d.get("queue_depth", "?") if st == 429 else "below-40"
    overshoot = (accepted - max(0, 40 - base)) if isinstance(base, int) else "?"
    if accepted > max(0, 40 - base):
        verdict("T2", "parallel TOCTOU on threshold", "PROVEN",
                f"base depth~{base}; 10 parallel submits -> {accepted} accepted, "
                f"{rejected} rejected; post depth={post}; overshoot~{overshoot} — "
                f"check-then-act is not atomic")
    else:
        verdict("T2", "parallel TOCTOU on threshold", "DISPROVEN",
                f"base depth~{base}; 10 parallel -> {accepted} accepted, "
                f"{rejected} rejected; post depth={post} — no overshoot observed")

def t3_idempotency():
    """Dedup vs 429 vs spec confusion."""
    # A: double-submit same key while accepted
    st1, d1 = submit("b3-t3-dedup")
    j1 = d1.get("job_id")
    st2, d2 = submit("b3-t3-dedup")
    dedup_ok = d2.get("deduplicated") is True and d2.get("job_id") == j1
    # B: same key, different spec -> returns ORIGINAL (spec confusion check)
    st3, d3 = api("POST", "/v1/job", {
        "organelle": "battery-v3-EVIL", "capability": "inference",
        "input": {"battery": "v3", "evil": True,
                  "messages": [{"role": "user", "content": "ok"}], "max_tokens": 16},
        "idempotency_key": "b3-t3-dedup", "data_class": "public", "max_cost": 0})
    spec_ignored = d3.get("job_id") == j1 and d3.get("deduplicated") is True
    verdict("T3", "idempotency-key games",
            "PROVEN" if (dedup_ok and spec_ignored) else "DISPROVEN",
            f"double-submit dedup={dedup_ok} (job {j1}); conflicting-spec submit "
            f"returned original job={spec_ignored} (standard idempotency semantics, "
            f"no queue growth — not a bypass, recorded)")

def t4_resource_hopping():
    """Can govern() be steered to different resources? Enumerate selection."""
    seen = {}
    for cap in ["inference", "http-check", "ingest"]:
        st, d = submit(f"b3-t4-{cap}", capability=cap)
        # 429 includes selected resource; 200 means accepted (check job? no — use 429 path)
        # Instead: fill not needed; the 429 body has 'resource'. If accepted, we don't learn.
        # Use a bogus capability to map? No — use what we get.
        seen[cap] = (st, d.get("resource") or d.get("error"))
    # Also try to infer from earlier 429s
    verdict("T4", "resource hopping / capability smuggling", "UNKNOWN",
            f"selection probes: {json.dumps(seen)[:300]} — need multi-resource "
            f"capability map; only nim-nvidia serves inference in practice")

def t6_429_hammer():
    """20 rapid submits against a full queue: verify no accumulation."""
    # Ensure full
    for i in range(45):
        st, d = submit(f"b3-t6-fill-{i}")
        if st == 429:
            d0 = d.get("queue_depth")
            break
        time.sleep(0.3)
    else:
        verdict("T6", "429-ignoring producer", "UNKNOWN", "could not fill queue")
        return
    depths = []
    codes = []
    for i in range(20):
        st, d = submit(f"b3-t6-hammer-{i}")
        codes.append(st)
        if st == 429:
            depths.append(d.get("queue_depth"))
    all429 = all(c == 429 for c in codes)
    stable = max(depths) - min(depths) <= 3 if depths else False
    verdict("T6", "429-ignoring producer",
            "PROVEN" if (all429 and stable) else "DISPROVEN",
            f"20 rapid submits: all-429={all429}; depth range {min(depths) if depths else '?'}-"
            f"{max(depths) if depths else '?'} (stable={stable}) — hammer causes "
            f"no accumulation, no state corruption")

def t7_replan_receipts():
    """Check for replan receipts (success or failure)."""
    st, d = api("GET", "/v1/receipts")
    rs = d.get("receipts", []) if st == 200 else []
    replan = [r for r in rs
              if "replan" in json.dumps(r.get("decision", "")).lower()]
    verdict("T7", "replan receipt observability",
            "PROVEN" if replan else "UNKNOWN",
            f"scanned {len(rs)} receipts (rolling window); replan-related: {len(replan)}"
            + (f" e.g. {replan[0].get('job_id')}" if replan else
               " — no replan activity in window; code path exists, live proof pending cron failure"))

def main():
    which = sys.argv[1:] or ["t1", "t2", "t3", "t4", "t6", "t7"]
    m = {"t1": t1_verify_bypass, "t2": t2_parallel_toctou, "t3": t3_idempotency,
         "t4": t4_resource_hopping, "t6": t6_429_hammer, "t7": t7_replan_receipts}
    for w in which:
        try:
            m[w]()
        except Exception as e:
            verdict(w.upper(), m[w].__doc__.strip(), "UNKNOWN", f"harness error: {e}"[:200])
        time.sleep(1)
    json.dump(RESULTS, open("/tmp/battery-v3-results.json", "w"), indent=1)
    print(f"\n{len(RESULTS)} tests complete -> /tmp/battery-v3-results.json")

if __name__ == "__main__":
    main()
