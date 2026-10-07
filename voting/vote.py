#!/usr/bin/env python3
"""Multi-model voting on the Exchange (prototype).

A ballot = N ordinary Exchange jobs (one per voter model), executed by the
existing claimant workers, tallied client-side into one verdict receipt.
Workers never know they are voting — voting is a governor composition
pattern. See voting/DESIGN.md.

Usage:
  vote.py verdict "Proposition text" [--voters m1,m2] [--deadline 600]
  vote.py answer  "Question text"    [--voters m1,m2] [--deadline 600]
  vote.py --probe                    # re-check which NIM models are live

Exit: 0 decided · 2 INCONCLUSIVE · 3 EXPIRED · 1 error.
$0: NIM free tier only; no new secrets (uses ~/.exchange_key like other scripts).
"""
import sys, os, json, time, re, hashlib, urllib.request

EXCHANGE = "https://lucidota-exchange.cutecity.workers.dev"
UA = "lucidota-voting/1.0"
DEFAULT_VOTERS = [
    "openai/gpt-oss-20b",
    "z-ai/glm-5.3-flash",
    "nvidia/nemotron-3.5-lightning-30b-a3b",
]
BALLOT_DIR = os.path.expanduser("~/workspace/exchange/voting/ballots")

VERDICT_SUFFIX = """
Evaluate the proposition above. Think step by step, then give your
final verdict on its own line at the very end, exactly one word:
AUTHORIZED or UNKNOWN or DENIED.

Final verdict:"""

ANSWER_SUFFIX = """
Answer with the final answer only, no explanation:"""


def xchg(method, path, body=None):
    key = open(os.path.expanduser("~/.exchange_key")).read().strip()
    req = urllib.request.Request(
        EXCHANGE + path, method=method,
        data=json.dumps(body).encode() if body is not None else None,
        headers={"Authorization": f"Bearer {key}",
                 "Content-Type": "application/json", "User-Agent": UA})
    try:
        with urllib.request.urlopen(req, timeout=60) as r:
            return json.loads(r.read().decode()), r.status
    except urllib.error.HTTPError as e:
        try:
            return json.loads(e.read().decode()), e.code
        except Exception:
            return {"error": f"http {e.code}"}, e.code


def slug(model):
    return re.sub(r"[^a-z0-9]+", "-", model.lower()).strip("-")


def submit_vote(ballot_id, voter, prompt):
    spec = {
        "organelle": "voting",
        "capability": "inference",
        "input": {
            "messages": [{"role": "user", "content": prompt}],
            "model": voter,
            "temperature": 0,
            "max_tokens": 512,
        },
        "idempotency_key": f"ballot_{ballot_id}_{slug(voter)}",
        "data_class": "public",
        "max_cost": 0,
    }
    r, st = xchg("POST", "/v1/job", spec)
    return r, st


def job_output(job_id):
    """Return (state, content|None, receipt_id|None, model_actual|None, latency)."""
    r, st = xchg("GET", f"/v1/job/{job_id}")
    if st != 200:
        return "unknown", None, None, None, None
    state = (r.get("job") or {}).get("state")
    model_actual = None
    for ex in r.get("executions", []) or []:
        model_actual = ex.get("actual_model") or model_actual
    for rc in r.get("receipts", []) or []:
        if rc.get("result") not in ("ok",):
            continue
        try:
            dec = json.loads(rc.get("decision") or "{}")
        except Exception:
            continue
        out = dec.get("output") or {}
        content = out.get("content")
        if content:
            return "done", content, rc.get("id"), model_actual, None
    return state, None, None, model_actual, None


def extract_verdict(content):
    tail = content[-300:]
    hits = re.findall(r"\b(AUTHORIZED|UNKNOWN|DENIED)\b", tail)
    return hits[-1] if hits else "INVALID"


def normalize_answer(content):
    t = content.strip().lower()
    t = re.sub(r"\s+", " ", t)
    t = re.sub(r"[.\u201d\"']+$", "", t).strip()
    return t[:200]


def tally(ballot_type, votes):
    """votes: list of dicts with vote, valid, model... Returns (verdict, detail)."""
    valid = [v for v in votes if v["valid"]]
    n = len(votes)
    quorum = n // 2 + 1
    if len(valid) < quorum:
        return "EXPIRED", {"reason": f"quorum {quorum} not reached ({len(valid)} valid)"}
    if ballot_type == "verdict":
        counts = {}
        for v in valid:
            counts[v["vote"]] = counts.get(v["vote"], 0) + 1
        top, topn = max(counts.items(), key=lambda kv: kv[1])
        if topn > len(valid) / 2:
            return top, {"counts": counts, "majority": True}
        return "INCONCLUSIVE", {"counts": counts, "majority": False,
                                "note": "no majority -> UNKNOWN per design"}
    # answer: normalized exact-match clustering
    clusters = {}
    for v in valid:
        clusters.setdefault(v["normalized"], []).append(v["model"])
    top_norm, members = max(clusters.items(), key=lambda kv: len(kv[1]))
    if len(members) > len(valid) / 2:
        return top_norm, {"clusters": {k: len(m) for k, m in clusters.items()},
                          "majority": True}
    return "INCONCLUSIVE", {"clusters": {k: len(m) for k, m in clusters.items()},
                            "majority": False}


def probe_models():
    sys.path.insert(0, "/opt/hatch/skills/skill-creator/bin")
    from dynamic_credentials import add_surrogate_to_request
    cands = DEFAULT_VOTERS + ["z-ai/glm-5.3"]
    live = []
    for m in cands:
        body = json.dumps({"model": m, "messages": [{"role": "user", "content": "OK"}],
                           "max_tokens": 4, "temperature": 0}).encode()
        req = urllib.request.Request("https://integrate.api.nvidia.com/v1/chat/completions",
                                     data=body, method="POST",
                                     headers={"Content-Type": "application/json"})
        add_surrogate_to_request(req, "custom.nvidia-nim",
                                 allowed_hosts=["integrate.api.nvidia.com"])
        try:
            urllib.request.urlopen(req, timeout=20)
            live.append(m)
            print(f"LIVE {m}")
        except Exception as e:
            print(f"DEAD {m} ({str(e)[:50]})")
    return live


def main(argv):
    if "--probe" in argv:
        probe_models()
        return 0
    if len(argv) < 3 or argv[1] not in ("verdict", "answer"):
        print(__doc__)
        return 1
    ballot_type, text = argv[1], argv[2]
    voters, deadline = list(DEFAULT_VOTERS), 600
    for i, a in enumerate(argv):
        if a == "--voters" and i + 1 < len(argv):
            voters = [v.strip() for v in argv[i + 1].split(",") if v.strip()]
        if a == "--deadline" and i + 1 < len(argv):
            deadline = int(argv[i + 1])

    suffix = VERDICT_SUFFIX if ballot_type == "verdict" else ANSWER_SUFFIX
    prompt = text.rstrip() + "\n" + suffix
    prompt_hash = hashlib.sha256(prompt.encode()).hexdigest()[:16]
    ballot_id = f"bal_{prompt_hash}_{int(time.time()) % 100000:05d}"
    print(f"ballot {ballot_id} type={ballot_type} voters={len(voters)} "
          f"deadline={deadline}s", flush=True)

    jobs = {}
    for voter in voters:
        r, st = submit_vote(ballot_id, voter, prompt)
        if st == 429:
            print(f"  {voter}: REJECTED by backpressure "
                  f"(queue {r.get('queue_depth')}) — aborting ballot", flush=True)
            return 1
        if st != 200 or "job_id" not in r:
            print(f"  {voter}: submit failed {st} {str(r)[:150]}", flush=True)
            return 1
        jobs[voter] = r["job_id"]
        print(f"  {voter}: job {r['job_id']} state={r.get('state')}", flush=True)

    # Wait for the claimant workers to execute.
    t0, results = time.time(), {}
    while time.time() - t0 < deadline:
        pending = [v for v in voters if v not in results]
        if not pending:
            break
        for v in pending:
            state, content, rcpt, m_actual, _ = job_output(jobs[v])
            if state == "done" and content:
                results[v] = {"content": content, "receipt_id": rcpt,
                              "model_actual": m_actual}
            elif state in ("failed", "blocked"):
                results[v] = {"content": None, "failed": state}
        time.sleep(10)

    votes = []
    for v in voters:
        r = results.get(v)
        if not r or not r.get("content"):
            votes.append({"model": v, "vote": None, "valid": False,
                          "reason": r.get("failed", "no output by deadline")
                          if r else "no output by deadline",
                          "job_id": jobs[v]})
            continue
        if ballot_type == "verdict":
            vote = extract_verdict(r["content"])
            votes.append({"model": v, "vote": vote, "valid": vote != "INVALID",
                          "job_id": jobs[v], "receipt_id": r.get("receipt_id"),
                          "model_actual": r.get("model_actual"),
                          "raw_tail": r["content"][-150:]})
        else:
            norm = normalize_answer(r["content"])
            votes.append({"model": v, "vote": norm, "normalized": norm,
                          "valid": bool(norm), "job_id": jobs[v],
                          "receipt_id": r.get("receipt_id"),
                          "model_actual": r.get("model_actual"),
                          "raw": r["content"][:150]})

    verdict, detail = tally(ballot_type, votes)
    ballot = {
        "ballot_id": ballot_id, "type": ballot_type,
        "proposition": text, "prompt_hash": prompt_hash,
        "voters": voters, "quorum": len(voters) // 2 + 1,
        "deadline_s": deadline,
        "votes": votes, "tally": detail, "verdict": verdict,
        "correlation_domain": ["nvidia-nim"],
        "weight_basis": "unweighted — insufficient reliability data",
        "decided_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
    }
    os.makedirs(BALLOT_DIR, exist_ok=True)
    path = os.path.join(BALLOT_DIR, f"{ballot_id}.json")
    json.dump(ballot, open(path, "w"), indent=2)
    print(f"\nverdict: {verdict}")
    print(json.dumps(detail, indent=2)[:800])
    print(f"ballot receipt: {path}")
    return 0 if verdict not in ("INCONCLUSIVE", "EXPIRED") else \
        (2 if verdict == "INCONCLUSIVE" else 3)


if __name__ == "__main__":
    sys.exit(main(sys.argv))
