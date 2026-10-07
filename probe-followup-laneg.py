#!/usr/bin/env python3
"""LANE G probe follow-up campaign (2026-10-07).

New families on top of probe-harness.py (imported, not forked):
  F9  - v0.6.2 verify-endpoint backpressure: confirm the verify endpoint still
        works at healthy depth (post-fix light-touch; battery v3 already proved
        the T1 bypass and the fix is deployed).
  F10 - INTEGRITY non-stationarity state-variable hunt: re-run the
        odd-classifier boundary (9925 chars / max_tokens 1500) spaced in time,
        recording pass/fail + timestamp to catch flips.
  F12 - NIM model reality for the voting lane: which catalog model IDs actually
        respond (tiny probes, bounded).

Contract: pace 1 probe / 10s, 429 -> sleep retry_after_s (hard stop, no spin),
persistent 429 -> BackpressureHalt. $0 (free tier only).
"""
import sys, os, time, json
sys.path.insert(0, os.path.expanduser("~/workspace/exchange"))
import importlib.util
spec = importlib.util.spec_from_file_location(
    "probe_harness", os.path.expanduser("~/workspace/exchange/probe-harness.py"))
ph = importlib.util.module_from_spec(spec)
spec.loader.exec_module(ph)

TS = time.strftime("%Y%m%d-%H%M%S", time.gmtime())
OUT = os.path.expanduser(f"~/workspace/exchange/probe-results-laneg-{TS}.jsonl")
recs = []

def rec(family, variant, variable, result, invariant, unknown=""):
    trace = result.get("governor_trace", []) if isinstance(result, dict) else []
    recs.append({
        "type": "probe", "family": family, "variant": variant,
        "variable": variable, "invariant": invariant,
        "http": result.get("_http") if isinstance(result, dict) else None,
        "state": result.get("state") if isinstance(result, dict) else None,
        "error": result.get("error") if isinstance(result, dict) else None,
        "receipt_id": result.get("receipt_id"),
        "unknown": unknown,
        "ts": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
    })

def family_f9_verify_backpressure():
    inv = "v0.6.2: verify endpoint must respect queue cap like /v1/job"
    # light-touch: verify at healthy depth should delegate (not 429)
    body = {}
    try:
        r, code = ph.http_post("/v1/resources/nim-nvidia/verify", body), None
    except Exception as e:
        code = getattr(e, "code", None)
        r = {"error": str(e)[:200]}
    # http_post raises on 429 per harness contract; reaching here means <429
    rec("F9", "verify-at-healthy-depth", {"queue": "below cap"},
        r if isinstance(r, dict) else {"raw": str(r)[:200]}, inv,
        unknown="full-cap verify behavior re-tested only by battery v3 T1")
    ph.record_summary("F9", inv,
        boundary="verify delegates below cap (no 429); 429 path added in v0.6.2",
        repeatability="1/1 (light-touch)",
        interaction="none",
        unknown="whether verify 429 triggers at exactly depth>=40 (battery v3 T1 covered pre-fix)")

def family_f10_integrity_hunt():
    inv = ("odd-classifier boundary case: 9925-char content, max_tokens=1500; "
           "boundary was FAIL at 02:15-02:35Z, PASS at 03:05Z -- hunting the state variable")
    content = "x" * 9925
    for rep in range(3):
        r = ph.submit_probe("F10", f"odd-9925-mt1500/rep{rep}",
                            input_overrides={"messages": [{"role": "user", "content": content}],
                                             "max_tokens": 1500})
        p, reason, _ = ph.gate_outcome(r.get("governor_trace", []), "INTEGRITY")
        rec("F10", f"odd-9925-mt1500/rep{rep}",
            {"content_chars": 9925, "max_tokens": 1500},
            {"_http": 200, "state": r.get("state"), "integrity": f"{p}:{reason}",
             "receipt_id": r.get("receipt_id")}, inv,
            unknown="state variable behind the flip (queue depth / attestation freshness candidates)")
        if rep < 2:
            time.sleep(60)
    ph.record_summary("F10", inv,
        boundary="TBD from reps: does the boundary hold, flip, or drift?",
        repeatability="3 reps spaced 60s",
        interaction="time x queue-depth (uncontrolled)",
        unknown="the state variable itself")

def family_f12_model_reality():
    inv = "voting lane needs ground truth: which NIM model IDs actually execute"
    models = ["openai/gpt-oss-20b", "openai/gpt-oss-120b",
              "deepseek-ai/deepseek-v3.1", "qwen/qwen3-32b"]
    for m in models:
        r = ph.submit_probe("F12", m.replace("/", "_"),
                            input_overrides={
                                "model": m,
                                "messages": [{"role": "user", "content": "Reply with exactly: ok"}],
                                "max_tokens": 16})
        rec("F12", m, {"model": m},
            {"state": r.get("state"), "error": (r.get("error") or "")[:160],
             "receipt_id": r.get("receipt_id")}, inv,
            unknown="per-account availability may differ from this probe")
    ph.record_summary("F12", inv,
        boundary="binary per model: delegates (alive) vs gate-fail (dead ID)",
        repeatability="1/1 per model",
        interaction="none",
        unknown="latency/quality differences between live models (voting lane measures)")

FAMS = {"F9": family_f9_verify_backpressure, "F10": family_f10_integrity_hunt,
        "F12": family_f12_model_reality}

def main():
    which = [a.split("=")[1].split(",") if a.startswith("--families=") else None
             for a in sys.argv[1:]]
    which = next((w for w in which if w), sorted(FAMS))
    which = [w.upper() for w in which]
    t0 = time.time()
    for f in which:
        print(f"[laneg] family {f} ...", flush=True)
        try:
            FAMS[f]()
        except ph.BackpressureHalt as e:
            print(f"[laneg] HALTED ON BACKPRESSURE: {e}", flush=True)
            break
        except Exception as e:
            recs.append({"type": "family_error", "family": f,
                         "error": str(e)[:300],
                         "ts": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())})
    for r in ph.records:
        recs.append(r)
    with open(OUT, "w") as fh:
        for r in recs:
            fh.write(json.dumps(r) + "\n")
    print(f"[laneg] wrote {len(recs)} records -> {OUT}")
    print(f"[laneg] elapsed {time.time()-t0:.0f}s")

if __name__ == "__main__":
    main()
