#!/usr/bin/env python3
"""exch-020: Voter set expansion hook.
A model becomes auto-eligible as a ballot voter after a 10-verify streak:
10 consecutive SUCCEEDED+verified completions (no VERIFICATION_FAILURE,
no failure) on inference capability, newest-first.
Reads the D1 mirror (never live D1). --apply posts to POST /v1/ballot-policy
(requires the v0.7.0 staged bundle deployed); default is --check (report only).
"""
import json, os, sqlite3, sys, urllib.request

MIRROR = os.path.expanduser("~/workspace/exchange/d1-mirror.db")
KEY = open(os.path.expanduser("~/.exchange_key")).read().strip()
UA = "Mozilla/5.0 (X11; Linux x86_64)"
BASE = "https://lucidota-exchange.cutecity.workers.dev"
STREAK = 10

def current_voters():
    # Freeze-safe: derive from the mirror's most recent ballot (ballots carry
    # their voter snapshot). No live governor reads.
    c = sqlite3.connect(MIRROR)
    row = c.execute("SELECT voters FROM ballots ORDER BY created_at DESC LIMIT 1").fetchone()
    return json.loads(row[0]) if row else []

def streaks():
    c = sqlite3.connect(MIRROR)
    # Model identity lives on executions.actual_model (via claims), not on
    # most receipts. Join receipts->claims->executions; verified = terminal ok.
    rows = c.execute("""
      SELECT e.actual_model AS model, r.result, r.worker_outcome, r.created_at
      FROM receipts r
      JOIN claims cl ON cl.claim_id = r.claim_id
      JOIN executions e ON e.claim_id = cl.claim_id
      WHERE e.actual_model IS NOT NULL
      ORDER BY r.created_at DESC LIMIT 20000""").fetchall()
    hist = {}
    for model, result, outcome, _ts in rows:
        verified = (result == "ok" and outcome == "SUCCEEDED")
        hist.setdefault(model, []).append(verified)
    # Eligibility: longest 10-verify streak anywhere in history AND the most
    # recent completion verified (current form healthy — a model that degraded
    # since its streak does not auto-qualify).
    # Eligibility: longest 10-verify streak anywhere in history AND the most
    # recent completion verified (current form healthy — a model that degraded
    # since its streak does not auto-qualify). Test/impostor identities from
    # redteam and dogfood traffic can never become voters.
    BLOCKED = ("rt2", "impostor", "simulated", "fake", "manual", "test")
    eligible = {}
    for model, seq in hist.items():
        if any(b in model.lower() for b in BLOCKED):
            continue
        best = cur = 0
        for v in seq:
            cur = cur + 1 if v else 0
            best = max(best, cur)
        if best >= STREAK and seq[0]:
            eligible[model] = best
    return eligible

def main():
    apply = "--apply" in sys.argv
    eligible = streaks()
    voters = current_voters()
    new = [m for m in eligible if m not in voters]
    report = {"streak_models": eligible, "current_voters": voters,
              "newly_eligible": new, "applied": False}
    if apply and new:
        updated = voters + new[:7 - len(voters)]
        updated = updated[:7]
        body = json.dumps({"voters": updated}).encode()
        req = urllib.request.Request(BASE + "/v1/ballot-policy", data=body, method="POST",
            headers={"Authorization": f"Bearer {KEY}", "User-Agent": UA,
                     "Content-Type": "application/json"})
        try:
            with urllib.request.urlopen(req, timeout=30) as r:
                report["applied"] = True
                report["policy"] = json.load(r)["policy"]
        except Exception as e:
            report["apply_error"] = str(e)[:200]
    print(json.dumps(report, indent=1))

if __name__ == "__main__":
    sys.exit(main())
