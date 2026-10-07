# REDTEAM-2 Quorum-Bypass Group — Report (2026-10-07)

**Target:** live Exchange governor v0.6.6 (`https://lucidota-exchange.cutecity.workers.dev`)
**Status:** LIVE-FIRE HALTED 21:16 PDT by Zackary's D1 emergency freeze (Cloudflare 87% of 5M daily rows_read; zero traffic until 2026-10-08 00:00 UTC). Remaining battery executed against a local SQLite replica of the audited tally code.
**Harness:** [redteam2-quorum.py](redteam2-quorum.py) (live) · [redteam2-tally-replica.py](redteam2-tally-replica.py) + [redteam2-battery-local.py](redteam2-battery-local.py) (offline, 23/23 checks green)

## Per-test verdicts

| Test | Question | Verdict | Evidence |
|---|---|---|---|
| (a) | 3-voter ballot, 1 valid + 2 INVALID — does it leave 'open'? | **PROVEN** (live) | `bal_muxkonwh1vlphw` (garbage w/o ternary): → `expired`, valid=1 < quorum=2. `bal_muxksrm7vmtzgv` (garbage + ternary=1, INVALID visible in tally): → `expired`, valid=1 < quorum=2. Never stuck, never decided. |
| (b) | 2-voter ballot, 2 disagreeing valid votes — INCONCLUSIVE or stuck? | **UNKNOWN** (live); **PROVEN** (replica) | Live: claim race lost twice to honest workers (see Contention). Replica: tie 1–1 → `open` pre-deadline (keep-waiting branch) → `inconclusive` + `UNKNOWN` at deadline. |
| (c) | 2-voter ballot, 1 valid + 1 FAILED_PERMANENT — quorum math? | **UNKNOWN** (live); **PROVEN** (replica) | Replica: failed completion → `decision.output=null`, no `ballot_id` stamp → receipt invisible to tally → valid=1 < quorum=2 → `expired`. Failed executions cannot shrink the voter set into a decision. |
| (d) | 'decided' with fewer valid votes than floor(n/2)+1, by any means | **DISPROVEN** (all vectors) | See attack vectors below. |

## Attack vectors for (d) — all closed

1. **Failed-completion vote smuggling** (complete FAILED_PERMANENT but attach vote-bearing output): DISPROVEN. `completeClaimed` writes `output: !failed ? b.output : null` (worker.js) — the output is nullified before the receipt is written, `extractVote` → INVALID, and no `ballot_id` is stamped. Live code + replica T4.
2. **Double completion / receipt stuffing**: DISPROVEN. Claim-level idempotency (W2): second complete of a completed claim replays the original, no new receipt. Replica T5.
3. **Late vote after terminal state**: DISPROVEN. `if (state === "open")` guard — receipts arriving after `expired`/`decided`/`inconclusive` are tallied for display but cannot transition state. Replica T6 (valid rose 1→2 post-expiry, state frozen at `expired`).
4. **Deadline-boundary re-poll**: DISPROVEN. Tally is a deterministic function of (receipts, now); polls at deadline−1ms/+0/+1/+2/+1000ms are stable. Replica T9.
5. **Same-ms duplicate receipts** (MAX(created_at) dedup tie): MECHANISM PROVEN in SQL, live exploitability ~nil. If two receipts for one job share an identical ms timestamp, the dedup JOIN matches both → 2 votes from 1 job (replica T7: 2 voters → 3 valid votes → `decided`). Live, this requires a ms-identical double write, but completions are idempotent, timestamps are ms-precision ISO, and deferred receipts are excluded by the `ballot_id` filter. Defense-in-depth fix: tiebreak dedup on `MAX(rowid)`.
6. **Sweep n ∈ {2,3,5,7}**: no configuration reaches `decided` with valid < quorum. Replica T11. `decided` strictly requires `top > valid/2 AND valid >= floor(n/2)+1`.

## Side findings

- **SF1 — CRITICAL (PROVEN live): server-side ballots are blind to all honest worker votes.** `receipts.ballot_id` is stamped only inside the `ternary_verdict ∈ {1,0,-1}` branch of `completeClaimed` (worker.js:787-789). The live `nim-claimant.py` completes with `output={content, finish_reason, usage}` — no `ternary_verdict` — so its receipts get `ballot_id=NULL` and are invisible to the tally's `WHERE ballot_id=?` query. Observed live: sibling's real NIM vote (`rcpt_muxla0jw8dmg9x`, tail `DENIED`) absent from its ballot's tally. **`/v1/ballots` cannot decide any ballot executed by honest workers.** Fix: stamp `ballot_id` whenever `spec.ballot_id` is present, independent of ternary extraction.
- **SF2 — LOW (PROVEN live + replica): ballot's terminal receipt pollutes its own tally.** The state-transition receipt is inserted with `job_id='ballot:'+id` (worker.js:2203-2204) and matches the tally query, appearing in `tally.votes` as an INVALID vote (`worker_outcome=null`). Cosmetic — INVALID is filtered from quorum math — but it corrupts vote-list consumers.
- **SF3 — INFO: FIFO claim-queue contention.** Multiple redteam2 agents plus honest workers race the `nim-nvidia` claim queue; oldest-lease-first means ballots lose jobs to faster pollers. Observed cooperative etiquette from a sibling: foreign jobs released via `FAILED_RETRYABLE` ("redteam2: test worker releasing foreign job for replan") rather than completed with bad data. Cost me test (b) twice; recommend claim-coordination (ledger-advertised ballot IDs) for future multi-agent redteams.

## Test count & IDs

- Live ballots created: 5 — `bal_muxkicuge8k46y` (probe), `bal_muxkonwh1vlphw` (a1), `bal_muxksrm7vmtzgv` (a2), `bal_muxkzuyto9qkm8` (b-att1), `bal_muxl9k4q8xewnx` (b-att2)
- Live completions by this agent: 7 (a1: 1 valid + 1 garbage; a2: 1 valid + 2 garbage; b-att1: 1 DENIED; b-att2: 1 DENIED)
- Key receipts: `rcpt_muxkprc27nk9hw` (a1 valid), `rcpt_muxktotc3ikblj` (a2 valid), `rcpt_muxktzif2aatar` (a2 INVALID), `rcpt_muxkw5t5kku3c2` (a2 terminal/phantom), `rcpt_muxla0jw8dmg9x` (sibling real vote, ballot_id=NULL — SF1 proof)
- Replica battery: 23/23 checks green (T1–T11)
- Rate discipline: held ≤34 req/min, zero 429s encountered

## Caveats

- (b) and (c) were not executed cleanly live; their verdicts rest on the replica, which is a line-by-line port of the audited `tallyBallot`/`extractVote`/receipt-stamping code. Re-run live after the freeze lifts to promote to PROVEN.
- The replica models the governor's claim-idempotency guard rather than re-proving it (W2 was proven by the earlier battery).
- No live traffic was generated after 21:16 PDT; the in-flight test-b retry was killed mid-run (its 6th-attempt ballots were never created — no orphaned state beyond the 5 listed ballots).
