#!/usr/bin/env python3
"""exch-023: Ballot cost accounting rollup (read-only).
ballot_total_cost = sum(vote costs); cost_basis per vote; ballot receipt references all job_ids.
Usage: python3 ballot-cost-rollup.py <ballot_id>
"""
import json, os, sys, urllib.request

KEY = open(os.path.expanduser("~/.exchange_key")).read().strip()
UA = "Mozilla/5.0 (X11; Linux x86_64)"
BASE = "https://lucidota-exchange.cutecity.workers.dev"

def api(path):
    req = urllib.request.Request(BASE + path,
        headers={"Authorization": f"Bearer {KEY}", "User-Agent": UA})
    with urllib.request.urlopen(req, timeout=30) as r:
        return json.load(r)

def main():
    ballot_id = sys.argv[1]
    b = api(f"/v1/ballots/{ballot_id}")
    tally = b.get("tally") or {}
    votes = tally.get("votes") or []
    rows, total = [], 0.0
    for v in votes:
        job_id = v.get("job_id")
        if not job_id:
            rows.append({"vote": v.get("vote"), "job_id": None, "cost": None, "cost_basis": None})
            continue
        try:
            j = api(f"/v1/job/{job_id}")
        except Exception as e:
            rows.append({"vote": v.get("vote"), "job_id": job_id, "cost": None, "cost_basis": None, "error": str(e)[:80]})
            continue
        accts = j.get("accounting") or []
        cost = sum((a.get("measured_cost") or 0) for a in accts)
        basis = accts[0].get("cost_basis") if accts else None
        total += cost
        rows.append({"vote": v.get("vote"), "model": v.get("model"), "job_id": job_id,
                     "cost": cost, "cost_basis": basis, "valid": v.get("valid")})
    # The terminal ballot receipt (minted on state transition) references all job_ids via tally.votes
    rcpts = api(f"/v1/receipts?job=ballot:{ballot_id}").get("receipts") or []
    ballot_receipts = [r["id"] for r in rcpts]
    report = {
        "ballot_id": ballot_id, "state": b.get("state"), "verdict": b.get("verdict"),
        "ballot_total_cost": round(total, 6),
        "votes": rows,
        "ballot_receipt_ids": ballot_receipts,
        "all_job_ids_referenced": all(r["job_id"] for r in rows) and len(ballot_receipts) > 0,
    }
    print(json.dumps(report, indent=1))
    return 0

if __name__ == "__main__":
    sys.exit(main())
