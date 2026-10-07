#!/usr/bin/env python3
"""Intelligence per byte: benchmark harness for Exchange jobs.

Metric (per ~/workspace/adhkey/MUSE-EXCHANGE-DESIGN.md, benchmark axis):

    verified useful decisions
    -------------------------
    bytes + compute + latency

Operationalized per job:
  verified  = 1 iff receipt result == "ok" AND a VERIFY gate passed in
              the governor decision trace (output independently verified)
  bytes     = input chars (job spec message content) + output chars
              (decision output content). chars ~= bytes for this text.
  compute   = measured cost from the receipt (0 on the free tier; shown,
              contributes 0 — the honest reading, not an invented weight)
  latency_s = finished_at - started_at from the receipt
  score     = verified / (bytes + LATENCY_BYTE_RATE * latency_s)

LATENCY_BYTE_RATE = 1000: one second of wall latency is priced the same
as one kilobyte transmitted. Tunable via --latency-rate; every component
is printed so the ranking can be re-weighted without re-running.

Why this metric: research shows token minimization alone misleads
(~1/3 the tokens, 20-80% slower on Terminal-Bench). A job that uses
fewer bytes but burns 10x the latency is not more intelligent per byte.

$0 only. Existing Exchange key. Real User-Agent. Read-only: receipts +
job specs, no submissions, no mutations.

LIMITATION (honest): the receipts endpoint returns the newest ~50
receipts and ignores offset pagination (verified 2026-10-06: pages
overlap 48-49/50). This harness therefore benchmarks a rolling window,
not complete history. For longitudinal benchmarking, snapshot --json
output per run and union the rows externally.

Usage: intelligence-per-byte.py [--json] [--latency-rate N]
"""
import json
import sys
import urllib.request
from datetime import datetime, timezone

EX = "https://lucidota-exchange.cutecity.workers.dev"
KEY = open("/home/hatch/.exchange_key").read().strip()
UA = "lucidota-intel-per-byte/1.0"

LATENCY_BYTE_RATE = 1000.0


def get(path):
    req = urllib.request.Request(
        EX + path,
        headers={"Authorization": f"Bearer {KEY}", "User-Agent": UA})
    return json.loads(urllib.request.urlopen(req, timeout=30).read())


def get_all_receipts():
    """Walk all pages. The endpoint caps at 50 per page; without this,
    older receipts fall out of the window as new ones arrive."""
    out = []
    seen_ids = set()
    offset = 0
    while True:
        page = get(f"/v1/receipts?limit=200&offset={offset}").get("receipts", [])
        if not page:
            break
        fresh = [r for r in page if r["id"] not in seen_ids]
        if not fresh:
            break  # endpoint ignores offset: same page again, stop
        for r in fresh:
            seen_ids.add(r["id"])
        out.extend(fresh)
        if len(page) < 50:
            break
        offset += len(page)
        if offset > 5000:  # sanity bound
            break
    # de-dupe by receipt id (pages may overlap)
    seen = {}
    for r in out:
        seen[r["id"]] = r
    return list(seen.values())


def parse_ts(s):
    if not s:
        return None
    try:
        return datetime.fromisoformat(s.replace("Z", "+00:00"))
    except ValueError:
        return None


def input_chars(job_id):
    """Input message chars from the job spec. None if unfetchable."""
    try:
        j = get(f"/v1/job/{job_id}")["job"]
        spec = json.loads(j["spec"])
        msgs = spec.get("input", {}).get("messages", [])
        return sum(len(m.get("content", "")) for m in msgs)
    except Exception:
        return None


def analyze():
    receipts = get_all_receipts()
    by_job = {}
    for r in receipts:
        by_job.setdefault(r["job_id"], []).append(r)

    rows = []
    skipped = 0
    for jid, rs in sorted(by_job.items()):
        rs.sort(key=lambda r: r.get("finished_at") or r.get("created_at") or "")
        last = rs[-1]
        if last.get("result") != "ok":
            continue
        try:
            dj = json.loads(last.get("decision", "{}"))
        except Exception:
            skipped += 1
            continue
        trace = dj.get("trace", [])
        verified = any(t.get("gate") == "VERIFY" and t.get("pass") for t in trace)
        out = dj.get("output") or {}
        out_chars = len(out.get("content") or "")
        usage = out.get("usage") or {}
        in_chars = input_chars(jid)
        if in_chars is None:
            skipped += 1
            continue
        t0 = parse_ts(last.get("started_at"))
        t1 = parse_ts(last.get("finished_at"))
        latency_s = (t1 - t0).total_seconds() if (t0 and t1) else None
        cost = last.get("cost", 0) or 0
        w = dj.get("worker")
        model = (w.get("model") if isinstance(w, dict) else w) or out.get("model") or "?"
        rows.append({
            "job": jid,
            "verified": verified,
            "bytes_in": in_chars,
            "bytes_out": out_chars,
            "bytes_total": in_chars + out_chars,
            "latency_s": latency_s,
            "cost": cost,
            "tokens_in": (usage.get("prompt_tokens")),
            "tokens_out": (usage.get("completion_tokens")),
            "resource": last.get("resource"),
            "model": model,
        })
    return rows, skipped


def main():
    as_json = "--json" in sys.argv
    rate = LATENCY_BYTE_RATE
    for i, a in enumerate(sys.argv):
        if a == "--latency-rate" and i + 1 < len(sys.argv):
            rate = float(sys.argv[i + 1])

    rows, skipped = analyze()
    for r in rows:
        denom = r["bytes_total"] + (rate * r["latency_s"] if r["latency_s"] else 0)
        r["score"] = (1.0 / denom) if (r["verified"] and denom > 0) else 0.0
    rows.sort(key=lambda r: r["score"], reverse=True)

    if as_json:
        print(json.dumps({"latency_byte_rate": rate, "rows": rows,
                          "skipped": skipped}, indent=1, default=str))
        return

    print(f"intelligence per byte  (latency priced at {rate:g} B/s; "
          f"score = verified / (bytes + rate*latency_s))")
    print(f"{'rank':<4} {'job':<34} {'verif':<5} {'KB_in':>7} {'KB_out':>7} "
          f"{'lat_s':>7} {'score':>12}")
    for i, r in enumerate(rows, 1):
        lat = f"{r['latency_s']:.0f}" if r["latency_s"] else "n/a"
        print(f"{i:<4} {r['job'][:33]:<34} {str(r['verified']):<5} "
              f"{r['bytes_in']/1024:>7.1f} {r['bytes_out']/1024:>7.1f} "
              f"{lat:>7} {r['score']:.3e}")

    # Fleet aggregates over verified jobs only.
    v = [r for r in rows if r["verified"]]
    tb = sum(r["bytes_total"] for r in v)
    tl = sum(r["latency_s"] for r in v if r["latency_s"])
    print(f"\nverified decisions: {len(v)}/{len(rows)} jobs "
          f"({skipped} skipped: no ok receipt or unfetchable spec)")
    if v:
        print(f"fleet bytes: {tb/1024:.1f} KB in+out, "
              f"fleet latency: {tl:.0f}s, "
              f"fleet score: {len(v)/(tb + rate*tl):.3e} decisions/byte-equiv")
        print(f"mean KB/decision: {tb/1024/len(v):.1f}, "
              f"mean latency/decision: {tl/len(v):.0f}s")


if __name__ == "__main__":
    main()
