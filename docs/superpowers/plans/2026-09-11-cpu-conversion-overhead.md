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
