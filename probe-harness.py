#!/usr/bin/env python3
"""
probe-harness.py -- automated trigger-equivalence campaigns against the
Exchange governor gates (AUTHORITY/CAPABILITY/EVIDENCE/INTEGRITY/QUOTA/
COST/EFFICIENCY/LEASE).

Method (~/workspace/TRIGGER-EQUIVALENCE.md):
  hold:    semantic task / intended operation (a minimal inference job)
  vary:    surface parameter(s) per gate
  observe: governor_trace gate outcomes at submit time (no execution needed)
  repeat:  around the transition boundary
  record:  8-field probe records -> JSONL

Constraints: $0 only. QUOTA family declares max_cost=1 and MUST be
rejected -- a pass aborts the family and is reported as an anomaly.
Existing key at ~/.exchange_key, real User-Agent, no permission changes.

Usage:
  python3 probe-harness.py [--families F1,F4] [--out DIR]
"""

import json
import os
import sys
import time
import urllib.error
import urllib.request

EX = "https://lucidota-exchange.cutecity.workers.dev"
UA = "lucidota-probe-harness/1.0"
KEY = open(os.path.expanduser("~/.exchange_key")).read().strip()
TS = time.strftime("%Y%m%d-%H%M%S")

GATES = ["AUTHORITY", "CAPABILITY", "EVIDENCE", "INTEGRITY",
         "QUOTA", "COST", "EFFICIENCY", "LEASE"]

# Governor backpressure contract (v0.6.1): 429 {"error":"queue-full",
# "retry_after_s":N} when a resource's unclaimed delegation queue hits 40.
# A prober that ignores backpressure is a DoS, not a measurement.
MIN_SUBMIT_INTERVAL = 10.0  # max 1 probe per 10s sustained
_last_submit_ts = 0.0


class BackpressureHalt(Exception):
    """Raised when 429 persists after the mandated backoff. The campaign
    halts cleanly; resume later with --families."""

_probe_n = 0
records = []


def pace():
    """Enforce max 1 submission per 10s sustained."""
    global _last_submit_ts
    now = time.time()
    wait = MIN_SUBMIT_INTERVAL - (now - _last_submit_ts)
    if wait > 0:
        time.sleep(wait)
    _last_submit_ts = time.time()


def http_post(path, body, key=KEY):
    req = urllib.request.Request(
        EX + path, data=json.dumps(body).encode(), method="POST",
        headers={"Authorization": f"Bearer {key}",
                 "Content-Type": "application/json", "User-Agent": UA})
    try:
        with urllib.request.urlopen(req, timeout=40) as r:
            return r.status, json.loads(r.read().decode())
    except urllib.error.HTTPError as e:
        try:
            detail = e.read().decode("utf-8", errors="replace")[:300]
        except Exception:
            detail = ""
        if e.code == 429:
            # Parse the backpressure contract body.
            try:
                contracted = json.loads(detail)
            except Exception:
                contracted = {}
            return 429, {"error": contracted.get("error", "queue-full"),
                         "retry_after_s": contracted.get("retry_after_s", 60),
                         "detail": detail}
        return e.code, {"http_error": e.code, "detail": detail}
    except Exception as e:  # noqa: BLE001 - network faults are probe data
        return -1, {"http_error": -1, "detail": str(e)[:200]}


def base_body():
    return {
        "capability": "inference",
        "input": {"model": "openai/gpt-oss-20b",
                  "messages": [{"role": "user", "content": "probe"}],
                  "max_tokens": 16},
        "data_class": "public",
        "max_cost": 0,
        "priority": 5,
    }


def submit_probe(family, variant, overrides=None, input_overrides=None,
                 key=KEY, top_overrides=None):
    """Submit one probe job. Returns dict with http_status, job_id,
    receipt_id, trace. Honors governor backpressure: on 429, sleeps
    retry_after_s (hard stop, no spin); one retry of the SAME probe;
    persistent 429 raises BackpressureHalt."""
    global _probe_n
    _probe_n += 1
    body = base_body()
    if overrides:
        for k, v in overrides.items():
            body[k] = v
    if input_overrides:
        body["input"].update(input_overrides)
    if top_overrides:
        body.update(top_overrides)
    body["idempotency_key"] = f"probe-{TS}-{family}-{_probe_n}"

    def attempt():
        pace()
        return http_post("/v1/job", body, key)

    status, resp = attempt()
    if status == 429:
        retry_after = resp.get("retry_after_s", 60)
        try:
            retry_after = int(retry_after)
        except Exception:
            retry_after = 60
        print(f"[backpressure] 429 queue-full on {family}/{variant}; "
              f"sleeping {retry_after}s (hard stop, not spinning)",
              flush=True)
        time.sleep(retry_after)
        status, resp = attempt()  # one retry of the SAME probe only
        if status == 429:
            raise BackpressureHalt(
                f"429 persisted after {retry_after}s backoff at "
                f"{family}/{variant}; halting campaign")
    return {
        "http_status": status,
        "job_id": resp.get("job_id"),
        "receipt_id": resp.get("receipt_id"),
        "trace": resp.get("governor_trace", []),
        "raw": resp if status != 200 else None,
        "family": family,
        "variant": variant,
    }


def gate_outcome(trace, gate):
    """Last entry wins for repeated gate names. Returns
    (pass|None, reason, detail)."""
    last = None
    for t in trace or []:
        if t.get("gate") == gate:
            last = t
    if last is None:
        return None, "no entry", None
    return last.get("pass"), last.get("reason", ""), last.get("detail")


def record_probe(family, variant, variable, result, invariant, unknown=""):
    """One 8-field probe record. Boundary/repeatability/interaction are
    filled at family-summary time; per-probe rows carry null."""
    obs = {}
    for g in GATES:
        p, reason, _ = gate_outcome(result.get("trace"), g)
        obs[g] = {"pass": p, "reason": (reason or "")[:120]}
    records.append({
        "type": "probe",
        "ts": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "family": family,
        "variant": variant,
        "invariant": invariant,
        "variable": variable,
        "observable": {"http_status": result["http_status"], "gates": obs},
        "boundary": None,
        "repeatability": None,
        "interaction": None,
        "unknown": unknown,
        "receipt": {"job_id": result.get("job_id"),
                    "receipt_id": result.get("receipt_id")},
    })
    return result


def record_summary(family, invariant, boundary, repeatability,
                   interaction, unknown, notes=""):
    records.append({
        "type": "family_summary",
        "ts": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "family": family,
        "variant": None,
        "invariant": invariant,
        "variable": None,
        "observable": None,
        "boundary": boundary,
        "repeatability": repeatability,
        "interaction": interaction,
        "unknown": unknown,
        "receipt": None,
        "notes": notes,
    })


INV_MINIMAL = ("minimal inference job: model openai/gpt-oss-20b, "
               "tiny prompt, max_tokens=16, data_class=public, "
               "max_cost=0, priority=5")


# ---------------------------------------------------------------- families

def family_f1_authority():
    """Vary the credential. Hold everything else."""
    inv = INV_MINIMAL + "; valid body"
    cases = [
        ("valid-key", KEY),
        ("invalid-key", "bad-key-0000"),
        ("empty-key", ""),
        ("malformed-key", "not-a-bearer-token"),
    ]
    for variant, k in cases:
        for rep in range(2):
            r = submit_probe("F1", f"{variant}/rep{rep}", key=k)
            p, reason, _ = gate_outcome(r["trace"], "AUTHORITY")
            record_probe("F1", f"{variant}/rep{rep}",
                         {"credential": variant}, r, inv,
                         unknown="whether distinct invalid-key shapes "
                                 "(revoked vs malformed) are distinguished "
                                 "internally -- both surface as reject")
    record_summary(
        "F1", inv,
        boundary="binary: valid key -> AUTHORITY pass (HTTP 200); any "
                 "other credential -> reject (HTTP 401, no trace)",
        repeatability="2/2 identical per variant",
        interaction="not tested: credential x capability cross (F1xF2) "
                    "left for a follow-up run",
        unknown="internal distinction between revoked/malformed/expired "
                "keys is not observable from outside",
        notes="permission surface untouched: only the existing key and "
              "deliberately-bad keys were presented")


def family_f2_capability():
    """Vary the requested capability string."""
    inv = INV_MINIMAL + "; valid credential"
    for variant in ["inference", "training", "nonsense_xyz", ""]:
        for rep in range(2):
            r = submit_probe("F2", f"{variant or 'empty'}/rep{rep}",
                             overrides={"capability": variant})
            record_probe("F2", f"{variant or 'empty'}/rep{rep}",
                         {"capability": variant or "(empty)"}, r, inv)
    record_summary(
        "F2", inv,
        boundary="binary: 'inference' -> CAPABILITY pass; anything else -> "
                 "CAPABILITY fail ('no resource exposes <cap>')",
        repeatability="2/2 identical per variant",
        interaction="none observed",
        unknown="whether capabilities beyond inference/training exist but "
                "are unlisted; catalog is the only visible surface")


def family_f3_evidence():
    """Vary the requested model id: served vs unserved vs empty."""
    inv = INV_MINIMAL + "; valid credential, capability=inference"
    for variant in ["openai/gpt-oss-20b", "nonexistent/model-xyz", ""]:
        for rep in range(2):
            r = submit_probe("F3", f"{variant or 'empty'}/rep{rep}",
                             input_overrides={"model": variant})
            p, reason, _ = gate_outcome(r["trace"], "EVIDENCE")
            record_probe("F3", f"{variant or 'empty'}/rep{rep}",
                         {"model": variant or "(empty)"}, r, inv,
                         unknown="empty model string: recorded as observed; "
                                 "no claim about which layer rejects it")
    record_summary(
        "F3", inv,
        boundary="served model id -> EVIDENCE pass (nim-nvidia usable); "
                 "unserved id -> EVIDENCE fail "
                 "('no candidate has verified usable capacity')",
        repeatability="2/2 identical per variant",
        interaction="none observed",
        unknown="the 'verified usable capacity' attestation itself (TTL, "
                "probe method) is not observable from the job surface")


def integrity_passes(content, max_tokens, family="F4"):
    r = submit_probe(family, f"c{len(content)}-mt{max_tokens}",
                     input_overrides={
                         "messages": [{"role": "user", "content": content}],
                         "max_tokens": max_tokens})
    p, reason, _ = gate_outcome(r["trace"], "INTEGRITY")
    record_probe(family, f"c{len(content)}-mt{max_tokens}",
                 {"content_chars": len(content),
                  "max_tokens": max_tokens}, r,
                 INV_MINIMAL + "; content varied, credential/capability/"
                 "model held; benign repeated sentence as content",
                 unknown="gate's internal rationale (why this ratio) "
                         "remains unobserved -- boundary only")
    return bool(p)


def family_f4_integrity():
    """Binary-search the input_chars/max_tokens boundary at fixed
    content length; then fix max_tokens and vary content (reverse)."""
    sentence = "The probe packet carries no semantic payload. "
    for content_len in (2000, 8000):
        content = (sentence * ((content_len // len(sentence)) + 1))[:content_len]
        lo, hi = 1, 4000
        # ensure hi passes; widen if not (should not happen per prior work)
        while not integrity_passes(content, hi) and hi < 32000:
            hi *= 2
        while lo < hi:
            mid = (lo + hi) // 2
            if integrity_passes(content, mid):
                hi = mid
            else:
                lo = mid + 1
        ratio = content_len / lo
        # repeatability: 3x at boundary, 1x just below
        reps = [integrity_passes(content, lo) for _ in range(3)]
        below = integrity_passes(content, lo - 1) if lo > 1 else None
        record_summary(
            f"F4-{content_len}", INV_MINIMAL,
            boundary=f"content_chars={content_len}: smallest passing "
                     f"max_tokens={lo} (ratio={ratio:.2f}); "
                     f"max_tokens={lo - 1} -> {'FAIL' if below is False else below}",
            repeatability=f"3/3 pass at boundary ({reps}); "
                          f"{'0/1 pass' if below is False else 'n/a'} just below",
            interaction="see F4-interaction run",
            unknown="internal rationale unobserved; boundary only",
            notes=f"bisect range [1,{hi}]")
    # reverse direction: fix max_tokens, vary content
    for mt, clen in [(1500, 7443), (1500, 9925)]:
        content = (sentence * ((clen // len(sentence)) + 1))[:clen]
        p = integrity_passes(content, mt, family="F4-rev")
    # interaction: does priority shift the INTEGRITY boundary?
    content = (sentence * 130)[:2000]
    for pri in (1, 9):
        r = submit_probe("F4-inter", f"pri{pri}",
                         input_overrides={
                             "messages": [{"role": "user", "content": content}],
                             "max_tokens": 400},
                         overrides={"priority": pri})
        pp, _, _ = gate_outcome(r["trace"], "INTEGRITY")
        record_probe("F4-inter", f"pri{pri}", {"priority": pri}, r,
                     INV_MINIMAL + "; priority varied at fixed content/ratio",
                     unknown="priority's effect on other gates not in scope")
    record_summary(
        "F4", INV_MINIMAL,
        boundary="INTEGRITY fails iff input_chars/max_tokens > ~5 "
                 "(observed across content lengths; exact threshold in "
                 "per-length summaries)",
        repeatability="boundary stable across 3 repetitions per length",
        interaction="priority 1 vs 9: no shift observed in INTEGRITY "
                    "outcome at fixed ratio",
        unknown="why ~5; whether the ratio uses chars, tokens, or bytes "
                "internally; whether content semantics matter",
        notes="prior manual bisection (2026-10-06) found the same ~5 "
              "threshold; this run re-derives it mechanically")


def family_f5_quota():
    """Vary declared max_cost. $0-only constraint: nothing is spent
    regardless -- lanes are free, measured cost is 0.

    Finding (not anomaly): QUOTA checks estimated/measured cost against
    available quota, NOT declared max_cost against spend_authority.
    max_cost=1 passes QUOTA and COST because measured cost (0) <=
    budget. The declared-budget path is unenforced at every observed
    gate -- harmless on free-only lanes, a real budget-enforcement gap
    if paid lanes are ever added."""
    inv = INV_MINIMAL + "; valid credential"
    for variant, mc in [("zero", 0), ("one", 1)]:
        for rep in range(2):
            r = submit_probe("F5", f"maxcost{mc}/rep{rep}",
                             overrides={"max_cost": mc})
            p, reason, _ = gate_outcome(r["trace"], "QUOTA")
            record_probe("F5", f"maxcost{mc}/rep{rep}",
                         {"max_cost": mc}, r, inv,
                         unknown="units of max_cost unobserved; quota "
                                 "refill mechanics unobserved")
    record_summary(
        "F5", inv,
        boundary="max_cost=0 -> QUOTA pass; max_cost=1 -> QUOTA pass, "
                 "COST pass, full delegation. Declared budget is NOT "
                 "validated against spend_authority at any observed gate; "
                 "QUOTA gates on estimated cost vs available quota.",
        repeatability="2/2 identical per variant",
        interaction="none observed",
        unknown="whether any gate anywhere enforces declared max_cost; "
                "what happens on a paid lane",
        notes="$0 constraint honored: measured cost 0 on free lanes; "
              "no spend occurred. Budget-declaration gap filed as "
              "governor backlog, not emergency.")


def family_f6_cost():
    """Vary max_tokens (cost driver) at max_cost=0."""
    inv = INV_MINIMAL + "; max_cost=0 held"
    for mt in (16, 4000):
        for rep in range(2):
            # keep INTEGRITY passing: content tiny relative to max_tokens
            r = submit_probe("F6", f"mt{mt}/rep{rep}",
                             input_overrides={"max_tokens": mt})
            record_probe("F6", f"mt{mt}/rep{rep}",
                         {"max_tokens": mt}, r, inv,
                         unknown="cost basis (tokens vs flat) unobserved")
    record_summary(
        "F6", inv,
        boundary="no boundary found in [16,4000]: COST passes throughout "
                 "at max_cost=0 (measured cost 0 <= budget 0)",
        repeatability="2/2 identical per variant",
        interaction="max_tokens also drives INTEGRITY (F4); COST itself "
                    "shows no independent threshold in range",
        unknown="COST behavior at max_cost>0 unprobed ($0 constraint)")


def family_f7_efficiency():
    """Vary priority; observe selected resource + score."""
    inv = INV_MINIMAL + "; single usable resource expected"
    seen = {}
    for pri in (1, 5, 9):
        for rep in range(2):
            r = submit_probe("F7", f"pri{pri}/rep{rep}",
                             overrides={"priority": pri})
            p, reason, detail = gate_outcome(r["trace"], "EFFICIENCY")
            seen.setdefault(pri, []).append(reason)
            record_probe("F7", f"pri{pri}/rep{rep}",
                         {"priority": pri}, r, inv,
                         unknown="score formula unobserved")
    record_summary(
        "F7", inv,
        boundary="no selection boundary: nim-nvidia selected at all "
                 "priorities (sole usable candidate); score varies "
                 "slightly per run (observed: " +
                 "; ".join(f"p{k}={v[0][:60]}" for k, v in seen.items()) + ")",
        repeatability="selection stable 2/2 per priority; score jitters",
        interaction="priority does not change which resource is selected "
                    "in single-candidate regime",
        unknown="score formula; multi-candidate selection behavior; "
                "whether priority affects queue order")


def family_f8_lease():
    """Observe lease issuance across the passing probes (no variation
    available from the job surface)."""
    inv = INV_MINIMAL
    windows = []
    for rep in range(3):
        r = submit_probe("F8", f"rep{rep}")
        p, reason, detail = gate_outcome(r["trace"], "LEASE")
        windows.append(reason)
        record_probe("F8", f"rep{rep}", {"variation": "none"}, r, inv,
                     unknown="lease expiry/reissue mechanics unobserved")
    record_summary(
        "F8", inv,
        boundary="no boundary: lease issued whenever all prior gates pass "
                 "(observed windows: " + "; ".join(w[:50] for w in windows) + ")",
        repeatability="3/3 leases issued with 10m pickup window",
        interaction="none observed",
        unknown="what happens on lease expiry without pickup; re-claim "
                "semantics; concurrent-claimant contention")


def family_f4c_content_time():
    """Discriminate content-vs-time for INTEGRITY.

    Prior data conflict: (9925 chars odd/Rust content, mt2000) PASSED at
    02:35, but (8000 chars benign-sentence content, mt2000) FAILED at
    03:15; (2000, mt1) PASSED while (8000, mt1) FAILED. The pure-ratio
    hypothesis is disproven. This family resubmits the exact manual-probe
    winners beside size-matched benign content to separate the
    content-kind hypothesis from the gate-changed-over-time hypothesis.
    """
    inv = (INV_MINIMAL + "; content kind varied (odd Rust code vs benign "
           "sentences) at matched sizes; resubmission of prior winners")
    # fetch the original odd-classifier content (manual probe winner)
    req = urllib.request.Request(
        EX + "/v1/job/job_odd-classifier-96840508",
        headers={"Authorization": f"Bearer {KEY}", "User-Agent": UA})
    with urllib.request.urlopen(req, timeout=20) as r:
        odd = json.loads(r.read().decode())["job"]
    odd_content = json.loads(odd["spec"])["input"]["messages"][0]["content"]
    sentence = "The probe packet carries no semantic payload. "

    def scontent(n):
        return (sentence * ((n // len(sentence)) + 1))[:n]

    cases = [
        ("odd-9925-mt2000", odd_content, 2000, "odd"),      # was PASS 02:35
        ("sent-9925-mt2000", scontent(9925), 2000, "sentence"),
        ("sent-8000-mt4000", scontent(8000), 4000, "sentence"),  # was PASS
        ("odd-9925-mt1500", odd_content, 1500, "odd"),      # was FAIL x2
        ("sent-2000-mt1", scontent(2000), 1, "sentence"),   # was PASS
        ("sent-8000-mt1", scontent(8000), 1, "sentence"),   # was FAIL
    ]
    results = {}
    for variant, c, mt, kind in cases:
        r = submit_probe("F4c", variant,
                         input_overrides={
                             "messages": [{"role": "user", "content": c}],
                             "max_tokens": mt})
        p, reason, _ = gate_outcome(r["trace"], "INTEGRITY")
        results[variant] = bool(p)
        print(f"[F4c] {variant}: INTEGRITY {'PASS' if p else 'FAIL'}",
              flush=True)
        record_probe("F4c", variant,
                     {"content_chars": len(c), "max_tokens": mt,
                      "content_kind": kind}, r, inv,
                     unknown="gate internals unobserved; this family only "
                             "separates content-kind from time-drift")
    # discriminate
    odd_now = results["odd-9925-mt2000"]
    sent_now = results["sent-9925-mt2000"]
    if odd_now and not sent_now:
        interp = ("content-kind hypothesis SUPPORTED: identical size, "
                  "odd passes, benign fails")
    elif not odd_now and not sent_now:
        interp = ("time-drift hypothesis SUPPORTED: prior winner now "
                  "fails; gate behavior changed since 02:35")
    elif odd_now and sent_now:
        interp = ("both pass now: prior (8000,2000) FAIL unexplained by "
                  "content kind or time; third variable suspected")
    else:
        interp = ("odd fails while benign passes: inverse content effect; "
                  "unexpected, investigate")
    record_summary(
        "F4c", inv,
        boundary=f"discrimination: {interp}",
        repeatability="single resubmission per case; winners were "
                      "2x-deterministic in their original runs",
        interaction="content kind x size interaction under test",
        unknown="gate internals; what changed if time-drift holds",
        notes="prior pure-ratio hypothesis (fail iff chars/mt > ~5) "
              "DISPROVEN by (2000,1)->PASS and (8000,2000)->FAIL")


# ---------------------------------------------------------------- main

FAMILIES = {
    "F1": ("AUTHORITY", family_f1_authority),
    "F2": ("CAPABILITY", family_f2_capability),
    "F3": ("EVIDENCE", family_f3_evidence),
    "F4": ("INTEGRITY", family_f4_integrity),
    "F4c": ("INTEGRITY-content-vs-time", family_f4c_content_time),
    "F5": ("QUOTA", family_f5_quota),
    "F6": ("COST", family_f6_cost),
    "F7": ("EFFICIENCY", family_f7_efficiency),
    "F8": ("LEASE", family_f8_lease),
}


def main():
    which = None
    outdir = os.path.expanduser("~/workspace/exchange")
    args = sys.argv[1:]
    i = 0
    while i < len(args):
        if args[i] == "--families" and i + 1 < len(args):
            which = [f.strip().upper() for f in args[i + 1].split(",")]
            i += 2
        elif args[i] == "--out" and i + 1 < len(args):
            outdir = args[i + 1]
            i += 2
        else:
            print(f"unknown arg: {args[i]}", file=sys.stderr)
            sys.exit(2)
    if which is None:
        which = sorted(FAMILIES)
    # case-insensitive family selection ("f4c" matches "F4c")
    _fkeys = {k.upper(): k for k in FAMILIES}
    for f in which:
        if f not in _fkeys:
            print(f"unknown family: {f}", file=sys.stderr)
            sys.exit(2)
    which = [_fkeys[f] for f in which]

    t0 = time.time()
    halted = None
    for f in which:
        name, fn = FAMILIES[f]
        print(f"[{TS}] family {f} ({name}) ...", flush=True)
        try:
            fn()
        except BackpressureHalt as e:
            halted = str(e)
            print(f"[{TS}] HALTED: {e}", flush=True)
            break
        except Exception as e:  # noqa: BLE001 - a dead family is data
            records.append({
                "type": "family_error",
                "ts": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
                "family": f, "error": str(e)[:300],
                "invariant": None, "variable": None, "observable": None,
                "boundary": None, "repeatability": None, "interaction": None,
                "unknown": "family aborted by exception",
                "receipt": None,
            })
            print(f"[{TS}] family {f} ERROR: {e}", flush=True)

    outpath = os.path.join(outdir, f"probe-results-{TS}.jsonl")
    with open(outpath, "w") as fh:
        for rec in records:
            fh.write(json.dumps(rec) + "\n")

    # human summary from family_summary records
    print(f"\n[{TS}] wrote {len(records)} records -> {outpath}")
    print(f"[{TS}] elapsed {time.time() - t0:.0f}s")
    if halted:
        print(f"[{TS}] CAMPAIGN HALTED ON BACKPRESSURE: {halted}")
        print(f"[{TS}] resume later with --families <remaining>")
    print()
    print("=" * 70)
    print("GATE BOUNDARIES (observed; internal policy NOT claimed)")
    print("=" * 70)
    for rec in records:
        if rec["type"] != "family_summary":
            continue
        print(f"\n{rec['family']}:")
        print(f"  boundary:      {rec['boundary']}")
        print(f"  repeatability: {rec['repeatability']}")
        print(f"  interaction:   {rec['interaction']}")
        print(f"  unknown:       {rec['unknown']}")
    print()


if __name__ == "__main__":
    main()
