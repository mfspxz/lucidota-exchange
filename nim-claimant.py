#!/usr/bin/env python3
"""NIM worker — Worker Protocol v1 implementation.

BORING BY DESIGN. This worker does exactly five things:
  PICKUP    → POST /v1/claim (worker-scoped auth — never the Exchange key)
  VALIDATE  → delegation fields present, lease not expired
  EXECUTE   → exactly one NIM call with the vault-held credential
  CAPTURE   → raw result, no interpretation
  REPORT    → POST /v1/complete with exactly one terminal outcome

It never: retries, fails over to another provider, chooses models, asserts
cost, extends its lease, or decides what happens next. The governor owns
authority; this process owns execution. Neither impersonates the other.

The credential never leaves the vault: it is attached via surrogate by the
nvidia-nim skill, exactly like every other skill CLI. The delegation carries
a credential_ref (opaque); the secret itself never travels.
"""
import json
import os
import sys
import time
import urllib.error
import urllib.request

EXCHANGE = "https://lucidota-exchange.cutecity.workers.dev"
RESOURCE = "nim-nvidia"
WORKER_ID_FILE = os.environ.get(
    "WORKER_CRED_FILE", os.path.expanduser("~/.exchange_worker_nim"))
DEFAULT_MODEL = "openai/gpt-oss-20b"  # proven 200 on this account 2026-10-06

_wid, _wsec = open(WORKER_ID_FILE).read().strip().split("\n")[:2]


def xchg(path, body):
    req = urllib.request.Request(
        EXCHANGE + path, data=json.dumps(body).encode(), method="POST",
        headers={"Content-Type": "application/json",
                 "X-Worker-Id": _wid, "X-Worker-Secret": _wsec,
                 "User-Agent": "lucidota-exchange-worker/1.0"},
    )
    try:
        with urllib.request.urlopen(req, timeout=60) as r:
            return json.loads(r.read()), r.status
    except urllib.error.HTTPError as e:
        try:
            return json.loads(e.read().decode("utf-8", errors="replace")), e.code
        except Exception:
            return {"error": f"HTTP {e.code}"}, e.code


def load_nim():
    from importlib.machinery import SourceFileLoader
    p = os.path.expanduser("~/workspace/skills/nvidia-nim/bin/nvidia-nim")
    return SourceFileLoader("nimcli", p).load_module()


# Mechanical outcome table. Fixed, no judgment: HTTP status → terminal.
# The worker reports what it mechanically observed; the governor classifies,
# verifies, and decides what happens next.
def to_outcome(http_status, has_content):
    if http_status == 200 and has_content:
        return "SUCCEEDED", None
    if http_status == 429:
        return "FAILED_RETRYABLE", "RATE_LIMITED"
    if http_status in (401, 403):
        return "FAILED_PERMANENT", "AUTH_ERROR"
    if http_status in (404, 410):
        return "FAILED_PERMANENT", "NOT_FOUND"
    if http_status == 400:
        return "FAILED_PERMANENT", "BAD_REQUEST"
    if http_status and 500 <= http_status < 600:
        return "FAILED_RETRYABLE", "SERVER_ERROR"
    if http_status == 200:
        return "FAILED_RETRYABLE", "BAD_RESPONSE"  # 200 with no usable content
    return "FAILED_RETRYABLE", "NETWORK"  # timeouts, DNS, connection resets


def run_inference(nim, model, messages, max_tokens, temperature=0.7):
    prompt = "\n".join(f"{m.get('role', 'user')}: {m.get('content', '')}" for m in messages)
    t0 = time.time()
    status, raw = None, {}
    try:
        raw = nim.api("POST", "/chat/completions", {
            "model": model,
            "messages": [{"role": "user", "content": prompt}],
            "max_tokens": max_tokens, "temperature": temperature, "stream": False,
        })
        status = 200
    except urllib.error.HTTPError as e:
        status = e.code
        try:
            raw = {"error": e.read().decode("utf-8", errors="replace")[:300]}
        except Exception:
            raw = {"error": f"HTTP {e.code}"}
    except Exception as e:
        raw = {"error": str(e)[:300]}
    lat = int((time.time() - t0) * 1000)

    content, model_actual, usage = None, None, None
    if status == 200 and isinstance(raw, dict):
        ch = (raw.get("choices") or [{}])[0]
        msg = ch.get("message") or {}
        # Reasoning models (gpt-oss) answer in reasoning_content with
        # content=null; others use content. Take what's real, never invent.
        content = msg.get("content") or msg.get("reasoning_content") or msg.get("reasoning")
        model_actual = raw.get("model")  # requested vs executed, bound by governor
        usage = raw.get("usage")  # token counts are the scarce resource; governor persists them
    outcome, error_class = to_outcome(status, bool(content))
    return {
        "worker_outcome": outcome,
        "error_class": error_class,
        "error": None if outcome == "SUCCEEDED" else str(raw.get("error", f"http {status}"))[:300],
        "output": {"content": content, "finish_reason": ((raw.get("choices") or [{}])[0].get("finish_reason")),
                   "usage": usage} if outcome == "SUCCEEDED" else None,
        "latency_ms": lat,
        "model_actual": model_actual,
    }


# DRAIN CAP: bound a single poll run so the hook never overruns its 120s
# interval. 25 jobs x ~3s NIM latency ~= 75s worst case per worker.
DRAIN_CAP = 25

def run_one(nim):
    """PICKUP -> VALIDATE -> EXECUTE -> CAPTURE -> REPORT, one job.
    Returns (did_work: bool, fatal: bool)."""
    claim, _ = xchg("/v1/claim", {"resource_id": RESOURCE})
    if not claim.get("claimed"):
        return False, False
    d = claim["delegation"]
    # VALIDATE — structural only. The worker cannot fix a bad delegation,
    # only refuse it honestly.
    for f in ("claim_id", "input", "lease_expiry", "policy_snapshot"):
        if f not in d or d[f] is None:
            print(json.dumps({"error": f"delegation missing {f} — refusing"}))
            return True, True
    if d["lease_expiry"] < time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()):
        print(json.dumps({"error": "delegation already expired — refusing"}))
        return True, True
    inp = d["input"] or {}
    model = inp.get("model") or d.get("requested_model") or DEFAULT_MODEL
    messages = inp.get("messages") or [{"role": "user", "content": str(inp)}]
    # EXECUTE — exactly once. No retries, no failover. The governor replans.
    res = run_inference(nim, model, messages, inp.get("max_tokens", 512),
                      inp.get("temperature", 0.7))
    # CAPTURE + REPORT — one terminal outcome. Cost is never asserted here;
    # the governor's accounting row is the books.
    done, _ = xchg("/v1/complete", {"claim_id": d["claim_id"], **res})
    print(json.dumps({"completed": claim["job_id"], "outcome": res["worker_outcome"],
                      "model": model, "state": done.get("state"),
                      "idempotent": done.get("idempotent", False)}),
          flush=True)
    return True, False


def main():
    nim = load_nim()
    # DRAIN — claim until no work or cap. The governor's claim is atomic,
    # so parallel workers never double-execute; each drains independently.
    drained, fatal = 0, False
    while drained < DRAIN_CAP:
        did_work, bad = run_one(nim)
        if bad:
            fatal = True
            break
        if not did_work:
            break
        drained += 1
    print(json.dumps({"drained": drained, "cap": DRAIN_CAP,
                      "idle": drained == 0}))
    return 1 if fatal else 0


if __name__ == "__main__":
    sys.exit(main())
