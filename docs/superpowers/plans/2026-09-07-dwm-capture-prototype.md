# DWM shared-surface capture prototype

Prototype only: test whether an explicitly selected window exposes useful frames through
`user32!DwmGetDxSharedSurface`. Do not add a production backend, change recording defaults,
invoke WGC, inject into games, or copy a third-party capture implementation.

Baseline: Nightly 1.0.4 (`c4554238`). The research branch was fast-forwarded from the old
0.1.34 checkout before implementation. Local hardware is Windows 11 build 26200 with
AMD RX 6700 XT and a USB virtual display; Windows 10 and game acceptance remain separate.

- [ ] Add failing checks for probe argument limits and BMP row layout; examples must be
  included in workspace tests. Then implement a minimal cross-platform command wrapper.
- [ ] Implement the isolated Windows example module. Resolve the undocumented export at
  runtime, validate a selected visible window and its capture affinity, select the returned
  adapter, open the surface, and read supported 32-bit SDR pixels with checked row pitches.
  All unsafe code stays under an examples/windows directory. No fallback capture API.
- [ ] Sample for a bounded duration, report successful reads, changed pixel hashes, failures,
  and readback latency separately (polls are not unique game FPS). Save a few BMP snapshots
  plus a CSV log in a new output directory. Minimized/closed/unsupported windows fail visibly.
  Mark CPU readback and undocumented synchronization as prototype limits.
- [ ] Run a controlled animated test window, inspect snapshots, cover it with another window,
  resize/minimize/restore, and record observed behavior. Do not claim physical Windows 10
  validation from a Windows 11 host or claim screenshots establish encoder readiness.
- [ ] Run workspace tests and fresh-cache warning-denied workspace Clippy, review the diff,
  update handoff and research findings, and provide the exact Windows 10 tester command.
  Open Clipline per the repository workflow; make clear that the prototype is a separate exe.

Acceptance on Windows 10 22H2: League/Valorant with motion, window overlap, fullscreen modes,
resize/focus changes, stable memory, correct colors, and useful sampling throughput. Keep
capture exclusions intact. A successful simple test window is only an initial feasibility result.

Sources and uncertainty are documented in
`docs/research/2026-09-07-windows-10-support.md`. If the private API is unavailable, black,
stale, or otherwise incompatible, record that result rather than substitute WGC/DXGI.
