#!/usr/bin/env python3
"""exch-018: Evidence graph sync. Phase 3 revision (2026-11-20).

Mirrors ballot/receipt/evidence edges from the D1 mirror into a local
queryable SQLite graph (~/workspace/exchange/evidence-graph.db).

Deliberately NOT the curated entity graph (~/workspace/entity-graph/graph.db):
that store records only what Zackary reports. This is a mechanical mirror of
Exchange evidence — ballots, receipts, jobs, resources — as nodes and edges.

Edges: ballot -has_vote-> receipt, receipt -for_job-> job,
       receipt -ran_on-> resource.
Reads the D1 mirror only (freeze-safe). Rebuilds from scratch into a shadow
file and atomically swaps it in (no stale rows accumulate).

Phase 3 fix (G04 — the graph was 95.8% orphan edges, 1,284/1,341):
  1. The Oct 7 version materialized nodes ONLY for ballot-linked entities
     (34 of 1,360 receipts) and emitted for_job edges only inside the ballot
     loop. This version materializes ALL ballots, receipts, jobs AND
     resources as nodes, and emits for_job / ran_on for EVERY receipt.
  2. Stub pass (DeepSeek methods review, 2026-11-20): any edge endpoint
     still missing after materialization gets a `stub` node (kind `stub`,
     label records the referencing rel) so the graph is CLOSED by
     construction — 0 orphan edges, verified on every run. Known stub
     source in live data: 11 receipts carry job_id `ballot:bal_*`
     (a ballot reference in a job-id column, not a job id).
  3. Closure verification on every run: orphan-edge count, duplicate-node
     check, integrity_check. Non-zero orphans or a failed integrity check
     aborts the swap (exit 1) and leaves the previous graph in place.

Known gaps (unchanged from Oct 7, not introduced here): the docstring's
`decided_by` / `contests` rels are still not emitted — no mirror table
carries that data. Workers are not nodes (no edge references them).

Usage: python3 evidence-graph-sync.py [--db PATH] [--mirror PATH]
"""
import json, os, sqlite3, sys

MIRROR = os.path.expanduser("~/workspace/exchange/d1-mirror.db")
OUT = os.path.expanduser("~/workspace/exchange/evidence-graph.db")
REQUIRED_TABLES = ("ballots", "receipts", "jobs", "resources")


def arg(flag, default):
    return sys.argv[sys.argv.index(flag) + 1] if flag in sys.argv else default


def main():
    out = arg("--db", OUT)
    mirror = arg("--mirror", MIRROR)
    m = sqlite3.connect(f"file:{mirror}?mode=ro", uri=True)
    m.row_factory = sqlite3.Row
    have = {r[0] for r in m.execute(
        "SELECT name FROM sqlite_master WHERE type='table'")}
    missing = [t for t in REQUIRED_TABLES if t not in have]
    if missing:
        print(f"ABORT: mirror {mirror} lacks tables: {missing}", file=sys.stderr)
        return 1
    tmp = out + ".new"
    if os.path.exists(tmp):
        os.remove(tmp)
    g = sqlite3.connect(tmp)
    g.execute("CREATE TABLE nodes (id TEXT PRIMARY KEY, kind TEXT, label TEXT, at TEXT)")
    g.execute("""CREATE TABLE edges
                 (src TEXT, rel TEXT, dst TEXT, detail TEXT,
                  PRIMARY KEY (src, rel, dst))""")
    g.execute("CREATE INDEX idx_edges_src ON edges(src)")
    g.execute("CREATE INDEX idx_edges_dst ON edges(dst)")

    nodes = {}  # id -> (kind, label, at); single writer, no id collisions
    def add_node(nid, kind, label, at):
        if nid is None:
            return
        if nid in nodes and nodes[nid][0] != kind:
            print(f"WARN: id {nid!r} claimed by both {nodes[nid][0]} and {kind}; "
                  f"keeping first", file=sys.stderr)
            return
        nodes[nid] = (kind, label, at)

    for b in m.execute("SELECT id, ballot_type, state, created_at FROM ballots"):
        add_node(b["id"], "ballot", f"{b['ballot_type']}:{b['state']}", b["created_at"])
    for r in m.execute("SELECT id, result, worker_outcome, created_at FROM receipts"):
        add_node(r["id"], "receipt", f"{r['result']}/{r['worker_outcome']}", r["created_at"])
    for j in m.execute("SELECT id, organelle, capability, created_at FROM jobs"):
        add_node(j["id"], "job", f"{j['organelle']}:{j['capability']}", j["created_at"])
    for r in m.execute("SELECT id, name, kind, created_at FROM resources"):
        add_node(r["id"], "resource", f"{r['name']} ({r['kind']})", r["created_at"])

    edges = []  # (src, rel, dst, detail)
    for r in m.execute("SELECT id, ballot_id, job_id, resource, result FROM receipts"):
        if r["ballot_id"]:
            edges.append((r["ballot_id"], "has_vote", r["id"],
                          json.dumps({"job_id": r["job_id"], "result": r["result"]})))
        if r["job_id"]:
            edges.append((r["id"], "for_job", r["job_id"], None))
        if r["resource"]:
            edges.append((r["id"], "ran_on", r["resource"], None))

    # Stub pass: every edge endpoint becomes a node, so the graph is closed.
    stub_rels = {}
    for src, rel, dst, _ in edges:
        for ep in (src, dst):
            if ep not in nodes:
                stub_rels.setdefault(ep, set()).add(rel)
    for ep, rels in sorted(stub_rels.items()):
        add_node(ep, "stub", f"unresolved endpoint, referenced by: {','.join(sorted(rels))}", None)

    g.executemany("INSERT INTO nodes VALUES (?,?,?,?)",
                  [(nid, k, lab, at) for nid, (k, lab, at) in nodes.items()])
    g.executemany("INSERT OR IGNORE INTO edges VALUES (?,?,?,?)", edges)
    g.commit()

    # Closure verification (before the swap).
    orphans = g.execute(
        "SELECT COUNT(*) FROM edges e WHERE NOT EXISTS "
        "(SELECT 1 FROM nodes n WHERE n.id = e.src) OR NOT EXISTS "
        "(SELECT 1 FROM nodes n WHERE n.id = e.dst)").fetchone()[0]
    dup_nodes = g.execute(
        "SELECT COUNT(*) FROM (SELECT id FROM nodes GROUP BY id HAVING COUNT(*) > 1)"
    ).fetchone()[0]
    integrity = g.execute("PRAGMA integrity_check").fetchone()[0]
    by_kind = dict(g.execute("SELECT kind, COUNT(*) FROM nodes GROUP BY kind").fetchall())
    by_rel = dict(g.execute("SELECT rel, COUNT(*) FROM edges GROUP BY rel").fetchall())
    n_nodes = g.execute("SELECT COUNT(*) FROM nodes").fetchone()[0]
    n_edges = g.execute("SELECT COUNT(*) FROM edges").fetchone()[0]
    g.close()
    if orphans or dup_nodes or integrity != "ok":
        os.remove(tmp)
        print(json.dumps({"db": out, "verdict": "ABORT", "orphan_edges": orphans,
                          "duplicate_nodes": dup_nodes, "integrity": integrity}))
        return 1
    os.replace(tmp, out)
    print(json.dumps({"db": out, "verdict": "OK", "nodes": n_nodes, "edges": n_edges,
                      "nodes_by_kind": by_kind, "edges_by_rel": by_rel,
                      "orphan_edges": 0, "stub_nodes": by_kind.get("stub", 0),
                      "integrity": integrity}))
    return 0


if __name__ == "__main__":
    sys.exit(main())
