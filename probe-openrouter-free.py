#!/usr/bin/env python3
"""Probe candidate free OpenRouter models with a 1-token chat call.

Used when the default :free model is 429'd upstream — finds a working
alternative before switching the Exchange default via /v1/config.
Key via keyman (OPENROUTER_KEY); never printed. $0 (all :free models).
Usage: python3 probe-openrouter-free.py [model ...]
Default candidates: 4 small free models.
"""
import json
import subprocess
import sys
import urllib.request

DEFAULT = [
    "liquid/lfm-3-1.7b:free",
    "inclusionai/ling-3.2-flash:free",
    "baidu/ernie-5.5-flash-preview:free",
    "recursive/flint-v1.0:free",
]


def main():
    models = sys.argv[1:] or DEFAULT
    key = subprocess.run(
        ["python3", "/home/hatch/workspace/bin/keyman.py", "get", "OPENROUTER_KEY"],
        capture_output=True, text=True).stdout.strip()
    if not key:
        print("no key from keyman"); return 1
    for m in models:
        body = json.dumps({"model": m, "messages": [{"role": "user", "content": "Reply with exactly: ok"}],
                           "max_tokens": 8}).encode()
        req = urllib.request.Request(
            "https://openrouter.ai/api/v1/chat/completions", data=body, method="POST",
            headers={"Content-Type": "application/json",
                     "Authorization": f"Bearer {key}",
                     "User-Agent": "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36"})
        try:
            with urllib.request.urlopen(req, timeout=60) as r:
                j = json.loads(r.read())
                ch = j.get("choices") or [{}]
                content = ((ch[0].get("message") or {}).get("content") or "")[:60]
                print(f"OK   {m} -> {content!r} (model={j.get('model')})")
        except urllib.error.HTTPError as e:
            raw = e.read().decode("utf-8", "replace")[:200]
            print(f"HTTP {e.code} {m} -> {raw}")
        except Exception as e:
            print(f"ERR  {m} -> {type(e).__name__}: {str(e)[:150]}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
