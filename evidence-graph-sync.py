#!/usr/bin/env python3
"""exch-018: Evidence graph sync.
Mirrors ballot/receipt/evidence edges from the D1 mirror into a local
queryable SQLite graph (~/workspace/exchange/evidence-graph.db).

Deliberately NOT the curated entity graph (~/workspace/entity-graph/graph.db):
that store records only what Zackary reports. This is a mechanical mirror of
Exchange evidence — ballots, receipts, jobs, resources — as nodes and edges.

Edges: ballot -has_vote-> receipt, ballot -decided_by-> receipt (terminal),
       receipt -for_job-> job, job -ran_on-> resource, receipt -contests-> receipt.
Reads the D1 mirror only (freeze-safe). Idempotent: INSERT OR REPLACE.

Usage: python3 evidence-graph-sync.py [--db PATH]
"""
import json, os, sqlite3, sys

MIRROR = os.path.expanduser("~/workspace/exchange/d1-mirror.db")
OUT = os.path.expanduser("~/workspace/exchange/evidence-graph.db")

def main():
    out = sys.argv[sys.argv.index("--db") + 1] if "--db" in sys.argv else OUT
    m = sqlite3.connect(MIRROR); m.row_factory = sqlite3.Row
    g = sqlite3.connect(out)
    g.execute("CREATE TABLE IF NOT EXISTS nodes (id TEXT PRIMARY KEY, kind TEXT, label TEXT, at TEXT)")
    g.execute("""CREATE TABLE IF NOT EXISTS edges
                 (src TEXT, rel TEXT, dst TEXT, detail TEXT,
                  PRIMARY KEY (src, rel, dst))""")
    nn = ne = 0
    for b in m.execute("SELECT id, ballot_type, state, verdict, created_at FROM ballots"):
        g.execute("INSERT OR REPLACE INTO nodes VALUES (?,?,?,?)",
                  (b["id"], "ballot", f"{b['ballot_type']}:{b['state']}", b["created_at"]))
        nn += 1
        for r in m.execute(
            "SELECT id, job_id, result, worker_outcome, created_at FROM receipts WHERE ballot_id=?",
            (b["id"],)):
            g.execute("INSERT OR REPLACE INTO nodes VALUES (?,?,?,?)",
                      (r["id"], "receipt", f"{r['result']}/{r['worker_outcome']}", r["created_at"]))
            nn += 1
            g.execute("INSERT OR REPLACE INTO edges VALUES (?,?,?,?)",
                      (b["id"], "has_vote", r["id"],
                       json.dumps({"job_id": r["job_id"], "result": r["result"]})))
            ne += 1
            if r["job_id"]:
                g.execute("INSERT OR REPLACE INTO edges VALUES (?,?,?,?)",
                          (r["id"], "for_job", r["job_id"], None))
                ne += 1
                job = m.execute("SELECT id FROM jobs WHERE id=?", (r["job_id"],)).fetchone()
                if job:
                    g.execute("INSERT OR REPLACE INTO nodes VALUES (?,?,?,?)",
                              (job["id"], "job", None, None))
                    nn += 1
    for r in m.execute("SELECT id, resource FROM receipts WHERE resource IS NOT NULL"):
        g.execute("INSERT OR REPLACE INTO edges VALUES (?,?,?,?)",
                  (r["id"], "ran_on", r["resource"], None))
        ne += 1
    g.commit()
    print(json.dumps({"db": out, "nodes": nn, "edges": ne}))
    return 0

if __name__ == "__main__":
    sys.exit(main())
