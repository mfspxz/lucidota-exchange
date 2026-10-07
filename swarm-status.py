#!/usr/bin/env python3
"""Swarm status: jobs by state, recent receipts, failures with reasons.

The Exchange already receipts everything; this makes it visible.
Usage: swarm-status.py [--json]
"""
import json
import sys
import urllib.request

EX = "https://lucidota-exchange.cutecity.workers.dev"
KEY = open("/home/hatch/.exchange_key").read().strip()


def get(path):
    req = urllib.request.Request(
        EX + path,
        headers={"Authorization": f"Bearer {KEY}",
                 "User-Agent": "lucidota-swarm-status/1.0"})
    return json.loads(urllib.request.urlopen(req, timeout=30).read())


def main():
    as_json = "--json" in sys.argv
    receipts = get("/v1/receipts").get("receipts", [])

    by_job = {}
    for r in receipts:
        by_job.setdefault(r["job_id"], []).append(r)

    states = {}
    failures = []
    for jid, rs in sorted(by_job.items()):
        rs.sort(key=lambda r: r.get("finished_at") or "")
        last = rs[-1]
        res = last.get("result", "?")
        states[res] = states.get(res, 0) + 1
        if res not in ("ok", "deferred"):
            try:
                dj = json.loads(last.get("decision", "{}"))
                trace = dj.get("trace", [])
                bad = [t for t in trace if not t.get("pass")]
                reason = bad[-1].get("reason", "") if bad else res
                gate = bad[-1].get("gate", "") if bad else ""
            except Exception:
                reason, gate = res, ""
            failures.append({"job": jid, "gate": gate, "reason": reason[:100]})

    # deferred-but-stuck: jobs whose last receipt is an old deferral
    stuck = [jid for jid, rs in by_job.items()
             if rs[-1].get("result") == "deferred"
             and len(rs) >= 2]

    out = {"jobs_tracked": len(by_job), "by_result": states,
           "failures": failures,
           "repeatedly_deferred": stuck}
    if as_json:
        print(json.dumps(out, indent=1))
        return
    print(f"jobs tracked: {len(by_job)}")
    for k, v in sorted(states.items()):
        print(f"  {k}: {v}")
    if failures:
        print("failures (failing right = visible with gate + reason):")
        for f in failures:
            print(f"  {f['job']} [{f['gate']}] {f['reason']}")
    else:
        print("failures: none")
    if stuck:
        print("repeatedly deferred (may need resubmit):")
        for j in stuck:
            print(f"  {j}")


if __name__ == "__main__":
    main()
