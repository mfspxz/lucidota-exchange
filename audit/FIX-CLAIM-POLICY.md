# exchange/audit — FIX-CLAIM POLICY (R-1)

**Date:** 2026-10-10. **Standing until Zack says stop.**

## Rule

Every fix claim in this repo's audit batteries is **instance-only** by
default. A battery report may say "ROOT CAUSE FOUND + FIXED (vX.Y.Z)" only
with this attached scope note:

- **Scope:** the exact probe set run in the named battery window, against
  the named governor version. "FIXED" means the listed probes now fail;
  it does not mean the vulnerability class is closed.
- **Promotion to class-level:** requires a dedicated re-attack wave run
  AFTER the fix ships, with probe variants (not just replays), recorded in
  a follow-up battery report. No re-attack → the claim stays
  `instance-only` forever.
- **Reversal:** if a later battery bypasses the fix, the original battery
  report gets a `SUPERSEDED-BY-BYPASS` annotation pointing at the
  defeating probe (one book — claims and defeats in the same record).

## Why

The rigor audit (R-1) proved that fix-verification proofs replay the
exact red-team payload the fix was written against — they verify the
instance, never the class. The battery-v4 INTEGRITY gate fix (v0.6.3)
was verified against the battery-v4 probe set on a mid-window redeploy
(0.6.6→0.6.9 drift, see battery-v4-regression-report.md); its class
scope is `instance-only` until a post-fix re-attack battery signs off.

## Retroactive application

| Battery report | Fix claim | Scope today |
|---|---|---|
| battery-v4.md (INTEGRITY non-stationarity) | FIXED in v0.6.3 | instance-only: battery-v4 probe set, governor v0.6.3. Re-attack battery: NOT RUN → stays instance-only. |
| battery-v4-regression-report.md | QUOTA enforcement PROVEN (6/6), R1 double-accounting fix in v0.6.8 | instance-only: the 6 listed cases + R1 probes, v0.6.6→v0.6.9. Live version drift during the window is a material caveat (window-local `worker.js` was stale at 0.6.6). |
| battery-v3.md | per-report claims | instance-only; no promotion claimed. |

No battery report in this directory claims class-level closure. That is
the policy, not an accident.
