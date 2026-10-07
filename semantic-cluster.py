#!/usr/bin/env python3
"""exch-022: Semantic answer clustering (separate governed analysis, NOT the tally).
The ballot tally stays exact-match (worker.js tallyBallot untouched). This
script clusters a ballot's answer votes by deterministic normalization tiers:

  tier0 exact      — byte-identical strings
  tier1 normalized — case/punctuation/whitespace-insensitive
  tier2 token-set  — same multiset of tokens, order-insensitive

True semantic equivalence (paraphrase) needs an embedding model and is
explicitly OUT of scope: the Exchange does not let an LLM re-judge votes.
Tiers are mechanical, deterministic, and auditable. Reads the D1 mirror only.

Usage: python3 semantic-cluster.py <ballot_id>
"""
import json, os, re, sqlite3, sys
from collections import defaultdict

MIRROR = os.path.expanduser("~/workspace/exchange/d1-mirror.db")

def norm1(s):
    return re.sub(r"\s+", " ", s.strip().lower()).strip(".,;:!?\"'")

def norm2(s):
    return " ".join(sorted(norm1(s).split()))

def main():
    ballot_id = sys.argv[1]
    c = sqlite3.connect(MIRROR)
    c.row_factory = sqlite3.Row
    b = c.execute("SELECT ballot_type FROM ballots WHERE id=?", (ballot_id,)).fetchone()
    if not b:
        print(json.dumps({"error": "ballot not found"})); return 1
    if b["ballot_type"] != "answer":
        print(json.dumps({"note": "clustering is for answer ballots; verdict ballots use the ternary verdict",
                          "ballot_type": b["ballot_type"]})); return 0
    has_contested = c.execute(
        "SELECT COUNT(*) FROM pragma_table_info('receipts') WHERE name='contested'").fetchone()[0] > 0
    contest_filter = "AND (r.contested IS NULL OR r.contested=0)" if has_contested else ""
    contest_filter_sub = "AND (contested IS NULL OR contested=0)" if has_contested else ""
    rows = c.execute(f"""
      SELECT r.id, r.job_id, r.decision FROM receipts r
      JOIN (SELECT job_id, MAX(created_at) mc FROM receipts
            WHERE ballot_id=? AND job_id NOT LIKE 'ballot:%'
              {contest_filter_sub} GROUP BY job_id) latest
        ON r.job_id=latest.job_id AND r.created_at=latest.mc
      WHERE r.ballot_id=? {contest_filter}""", (ballot_id, ballot_id)).fetchall()
    answers = []
    for r in rows:
        try: d = json.loads(r["decision"] or "{}")
        except: continue
        out = (d.get("output") or {})
        content = out.get("content") or out.get("reasoning_content") or ""
        vote = content.strip().lower()
        vote = re.sub(r"\s+", " ", vote).strip(".,;:!?")
        if vote and vote != "invalid":
            answers.append({"receipt_id": r["id"], "job_id": r["job_id"],
                            "model": d.get("vote_model") or "unknown", "answer": vote})
    t0 = defaultdict(list); t1 = defaultdict(list); t2 = defaultdict(list)
    for a in answers:
        t0[a["answer"]].append(a["job_id"])
        t1[norm1(a["answer"])].append(a["job_id"])
        t2[norm2(a["answer"])].append(a["job_id"])
    def clusters(t):
        return [{"key": k, "size": len(v), "job_ids": v}
                for k, v in sorted(t.items(), key=lambda x: -len(x[1]))]
    print(json.dumps({
        "ballot_id": ballot_id, "n_answers": len(answers),
        "tier0_exact": clusters(t0), "tier1_normalized": clusters(t1),
        "tier2_token_set": clusters(t2),
        "note": "tally stays exact-match; clusters are advisory only",
    }, indent=1))
    return 0

if __name__ == "__main__":
    sys.exit(main())
