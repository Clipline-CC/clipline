# Performance first pass: MP4 writes and process queries

Base: `develop` at `1ea5f854`. The supplied performance audit reviewed `e8076200`.
Scope selected by the user: a new PR into `develop` with MP4 batching and
process-query reuse within each visible-window enumeration.

## MP4 batching

- [ ] Add a counted-sink regression before implementation: many small borrowed
  audio samples must use bounded batches while emitting the same fragment bytes.
- [ ] Compare bounded standard-library buffers on real files before selecting
  capacity; retain the synthetic write-call results as measurements, not FPS claims.
- [ ] Batch in the shared writer so replay, full-session and trim callers benefit.
  Preserve public APIs, exact seek offsets, explicit timestamps and bounded memory.
- [ ] Flush each completed fragment explicitly before reporting success. Exercise
  fragment recovery visibility and underlying write/flush failures; keep finalization
  and abort/`into_inner` semantics intact.

## Process-query reuse

- [ ] Exercise repeated PIDs, failed queries and fresh results in separate scans.
- [ ] Give one enumeration a metadata cache and a reusable UTF-16 path buffer.
  Keep titles, visibility and window ordering fresh. Drop all metadata after the scan;
  no cross-tick PID cache or retained process handles.
- [ ] Exercise the real Win32 enumeration and existing game-selection tests.

## Gates and delivery

- [ ] Obtain an independent read-only second opinion on the implementation.
- [ ] Run workspace tests, fresh-cache warning-denied Clippy for changed crates,
  and `git diff --check`.
- [ ] Update `handoff.md` with changes, measurements and their limits. Reopen the
  development app after rebuilding and provide focused manual acceptance checks.
- [ ] Push the branch and open a PR into `develop`; verify Ubuntu and Windows CI.

No WGC pacing, GPU pooling, audio quality changes, save/persistence workers,
telemetry framework, dependencies or settings changes belong in this pass.
Plan checkboxes intentionally remain unticked by repository convention.
