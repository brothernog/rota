# Optimization Trials

Date: 2026-11-27. Base revision: `83137bf3668a2455fca9637e758bb5019a7f796e`.

Measurements use the diagnostics runner, the measured preset, the same scripted 330-second
route, a warm pipeline cache, captures and one-second overlay samples. Stationary values below
are medians of samples between 220 and 280 seconds. The p95 values are percentiles of those
one-second interval averages, not individual frame times. The A/B comparison runs ended at the
330-second test timeout. A later profile smoke run was closed manually after about 264 seconds;
the user confirmed there was no crash. Captures provide a scene check, not exhaustive render
correctness. More complete gates remain necessary.

## Accepted

| Step | Run 1 | Run 2 | Decision |
|---|---:|---:|---|
| Existing preset plus `VICTORY_CARD_REHASH_RUNS=1` | 11.81 FPS, 84.7 ms | 11.97 FPS, 83.6 ms | Accept; preparing time fell from about 63.6 to 44 ms in the comparison. |
| Above plus `VICTORY_RELAY_COPY=1` | 12.32 FPS, 81.3 ms | 12.45 FPS, 80.3 ms | Accept; back-copy time fell from about 55 to 18 ms. |
| Above plus `VICTORY_CARD_STEPS=2` | 14.13 FPS, 70.9 ms, p95 76.2 | 13.78 FPS, 72.6 ms, p95 86.8 | Accept; repeated FPS improvement. This trades shadow fidelity for speed. |

The three settings are now defaults in `apps/victory/src/tuned.rs`. Environment overrides still
win, and `VICTORY_TUNED=0` disables the defaults. Two additional 330-second runs using the
compiled defaults measured 14.07 and 14.18 FPS (p95 73.7 and 82.5 ms), confirming the built-in
profile matches the separately measured environment settings.

## Rejected

| Candidate | Run 1 | Run 2 | Decision |
|---|---:|---:|---|
| Sparse 1 KiB relay merge from the prior checkout | 10.51 FPS, p95 216.4 ms | 10.66 FPS, p95 1426 ms | Reject; no robust gain and worse long intervals. |
| Texture pool spare blocks 64 to 16 | 13.59 FPS, p95 86.2 ms | 13.33 FPS, p95 86.8 ms | Reject; shared memory did not fall, and FPS regressed against 14.07/14.18 control. |
| `VICTORY_JOIN_COPIES=1` | 14.03 FPS, p95 75.8 ms | 14.32 FPS, p95 86.9 ms | Reject; no consistent gain beyond run spread. |
| `VICTORY_INDIRECT_BATCH=2` | 14.48 FPS, p95 71 ms | 14.10 FPS, p95 84.1 ms | Reject; no consistent FPS or tail-latency win; indirect drops persisted. |

## Limits and next work

Several hundred commands per frame are still dropped because the builder pipeline is disabled;
indirect execution also has independent drops. These runs do not pass the no-drops correctness
gate. `shared_used_gb` remained high, around 8 to 9 GB. The texture-pool cap did not solve it.
The accepted A/B tests lasted 330 seconds each. A 30-minute stability run was not requested and
was stopped after the application was closed manually at about 264 seconds. No claim is made
that the full 15-item performance plan is complete.

Next, profile one concrete repeated preparation path from the frame trace, make a narrow change,
and run the same two-run comparison. Larger memory-directory, descriptor and Wave64 imports stay
out until their individual correctness tests and matched game measurements show a benefit.
