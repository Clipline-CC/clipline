# Hybrid display topology recovery (PR #200 review)

Cursor identified fatal display re-lookup errors during ongoing hybrid capture.
Keep target identity/protection errors fatal, but treat unavailable or incomplete
monitor topology as waiting/black so audio and automatic recording intent survive.

- [ ] Add failing regressions for failed/incomplete enumeration, a missing target,
  post-acquisition topology loss, and recovery to window/display capture.
- [ ] Use one complete handle/info enumeration per hybrid observation, rejecting
  partial results without changing existing best-effort display callers.
- [ ] Preserve target-only selection, before/after guards and entry debounce.
- [ ] Run focused regressions, workspace tests and fresh-cache warning-denied
  Clippy; review the diff independently and record the limits of injected tests.
- [ ] Build and open Clipline, validate the available physical fixture, update
  handoff/report/PR, push the existing branch and verify Windows/Ubuntu CI.

No claim of physical hotplug validation on the single-display machine. No WGC
fallback, new monitor support, release or merge.
