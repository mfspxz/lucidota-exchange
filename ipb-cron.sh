#!/bin/bash
# exch-010: IPB continuous cron runner (STAGED — do not enable before 2026-10-08 00:00 UTC).
# Runs RUST-1's intelligence-per-byte binary hourly, appends JSON snapshots,
# refreshes the leaderboard. No daemon: cron is the backbone.
# ENABLE CONDITIONS: (1) D1 freeze lifted (>= 2026-10-08 00:00 UTC), (2) token
# triage heavy-cron ban lifted (post Oct 11 reset). The binary reads the live
# governor API (~/.exchange_key); running it during the freeze violates it.
set -u
BIN="$HOME/workspace/exchange-rs/target/debug/intelligence-per-byte"
OUTDIR="$HOME/workspace/exchange/ipb-snapshots"
LEDGER="$HOME/ledger/ledger.py"
mkdir -p "$OUTDIR"
STAMP=$(date -u +%Y%m%d-%H%M%S)
# Freeze guard: refuse to run before 2026-10-08 00:00 UTC
if [ "$(date -u +%s)" -lt "$(date -u -d '2026-10-08 00:00 UTC' +%s)" ]; then
  echo "FROZEN: D1 freeze active, IPB cron skipped ($STAMP)" >&2
  exit 0
fi
JSON_OUT="$OUTDIR/ipb-$STAMP.json"
if timeout 600 "$BIN" --json > "$JSON_OUT" 2>"$OUTDIR/ipb-$STAMP.err"; then
  LINES=$(wc -l < "$JSON_OUT")
  python3 "$LEDGER" "{\"actor\":\"cron:ipb\",\"chat\":\"direct\",\"kind\":\"result\",\"summary\":\"IPB hourly snapshot ok\",\"detail\":\"$JSON_OUT ($LINES lines)\"}" >/dev/null 2>&1 || true
else
  echo "IPB run failed ($STAMP), see $OUTDIR/ipb-$STAMP.err" >&2
  exit 1
fi
