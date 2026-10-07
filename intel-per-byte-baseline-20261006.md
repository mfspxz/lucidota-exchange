# Intelligence-per-byte — first benchmark (2026-10-06)

Harness: ~/workspace/exchange/intelligence-per-byte.py
Metric: score = verified / (bytes + 1000 * latency_s), per MUSE-EXCHANGE-DESIGN.md.
Window: newest ~50 receipts (API limitation, documented in script).
All 21 jobs in window verified (VERIFY gate passed). Compute cost 0 (free tier).

rank  job                                  KB_in  KB_out  lat_s  score
1     job_probe-submit-20261006              0.0    0.3      4    2.528e-04
2     job_integ-probe-password               0.0    0.3      4    2.128e-04
3     job_integ-probe-plain                  0.0    0.3      7    1.474e-04
4     job_integ-probe-key01                  0.0    0.3      7    1.304e-04
5     job_wash1-L0_full-P1a                  1.4    0.2     11    7.718e-05
6     job_bisect-b                           2.0    0.2     11    7.369e-05
7     job_swarm1-SWARM-S2-laundering         1.3    4.4     19    4.021e-05
8     job_resub-odd-20261006                 9.7    0.2     16    3.856e-05
9     job_odd-classifier-96840508            9.7    0.4     19    3.365e-05
10    job_b2-a                               9.7    0.3     22    3.080e-05
11    job_b2-d                               7.3    0.2     26    3.009e-05
12    job_b2-c                               4.8    0.1     38    2.304e-05
13    job_bisect-a                           9.7    0.2     45    1.814e-05
14    job_b2-b                               9.7    0.2     46    1.794e-05
15    job_resub-odd-fixed-20261006           9.7    0.3     47    1.742e-05
16    job_swarm1-SWARM-S1-adversarial        1.2    5.1     52    1.709e-05
17    job_resub-r2-20261006                 20.5    6.4     80    9.331e-06
18    job_adhkey-redteam-nim (r1)           29.3   17.5     77    7.991e-06
19    job_adhkey-redteam3-nim (r3)          12.5    5.9    113    7.575e-06
20    job_resub-r1-20261006                 29.3   16.7     85    7.573e-06
21    job_adhkey-redteam2-nim (r2)          20.5   18.5    116    6.433e-06

Fleet: 21/21 verified decisions, 266.4 KB in+out, 845s latency.
Fleet score: 1.879e-05 decisions/byte-equiv.
Mean: 12.7 KB/decision, 40s/decision.

Reading: latency dominates the denominator for large reasoning-model
jobs (e.g. r2: 116s*1000 = 116,000 byte-equiv vs 39,000 actual bytes —
latency is ~75% of the cost). Bytes alone would misrank: the metric
behaves as designed. Tiny probes rank highest; the deep redteam
analyses rank lowest per byte — they buy the most insight per the
metric's definition only if their outputs get reused (receipt-based
memory is the other half of that equation).
