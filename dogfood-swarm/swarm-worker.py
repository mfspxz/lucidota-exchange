#!/usr/bin/env python3
"""Swarm validator worker — executes a claim's probe against NIM and applies
a DETERMINISTIC check to the output.

This process NEVER asks a model for a verdict. The model executes (produces
behavior for a prompt); a pure function maps (output, expectation) to a
ternary verdict. Replace the model with a shell script returning canned text
and the verdict logic is unchanged — that is the test that this is not
determinism theater.

Protocol: task JSON on stdin → result JSON on stdout. Exit 0 = validator ran
cleanly (verdict may be +1, 0, or -1). Exit 2 = malformed task (permanent).
Any other failure (timeout, crash) is the claimant's FAILED_RETRYABLE.

$0: uses the vault-held NIM credential via the nvidia-nim skill surrogate,
exactly like nim-claimant.py. The secret never travels.
"""
import hashlib
import json
import re
import sys
import time
import urllib.error


def load_nim():
    from importlib.machinery import SourceFileLoader
    import os
    p = os.path.expanduser("~/workspace/skills/nvidia-nim/bin/nvidia-nim")
    return SourceFileLoader("nimcli", p).load_module()


def normalize(s):
    # Byte-stable: strip + collapse ASCII whitespace runs. No Unicode
    # normalization games — raw UTF-8 in, raw UTF-8 out.
    return re.sub(r"[ \t\r\n]+", " ", s.strip())


def deterministic_check(output, expect):
    """Pure function: (output text, expectation) -> (verdict, detail).
    verdict: +1 proven | 0 unknown | -1 disproven. Never calls a model."""
    match = expect.get("match", "exact")
    value = expect.get("value", "")
    if match == "exact":
        ok = normalize(output) == normalize(value)
        return (1 if ok else -1), {"match": "exact",
                                   "output_norm": normalize(output)[:200],
                                   "expect_norm": normalize(value)[:200]}
    if match == "contains":
        ok = value in output
        return (1 if ok else -1), {"match": "contains",
                                   "found": ok, "needle": value[:200]}
    if match == "hash-sha256":
        h = hashlib.sha256(output.encode("utf-8")).hexdigest()
        ok = h == value.lower()
        return (1 if ok else -1), {"match": "hash-sha256",
                                   "output_hash": h, "expect_hash": value.lower()}
    if match == "nonempty":
        # Gauntlet-grade: the execution produced *a* response. Verifies the
        # validator can drive NIM, not any specific model behavior.
        ok = bool(output and output.strip())
        return (1 if ok else -1), {"match": "nonempty",
                                   "output_len": len(output or "")}
    return 0, {"error": f"unknown match type: {match}"}


def execute_verify(task):
    ex = task.get("execution") or {}
    expect = task.get("expect") or {}
    model = ex.get("model") or "openai/gpt-oss-20b"
    prompt = ex.get("prompt")
    if not prompt:
        return {"task_error": "permanent", "detail": "execution.prompt required"}
    if "match" not in expect or "value" not in expect:
        return {"task_error": "permanent", "detail": "expect.match and expect.value required"}

    nim = load_nim()
    t0 = time.time()
    try:
        raw = nim.api("POST", "/chat/completions", {
            "model": model,
            "messages": [{"role": "user", "content": prompt}],
            "max_tokens": ex.get("max_tokens", 64),
            "temperature": 0.0,  # determinism: the model executes, we judge
            "stream": False,
        })
        status = 200
    except urllib.error.HTTPError as e:
        return {"task_error": "retryable",
                "detail": f"NIM HTTP {e.code}"}
    except Exception as e:
        return {"task_error": "retryable",
                "detail": f"NIM call failed: {str(e)[:200]}"}
    lat = int((time.time() - t0) * 1000)

    # Mechanical extraction — same as nim-claimant. No interpretation.
    content = None
    if status == 200 and isinstance(raw, dict):
        ch = (raw.get("choices") or [{}])[0]
        msg = ch.get("message") or {}
        content = msg.get("content") or msg.get("reasoning_content") or msg.get("reasoning")
    if not content:
        # The execution produced no evidence — unknown, not disproven.
        return {"ternary_verdict": 0,
                "detail": {"reason": "empty output — no evidence either way",
                           "latency_ms": lat, "model": model}}
    verdict, check = deterministic_check(content, expect)
    return {"ternary_verdict": verdict,
            "detail": {**check, "latency_ms": lat, "model": model,
                       "output_hash": hashlib.sha256(
                           content.encode("utf-8")).hexdigest()}}


def main():
    try:
        task = json.load(sys.stdin)
    except Exception as e:
        print(json.dumps({"task_error": "permanent",
                          "detail": f"stdin is not JSON: {e}"}))
        return 2
    if task.get("task_type") != "execute-verify":
        # Delegated gauntlet: the Exchange's verifier submits a probe job
        # with empty input. Answer it honestly with a minimal real NIM
        # round-trip — this IS the evidence the ladder requires.
        if not task.get("task_type") and not task:
            print(json.dumps(execute_verify({
                "task_type": "execute-verify",
                "execution": {"prompt": "Reply with the word ok.",
                              "max_tokens": 16},
                "expect": {"match": "nonempty", "value": ""},
            })))
            return 0
        print(json.dumps({"task_error": "permanent",
                          "detail": f"unknown task_type: {task.get('task_type')}"}))
        return 2
    res = execute_verify(task)
    if res.get("task_error") == "permanent":
        print(json.dumps(res))
        return 2
    print(json.dumps(res))
    return 0


if __name__ == "__main__":
    sys.exit(main())
