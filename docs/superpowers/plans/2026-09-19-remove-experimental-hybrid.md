# Remove experimental hybrid capture

Withdraw the opt-in PrintWindow/fullscreen-display backend and the shared
aspect-preserving conversion it leaked into Auto/WGC and every encoder.

- [ ] Fail settings, UI, policy, and conversion tests that still accept hybrid
      or letterbox/pillarbox output.
- [ ] Remove the experimental backend, PrintWindow worker, and hybrid-only
      cadence/status wiring. Persisted `experimental_hybrid` loads as Auto.
- [ ] Restore stretch-to-fill CPU and GPU conversion. Keep named display
      selection and explicit Desktop Duplication.
- [ ] Delete PrintWindow examples/CI/scripts and unused hybrid modules.
- [ ] Run workspace tests and warning-denied Clippy. Update handoff.

No injection, encoder, or default-backend change beyond dropping hybrid.
Checkboxes stay unticked (repo convention).
