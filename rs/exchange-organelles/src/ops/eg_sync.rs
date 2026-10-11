//! eg/sync — evidence graph sync. Port of evidence-graph-sync.py.
//! Mirrors ballot/receipt/evidence edges from the D1 mirror into a local
//! queryable SQLite graph. Idempotent: INSERT OR REPLACE.

use rusqlite::{params, Connection};
use serde_json::Value;

use super::{expanduser, OpError};

pub const MIRROR_DEFAULT: &str = "~/workspace/exchange/d1-mirror.db";
pub const OUT_DEFAULT: &str = "~/workspace/exchange/evidence-graph.db";

pub fn run(input: Value) -> Result<Value, OpError> {
    let out = input
        .get("db")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| expanduser(OUT_DEFAULT));
    let mirror = expanduser(MIRROR_DEFAULT);
    let (nn, ne) = sync(&mirror, &out)?;
    Ok(serde_json::json!({"db": out, "nodes": nn, "edges": ne}))
}

/// Mirror ballots/receipts/jobs from `mirror` into the graph at `out`.
/// Returns (nodes, edges).
pub fn sync(mirror: &str, out: &str) -> Result<(u64, u64), OpError> {
    let m = Connection::open(mirror)
        .map_err(|e| OpError::failed(format!("open mirror {mirror}: {e}")))?;
    let g = Connection::open(&out)
        .map_err(|e| OpError::failed(format!("open graph {out}: {e}")))?;
    g.execute_batch(
        "CREATE TABLE IF NOT EXISTS nodes (id TEXT PRIMARY KEY, kind TEXT, label TEXT, at TEXT);
         CREATE TABLE IF NOT EXISTS edges (src TEXT, rel TEXT, dst TEXT, detail TEXT,
                                           PRIMARY KEY (src, rel, dst));",
    )
    .map_err(|e| OpError::failed(format!("schema: {e}")))?;

    let (mut nn, mut ne) = (0u64, 0u64);
    let mut bq = m
        .prepare("SELECT id, ballot_type, state, verdict, created_at FROM ballots")
        .map_err(|e| OpError::failed(format!("ballots query: {e}")))?;
    let ballots: Vec<(String, String, String, String)> = bq
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(4)?,
            ))
        })
        .map_err(|e| OpError::failed(format!("ballots: {e}")))?
        .collect::<Result<_, _>>()
        .map_err(|e| OpError::failed(format!("ballots: {e}")))?;

    for (bid, btype, state, created_at) in ballots {
        g.execute(
            "INSERT OR REPLACE INTO nodes VALUES (?,?,?,?)",
            params![&bid, "ballot", format!("{btype}:{state}"), &created_at],
        )
        .map_err(|e| OpError::failed(format!("node insert: {e}")))?;
        nn += 1;

        let mut rq = m
            .prepare("SELECT id, job_id, result, worker_outcome, created_at FROM receipts WHERE ballot_id=?")
            .map_err(|e| OpError::failed(format!("receipts query: {e}")))?;
        let receipts: Vec<(String, Option<String>, Option<String>, Option<String>, Option<String>)> = rq
            .query_map([&bid], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, Option<String>>(4)?,
                ))
            })
            .map_err(|e| OpError::failed(format!("receipts: {e}")))?
            .collect::<Result<_, _>>()
            .map_err(|e| OpError::failed(format!("receipts: {e}")))?;

        for (rid, job_id, result, worker_outcome, rcreated) in receipts {
            let label = format!(
                "{}/{}",
                result.as_deref().unwrap_or(""),
                worker_outcome.as_deref().unwrap_or("")
            );
            g.execute(
                "INSERT OR REPLACE INTO nodes VALUES (?,?,?,?)",
                params![&rid, "receipt", &label, &rcreated],
            )
            .map_err(|e| OpError::failed(format!("node insert: {e}")))?;
            nn += 1;
            let detail = serde_json::json!({"job_id": job_id, "result": result}).to_string();
            g.execute(
                "INSERT OR REPLACE INTO edges VALUES (?,?,?,?)",
                params![&bid, "has_vote", &rid, &detail],
            )
            .map_err(|e| OpError::failed(format!("edge insert: {e}")))?;
            ne += 1;
            if let Some(jid) = job_id {
                g.execute(
                    "INSERT OR REPLACE INTO edges VALUES (?,?,?,?)",
                    params![&rid, "for_job", &jid, Option::<String>::None],
                )
                .map_err(|e| OpError::failed(format!("edge insert: {e}")))?;
                ne += 1;
                let has_job: bool = m
                    .query_row("SELECT id FROM jobs WHERE id=?", params![&jid], |_| Ok(true))
                    .unwrap_or(false);
                if has_job {
                    g.execute(
                        "INSERT OR REPLACE INTO nodes VALUES (?,?,?,?)",
                        params![&jid, "job", Option::<String>::None, Option::<String>::None],
                    )
                    .map_err(|e| OpError::failed(format!("node insert: {e}")))?;
                    nn += 1;
                }
            }
        }
    }

    let mut rq = m
        .prepare("SELECT id, resource FROM receipts WHERE resource IS NOT NULL")
        .map_err(|e| OpError::failed(format!("resource receipts: {e}")))?;
    let res_rows: Vec<(String, String)> = rq
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        .map_err(|e| OpError::failed(format!("resource receipts: {e}")))?
        .collect::<Result<_, _>>()
        .map_err(|e| OpError::failed(format!("resource receipts: {e}")))?;
    for (rid, resource) in res_rows {
        g.execute(
            "INSERT OR REPLACE INTO edges VALUES (?,?,?,?)",
            params![&rid, "ran_on", &resource, Option::<String>::None],
        )
        .map_err(|e| OpError::failed(format!("edge insert: {e}")))?;
        ne += 1;
    }

    Ok((nn, ne))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_mirror() -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("d1-mirror.db").to_string_lossy().into_owned();
        let m = Connection::open(&path).unwrap();
        m.execute_batch(
            "CREATE TABLE ballots (id TEXT, ballot_type TEXT, state TEXT, verdict TEXT, created_at TEXT);
             CREATE TABLE receipts (id TEXT, ballot_id TEXT, job_id TEXT, result TEXT, worker_outcome TEXT, created_at TEXT, resource TEXT);
             CREATE TABLE jobs (id TEXT);",
        )
        .unwrap();
        m.execute(
            "INSERT INTO ballots VALUES ('b1','mint','decided','ok','2026-10-01T00:00:00Z')",
            [],
        )
        .unwrap();
        m.execute(
            "INSERT INTO receipts VALUES ('r1','b1','j1','ok','SUCCEEDED','2026-10-01T00:01:00Z','res_a')",
            [],
        )
        .unwrap();
        m.execute(
            "INSERT INTO receipts VALUES ('r2','b1',NULL,'ok','SUCCEEDED','2026-10-01T00:02:00Z',NULL)",
            [],
        )
        .unwrap();
        m.execute("INSERT INTO jobs VALUES ('j1')", []).unwrap();
        (dir, path)
    }

    #[test]
    fn sync_builds_nodes_and_edges() {
        let (dir, mirror) = fixture_mirror();
        let out = dir.path().join("graph.db").to_string_lossy().into_owned();
        let (nn, ne) = sync(&mirror, &out).unwrap();
        // nodes: b1, r1, r2, j1 = 4; edges: has_vote x2, for_job x1, ran_on x1 = 4
        assert_eq!((nn, ne), (4, 4));

        let g = Connection::open(&out).unwrap();
        let label: String = g
            .query_row("SELECT label FROM nodes WHERE id='b1'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(label, "mint:decided");
        let detail: String = g
            .query_row(
                "SELECT detail FROM edges WHERE src='b1' AND rel='has_vote' AND dst='r1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let d: Value = serde_json::from_str(&detail).unwrap();
        assert_eq!(d["job_id"], "j1");
        assert_eq!(d["result"], "ok");

        // idempotent: re-run yields identical counts
        let (nn2, ne2) = sync(&mirror, &out).unwrap();
        assert_eq!((nn2, ne2), (4, 4));
        let n_nodes: i64 = g.query_row("SELECT COUNT(*) FROM nodes", [], |r| r.get(0)).unwrap();
        assert_eq!(n_nodes, 4);
    }
}
