#!/usr/bin/env python3
"""Local SQLite replica of governor v0.6.6 tallyBallot + completeClaimed receipt
stamping, ported line-by-line from ~/workspace/exchange/worker.js.

Purpose: run the REDTEAM-2 quorum attack battery OFFLINE (D1 freeze in effect).
Results are PROVEN against the replica; the replica is a faithful port of the
audited code paths, noted per-test where live behavior was also observed.
"""
import sqlite3, json, re, math, time
from datetime import datetime, timezone

DB = "/tmp/rt2_replica.db"

def now_iso():
    return datetime.now(timezone.utc).isoformat(timespec="milliseconds").replace("+00:00", "Z")

SCHEMA = """
CREATE TABLE IF NOT EXISTS ballots (
  id TEXT PRIMARY KEY, ballot_type TEXT NOT NULL, proposition TEXT NOT NULL,
  voters TEXT NOT NULL, quorum INTEGER NOT NULL, deadline_at TEXT NOT NULL,
  state TEXT DEFAULT 'open', verdict TEXT, tally TEXT, created_at TEXT);
CREATE TABLE IF NOT EXISTS receipts (
  id TEXT PRIMARY KEY, job_id TEXT, ballot_id TEXT, decision TEXT,
  worker_outcome TEXT, created_at TEXT, ternary_verdict INTEGER);
"""

def db():
    import os
    if os.path.exists(DB):
        os.remove(DB)
    c = sqlite3.connect(DB)
    c.executescript(SCHEMA)
    return c

# ---- port of extractVote (worker.js:2115) ----
def extract_vote(btype, output):
    content = (output or {}).get("content") or (output or {}).get("reasoning_content") or ""
    if btype == "verdict":
        tail = str(content)[-300:]
        m = re.search(r"\b(AUTHORIZED|UNKNOWN|DENIED)\b", tail)
        return m.group(1) if m else "INVALID"
    norm = re.sub(r"\s+", " ", str(content).strip().lower())
    norm = re.sub(r"[.,;:!?]+$", "", norm)
    return norm or "INVALID"

# ---- port of completeClaimed receipt stamping (worker.js:765-789) ----
_rcpt_seq = [0]
def complete_receipt(c, job_id, ballot_id, worker_outcome, output):
    """Returns receipt id. Faithful to governor: failed -> output NULL;
    ballot_id stamped ONLY if not failed and ternary_verdict in {1,0,-1}."""
    failed = worker_outcome != "SUCCEEDED"
    _rcpt_seq[0] += 1
    rid = f"rcpt_{int(time.time()*1000)}_{_rcpt_seq[0]}_{job_id[-4:]}"
    ternary = None
    stamp = None
    if not failed and output and output.get("ternary_verdict") in (1, 0, -1):
        ternary = output["ternary_verdict"]
        stamp = ballot_id  # worker.js:787-789 — nested INSIDE ternary branch
    out = None if failed else output  # worker.js: output: !failed ? b.output : null
    decision = json.dumps({"output": out, "vote_model": job_id})
    c.execute("INSERT INTO receipts VALUES (?,?,?,?,?,?,?)",
              (rid, job_id, stamp, decision, worker_outcome, now_iso(), ternary))
    return rid

# ---- port of tallyBallot (worker.js:2129-2183) ----
def tally_ballot(c, ballot_id, now_ms=None):
    b = c.execute("SELECT * FROM ballots WHERE id=?", (ballot_id,)).fetchone()
    cols = [d[0] for d in c.execute("SELECT * FROM ballots LIMIT 1").description]
    b = dict(zip(cols, b))
    voters = json.loads(b["voters"])
    rows = c.execute("""
      SELECT r.id, r.job_id, r.decision, r.worker_outcome FROM receipts r
      JOIN (SELECT job_id, MAX(created_at) AS mc FROM receipts
            WHERE ballot_id=? GROUP BY job_id) latest
        ON r.job_id = latest.job_id AND r.created_at = latest.mc
      WHERE r.ballot_id=?""", (ballot_id, ballot_id)).fetchall()
    votes = []
    for rid, job_id, decision, wout in rows:
        try:
            dec = json.loads(decision or "{}")
        except Exception:
            dec = {}
        vote = extract_vote(b["ballot_type"], dec.get("output"))
        votes.append({"receipt_id": rid, "job_id": job_id, "vote": vote,
                      "valid": vote != "INVALID", "worker_outcome": wout})
    valid = [v for v in votes if v["valid"]]
    counts = {}
    for v in valid:
        counts[v["vote"]] = counts.get(v["vote"], 0) + 1
    quorum = math.floor(len(voters) / 2) + 1
    now_ms = now_ms if now_ms is not None else int(time.time() * 1000)
    deadline_ms = int(datetime.fromisoformat(b["deadline_at"].replace("Z", "+00:00")).timestamp() * 1000)
    expired = now_ms > deadline_ms
    state, verdict = b["state"], b["verdict"]
    if state == "open":
        srt = sorted(counts.items(), key=lambda kv: kv[1], reverse=True)
        top = srt[0] if srt else None
        if top and top[1] > len(valid) / 2 and len(valid) >= quorum:
            state, verdict = "decided", top[0]
        elif expired:
            state = "inconclusive" if len(valid) >= quorum else "expired"
            verdict = "UNKNOWN" if state == "inconclusive" else None
        elif len(valid) >= quorum and len(srt) > 1 and srt[0][1] == srt[1][1]:
            pass  # tie, keep waiting
    tally = {"votes": votes, "counts": counts, "valid": len(valid),
             "quorum": quorum, "voters": len(voters), "expired": expired}
    if state != b["state"]:
        c.execute("UPDATE ballots SET state=?, verdict=?, tally=? WHERE id=?",
                  (state, verdict, json.dumps(tally), ballot_id))
        # terminal ballot receipt — worker.js:2203-2204 (job_id='ballot:'+id)
        trid = f"rcpt_terminal_{ballot_id}"
        c.execute("INSERT OR IGNORE INTO receipts VALUES (?,?,?,?,?,?,?)",
                  (trid, "ballot:" + ballot_id, ballot_id,
                   json.dumps({"ballot": ballot_id, "tally": tally, "verdict": verdict}),
                   None, now_iso(), None))
        c.commit()
    return {"ballot_id": ballot_id, "state": state, "verdict": verdict, "tally": tally}

def make_ballot(c, ballot_id, btype, voters, deadline_s):
    from datetime import timedelta
    dl = (datetime.now(timezone.utc) + timedelta(seconds=deadline_s)).isoformat().replace("+00:00", "Z")
    quorum = math.floor(len(voters) / 2) + 1
    c.execute("INSERT INTO ballots (id,ballot_type,proposition,voters,quorum,deadline_at,created_at) VALUES (?,?,?,?,?,?,?)",
              (ballot_id, btype, "test", json.dumps(voters), quorum, dl, now_iso()))
    c.commit()
    return {"ballot_id": ballot_id, "deadline_at": dl, "quorum": quorum}

def vote_out(word, ternary):
    return {"content": f"reasoning...\n\nFinal verdict:\n{word}", "ternary_verdict": ternary}

def show(res):
    t = res["tally"]
    print(f"  state={res['state']} verdict={res['verdict']} valid={t['valid']} quorum={t['quorum']} expired={t['expired']}")
    for v in t["votes"]:
        print(f"    job={str(v['job_id'])[-28:]} vote={v['vote']} valid={v['valid']} outcome={v['worker_outcome']}")

if __name__ == "__main__":
    print("replica module loads OK")
