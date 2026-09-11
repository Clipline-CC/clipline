# Shared CPU conversion performance regression

The user reports lower in-game FPS on Windows 11 Auto/WGC. Exact release audits
confirm the hybrid worker/cadence are opt-in, but 1.0.5 changed shared conversion.
Optimized CPU A/B tests show a 39-40% slowdown vs 1.0.4; GPU conversion A/B on this
Win10 AMD host shows no consistent slowdown. The user's encoder/GPU/game remain
unknown, so this CPU regression is not a confirmed cause of that report.

- [ ] Preserve the measured pre-fix latency regression and exact-source benchmarks.
- [ ] Initialize black NV12 planes and iterate only the even fitted content bounds,
  removing per-sample clipping branches without changing pixel output or geometry.
- [ ] Verify byte equality against released 1.0.5 over varied size/crop/stride cases,
  rerun optimized alternating benchmarks, and retain the existing correctness tests.
  Do not introduce timing thresholds in normal CI tests.
- [ ] Run workspace tests, fresh capture-cache warning-denied Clippy, review and
  formatting checks. Document audit findings and measurement limits in handoff/report.
- [ ] Push a separate PR against develop with Windows/Ubuntu CI. Keep PR #201's
  experimental canvas changes independent; no merge or release is authorized here.
- [ ] Leave the app open and report the still-unconfirmed Windows 11 FPS cause.

## Review revision: chroma invariants and measured loop structure

The user supplied byte-equivalence and mutation-test review of head 648f706.
The conversion is correct, but the committed white-only letterbox test cannot
detect chroma being written to the wrong output row. The current geometry test
also misses removal of the height alignment mask. Their League report now names
a Win11 RX 6700 XT and recovery after app restart; actual encoding remains unknown.

- [ ] Add red letterbox/pillarbox chroma placement tests and a width-limited
  odd fitted-height tuple. Show the targeted tests fail for the reported chroma
  row-offset and removed-height-mask mutants before retaining the changes.
- [ ] Reject misaligned fitted bounds in the CPU constructor and document/check
  the private pixel sampler's precondition. Derive black prefill from color math.
- [ ] Compare the reviewed implementation, per-row slices and a fused 2x2 loop
  using optimized, alternating benchmarks. Include letterbox/pillarbox cases,
  verify byte equivalence, and retain the best justified safe implementation.
  Do not add a full-coverage prefill branch or assume a win from source shape.
- [ ] Retain a reproducible in-tree benchmark/verification entry point so evidence
  does not depend solely on a developer desktop. No CI timing threshold.
- [ ] Correct the audit/PR's affected scope to MfSoftware, distinguish observed
  timings from hardware-sensitive explanations, and record the fusion decision.
- [ ] Run required local quality gates, independent review, push to PR #202,
  verify Windows/Ubuntu CI, update handoff and leave the app open. Do not merge
  or publish; the Win11 League FPS cause remains unconfirmed.
