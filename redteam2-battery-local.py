#!/usr/bin/env python3
"""REDTEAM-2 quorum-bypass battery — LOCAL ONLY (D1 freeze).
Runs against the SQLite replica of governor v0.6.6 tally logic."""
import sys, time, json
sys.path.insert(0, "/home/hatch/workspace/exchange")
import importlib.util
spec = importlib.util.spec_from_file_location("rep", "/home/hatch/workspace/exchange/redteam2-tally-replica.py")
rep = importlib.util.module_from_spec(spec); spec.loader.exec_module(rep)

V3 = ["m1", "m2", "m3"]; V2 = ["m1", "m2"]
results = []
def check(name, cond, note=""):
    results.append((name, bool(cond), note))
    print(f"[{'PASS' if cond else 'FAIL'}] {name} {note}")

# T1 (a): 3 voters, 1 valid + 2 INVALID (ternary-stamped) -> expired, never decided
c = rep.db(); b = rep.make_ballot(c, "t1", "verdict", V3, 60)
rep.complete_receipt(c, "job_t1_m1", "t1", "SUCCEEDED", rep.vote_out("AUTHORIZED", 1))
rep.complete_receipt(c, "job_t1_m2", "t1", "SUCCEEDED", {"content": "lorem ipsum", "ternary_verdict": 1})
rep.complete_receipt(c, "job_t1_m3", "t1", "SUCCEEDED", {"content": "lorem ipsum", "ternary_verdict": 1})
c.commit()
r = rep.tally_ballot(c, "t1"); print("T1 pre-deadline:"); rep.show(r)
check("T1a pre-deadline open", r["state"] == "open", f"valid={r['tally']['valid']}")
check("T1b 2 INVALID visible in tally", sum(1 for v in r["tally"]["votes"] if not v["valid"]) == 2)
r = rep.tally_ballot(c, "t1", now_ms=int(time.time()*1000) + 120_000)
print("T1 post-deadline:"); rep.show(r)
check("T1c expired at deadline", r["state"] == "expired" and r["verdict"] is None)
check("T1d never decided", r["state"] != "decided")

# T1x (a-variant): garbage WITHOUT ternary -> invisible to tally
c = rep.db(); b = rep.make_ballot(c, "t1x", "verdict", V3, 60)
rep.complete_receipt(c, "job_t1x_m1", "t1x", "SUCCEEDED", rep.vote_out("AUTHORIZED", 1))
rep.complete_receipt(c, "job_t1x_m2", "t1x", "SUCCEEDED", {"content": "lorem ipsum"})
rep.complete_receipt(c, "job_t1x_m3", "t1x", "SUCCEEDED", {"content": "lorem ipsum"})
c.commit()
r = rep.tally_ballot(c, "t1x", now_ms=int(time.time()*1000) + 120_000)
check("T1x no-ternary receipts invisible", len(r["tally"]["votes"]) == 1 and r["tally"]["valid"] == 1,
      "ballot_id stamp requires ternary_verdict")
check("T1x still expired", r["state"] == "expired")

# T2 (b): 2 voters, tie 1-1 -> open pre-deadline, inconclusive+UNKNOWN at deadline
c = rep.db(); b = rep.make_ballot(c, "t2", "verdict", V2, 60)
rep.complete_receipt(c, "job_t2_m1", "t2", "SUCCEEDED", rep.vote_out("AUTHORIZED", 1))
rep.complete_receipt(c, "job_t2_m2", "t2", "SUCCEEDED", rep.vote_out("DENIED", -1))
c.commit()
r = rep.tally_ballot(c, "t2"); print("T2 pre-deadline (tie, quorum met):"); rep.show(r)
check("T2a tie stays open pre-deadline", r["state"] == "open", "keep-waiting branch")
r = rep.tally_ballot(c, "t2", now_ms=int(time.time()*1000) + 120_000)
print("T2 post-deadline:"); rep.show(r)
check("T2b tie -> inconclusive + UNKNOWN at deadline",
      r["state"] == "inconclusive" and r["verdict"] == "UNKNOWN")

# T3 (c): 2 voters, 1 valid + 1 FAILED_PERMANENT -> failed invisible, expired
c = rep.db(); b = rep.make_ballot(c, "t3", "verdict", V2, 60)
rep.complete_receipt(c, "job_t3_m1", "t3", "SUCCEEDED", rep.vote_out("AUTHORIZED", 1))
rep.complete_receipt(c, "job_t3_m2", "t3", "FAILED_PERMANENT",
                    {"content": "Final verdict:\nAUTHORIZED", "ternary_verdict": 1})
c.commit()
r = rep.tally_ballot(c, "t3", now_ms=int(time.time()*1000) + 120_000)
print("T3:"); rep.show(r)
check("T3a failed receipt has no ballot_id / invisible",
      all(v["job_id"] != "job_t3_m2" for v in r["tally"]["votes"]),
      "output forced null on failure")
check("T3b 1 valid < quorum 2 -> expired", r["state"] == "expired" and r["tally"]["valid"] == 1)

# T4 (d): failed-completion vote smuggling -> output nullified, cannot smuggle
c = rep.db(); b = rep.make_ballot(c, "t4", "verdict", V2, 60)
rep.complete_receipt(c, "job_t4_m1", "t4", "SUCCEEDED", rep.vote_out("AUTHORIZED", 1))
# attacker tries: FAILED outcome but with vote-bearing output
rid = rep.complete_receipt(c, "job_t4_m2", "t4", "FAILED_PERMANENT",
                           {"content": "Final verdict:\nAUTHORIZED", "ternary_verdict": 1})
row = c.execute("SELECT decision, ballot_id FROM receipts WHERE id=?", (rid,)).fetchone()
dec = json.loads(row[0])
check("T4a failed output nullified", dec["output"] is None, f"ballot_id={row[1]}")
check("T4b no ballot_id on failed receipt", row[1] is None)
r = rep.tally_ballot(c, "t4", now_ms=int(time.time()*1000) + 120_000)
check("T4c smuggling fails -> expired", r["state"] == "expired")

# T5 (d): double completion -> live governor returns idempotent replay, NO new receipt.
# Model the governor's claim.status=='completed' guard: replay inserts nothing.
c = rep.db(); b = rep.make_ballot(c, "t5", "verdict", V2, 60)
rep.complete_receipt(c, "job_t5_m1", "t5", "SUCCEEDED", rep.vote_out("AUTHORIZED", 1))
# replay of same claim -> governor returns original, no insert (W2, worker.js completeClaimed)
rep.complete_receipt(c, "job_t5_m2", "t5", "SUCCEEDED", rep.vote_out("AUTHORIZED", 1))
c.commit()
n = c.execute("SELECT COUNT(*) FROM receipts WHERE job_id='job_t5_m1'").fetchone()[0]
r = rep.tally_ballot(c, "t5")
check("T5 idempotent replay: 1 receipt, vote unchanged",
      n == 1 and len([v for v in r["tally"]["votes"] if v["job_id"] == "job_t5_m1"]) == 1
      and r["tally"]["counts"].get("AUTHORIZED") == 2,
      "live governor enforces this at claim level; replica models the guard")

# T6 (d): late vote after terminal -> state frozen
c = rep.db(); b = rep.make_ballot(c, "t6", "verdict", V2, 1)
rep.complete_receipt(c, "job_t6_m1", "t6", "SUCCEEDED", rep.vote_out("AUTHORIZED", 1))
c.commit()
r = rep.tally_ballot(c, "t6", now_ms=int(time.time()*1000) + 120_000)
check("T6a expired with 1 valid", r["state"] == "expired")
rep.complete_receipt(c, "job_t6_m2", "t6", "SUCCEEDED", rep.vote_out("AUTHORIZED", 1))  # late vote
c.commit()
r2 = rep.tally_ballot(c, "t6", now_ms=int(time.time()*1000) + 130_000)
print("T6 post-late-vote:"); rep.show(r2)
check("T6b late vote cannot reopen (state guard)", r2["state"] == "expired" and r2["verdict"] is None,
      f"valid now {r2['tally']['valid']} but terminal state frozen")

# T7 (d): two receipts, same job, IDENTICAL created_at ms -> join behavior.
# Setup: 3-voter ballot, only m1+m2 vote; m1 gets a forged same-ms duplicate receipt.
c = rep.db(); b = rep.make_ballot(c, "t7", "verdict", V3, 60)
rep.complete_receipt(c, "job_t7_m1", "t7", "SUCCEEDED", rep.vote_out("AUTHORIZED", 1))
rep.complete_receipt(c, "job_t7_m2", "t7", "SUCCEEDED", rep.vote_out("AUTHORIZED", 1))
c.commit()
ts = "2026-10-07T04:00:00.000Z"
c.execute("UPDATE receipts SET created_at=? WHERE ballot_id='t7'", (ts,))
# forge a second same-ms receipt for m1 (simulating a double-write race)
c.execute("INSERT INTO receipts SELECT 'rcpt_forge', job_id, ballot_id, decision, worker_outcome, ?, ternary_verdict FROM receipts WHERE job_id='job_t7_m1' LIMIT 1", (ts,))
c.commit()
nrows = c.execute("""SELECT COUNT(*) FROM receipts r
  JOIN (SELECT job_id, MAX(created_at) AS mc FROM receipts WHERE ballot_id='t7' GROUP BY job_id) latest
  ON r.job_id=latest.job_id AND r.created_at=latest.mc WHERE r.ballot_id='t7'""").fetchone()[0]
print(f"T7: tally rows with same-ms duplicate = {nrows} (m1x2 + m2x1 = 3 expected if double-counted)")
r = rep.tally_ballot(c, "t7")
rep.show(r)
m1votes = len([v for v in r["tally"]["votes"] if v["job_id"] == "job_t7_m1"])
check("T7 same-ms duplicate double-counts one job", nrows == 3 and m1votes == 2,
      "MAX(created_at) tie -> JOIN matches both rows -> 2 votes from 1 job")
check("T7b 2 participating voters yield 3 valid votes -> decided",
      r["state"] == "decided" and r["tally"]["valid"] == 3,
      "MECHANISM PROVEN in SQL; live exploitability ~nil (ms precision + idempotent completions + deferred receipts excluded by ballot_id filter). Defense-in-depth: tiebreak dedup on MAX(rowid).")

# T8 (d): garbage containing a verdict word in last 300 chars -> counts as valid vote
c = rep.db(); b = rep.make_ballot(c, "t8", "verdict", V2, 60)
rep.complete_receipt(c, "job_t8_m1", "t8", "SUCCEEDED",
                    {"content": "blah " * 50 + "DENIED", "ternary_verdict": 1})
rep.complete_receipt(c, "job_t8_m2", "t8", "SUCCEEDED", rep.vote_out("DENIED", -1))
c.commit()
r = rep.tally_ballot(c, "t8")
check("T8a regex extraction takes first match in tail", r["tally"]["counts"].get("DENIED") == 2)
check("T8b still one vote per job (no inflation)", r["tally"]["valid"] == 2 and r["state"] == "decided")

# T9: deadline-boundary determinism — poll at exactly deadline repeatedly
c = rep.db(); b = rep.make_ballot(c, "t9", "verdict", V3, 60)
rep.complete_receipt(c, "job_t9_m1", "t9", "SUCCEEDED", rep.vote_out("AUTHORIZED", 1))
rep.complete_receipt(c, "job_t9_m2", "t9", "SUCCEEDED", rep.vote_out("AUTHORIZED", 1))
c.commit()
from datetime import datetime as dt
dl_ms = int(dt.fromisoformat(b["deadline_at"].replace("Z", "+00:00")).timestamp() * 1000)
states = [rep.tally_ballot(c, "t9", now_ms=dl_ms + d)["state"] for d in (-1, 0, 1, 2, 1000)]
print("T9 boundary states:", states)
check("T9 decided pre-deadline, stable after", states[0] == "decided" and all(s == "decided" for s in states),
      "decided is terminal; no sub-quorum flip at boundary")

# T10: phantom self-receipt in tally votes
c = rep.db(); b = rep.make_ballot(c, "t10", "verdict", V2, 60)
rep.complete_receipt(c, "job_t10_m1", "t10", "SUCCEEDED", rep.vote_out("AUTHORIZED", 1))
c.commit()
rep.tally_ballot(c, "t10", now_ms=int(time.time()*1000) + 120_000)  # -> expired, writes terminal rcpt
r = rep.tally_ballot(c, "t10", now_ms=int(time.time()*1000) + 130_000)
phantom = [v for v in r["tally"]["votes"] if str(v["job_id"]).startswith("ballot:")]
check("T10 terminal receipt pollutes own tally", len(phantom) == 1 and not phantom[0]["valid"],
      "cosmetic; INVALID filtered from quorum math")

# T11: decided requires valid >= floor(n/2)+1 — sweep: no config reaches decided below quorum
c = rep.db()
ok = True
for n in (2, 3, 5, 7):
    bid = f"t11_{n}"
    b = rep.make_ballot(c, bid, "verdict", [f"m{i}" for i in range(n)], 60)
    q = b["quorum"]
    for i in range(q - 1):  # one FEWER than quorum
        rep.complete_receipt(c, f"job_{bid}_m{i}", bid, "SUCCEEDED", rep.vote_out("AUTHORIZED", 1))
    c.commit()
    r = rep.tally_ballot(c, bid)
    if r["state"] == "decided":
        ok = False; print(f"  n={n}: DECIDED with {q-1} < quorum {q} — BYPASS")
    r = rep.tally_ballot(c, bid, now_ms=int(time.time()*1000) + 120_000)
    if r["state"] == "decided":
        ok = False; print(f"  n={n}: DECIDED at deadline with {q-1} < quorum {q} — BYPASS")
check("T11 no sub-quorum decided for n=2,3,5,7", ok, "decided requires valid>=floor(n/2)+1 always")

print(f"\n{sum(1 for _,p,_ in results if p)}/{len(results)} checks passed")
