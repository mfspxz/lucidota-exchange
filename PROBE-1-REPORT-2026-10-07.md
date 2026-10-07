# PROBE-1 Report — 2026-10-07 (ledger seq 595)

4 provider probes, **~140 live tests**, all vault-key-only, no signups, no spend.

## NIM re-probe (80/80 catalog models, temp-0, 60s hang cutoff)
**10 working chat models** (all answered 391 with identity echo):
| Model | Latency | Status |
|---|---|---|
| meta/llama-3.2-11b-vision-instruct | 1.06s | NEW |
| nvidia/nemotron-3-super-120b-a12b | 1.11s | NEW |
| poolside/laguna-xs-2.1 | 1.12s | NEW |
| meta/llama-3.2-90b-vision-instruct | 1.89s | NEW |
| z-ai/glm-5.3 | 1.9s | reconfirmed |
| openai/gpt-oss-20b | 3.26s | reconfirmed |
| meta/muse-glimmer-30b | 5.23s | NEW (prompt-echo-y but correct) |
| nvidia/nemotron-3-ultra-550b-a55b | 8.71s | NEW (heavy) |
| nvidia/nemotron-3.5-lightning-30b-a3b | 9.1s | reconfirmed |
| nvidia/ising-calibration-1.5-31b | 53.81s | NEW (too slow for ballots) |

67 failed: 55×404, 7×hang (deepseek-v4.1-flash, gemma-4-31b-it, llama-guard-4-12b, kimi-k3, 2×nemoguard-safety, **glm-5.3-flash**), 2×500, 1×503, 1×400, 1×200-empty. 3 non-chat endpoints work but aren't chat-usable.
⚠️ **glm-5.3-flash — a known-working model — hung 60.4s.** Working-set membership is non-stationary; evidence-gate per probe.

## OpenRouter free lanes (20/20 free models probed)
**8 new fully-working**: nemotron-3.5-lightning:free (2.2s), nemotron-3-super-120b:free (2.4s, prior 90s timeout fixed), nemotron-3-nano-omni-30b-reasoning:free (3.0s), liquid/lfm-2.5-2.6b:free (1.9s), **cohere/north-mini-code:free (1.5s, fastest)**, poolside/laguna-s-2.1:free (1.5s), inclusionai/ling-3.0-flash-sante:free (1.5s), openrouter/free alias (3.4s).
**3 reasoning-only** (output in `reasoning` field): apodex-1.1-mini, dots-3-note-preview, nemotron-3.5-content-safety (467 native tokens for a tiny prompt — expensive). **1 flaky** (laguna-xs-2.1 1/3). Blocked: inkling ×2 (403 entitlement), ling-3.1-flash + lyria ×2 (403 key limit), gemma-4-26b/31b (persistent upstream 429).

## ai-router (~35 probes)
**1 working route**: kilo keyless lane — auto-router, rotating served pool (nemotron-3-ultra-550b:free, dots-3-note-preview:free, step-3.7-flash, ling-3.1-flash, laguna-s-2.1:free; 2.3–9.1s, intermittent 429s). pollinations dead (key 402), ovh/llm7 down, all keyed lanes `configured:false` (deepseek secret missing). 12 builtin lanes, 0 KV dynamic lanes.

## New $0 compute (5 live probes)
All UNKNOWN: Lightning AI (15 free credits/mo per official sources, but signup — blocked on Zackary — must confirm no-card, L4 access, exhaustion behavior); Baseten ($30 one-time, keyless 403); Fireworks ($1 one-time, keyless 401); GMI (enterprise, not worth chasing); Pollinations text (HTTP 500 ENOSPC outage). Eigen AI unresolved, untouched.

## Action taken: ballot voter set widened
`ballot_policy` updated live (D1): **5 vendor-diverse voters** — openai/gpt-oss-20b, z-ai/glm-5.3 (replaces hung flash), meta/llama-3.2-11b-vision-instruct, poolside/laguna-xs-2.1, nvidia/nemotron-3.5-lightning-30b-a3b. Quorum auto=3. OpenRouter lanes are separate free capacity (not claimant-executable via NIM claimant).

Raw: `exchange/probe-results-nim-reprobe-20261007-035012.jsonl` · Ledger: seq 462 (new-compute), 484/485 (openrouter), 502 (ai-router), 586 (nim), 595 (this report).
