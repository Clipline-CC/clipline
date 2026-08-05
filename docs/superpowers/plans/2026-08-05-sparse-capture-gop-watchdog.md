# Sparse Capture GOP Watchdog Plan

**Goal:** Keep the missing-keyframe safety guard without treating valid sparse WGC frames as an encoder failure after a static-screen interval.

## Pipeline regression

- [ ] Add a failing recorder test whose first encoded sample has the configured frame duration and whose next sample arrives after a wall-clock gap longer than the GOP watchdog limit.
- [ ] Prove the sparse stream finishes successfully while the existing continuous no-keyframe stream still exceeds the safety limit.

## Minimal fix

- [ ] Measure pending GOP progress from encoded frame count and the stream's shortest valid sample duration instead of capture PTS span.
- [ ] Reset pending frame progress at the initial keyframe and every sealed GOP while retaining the combined video/audio byte guard.
- [ ] Keep the error explicit about a missing keyframe and encoded-frame duration.

## Verification and handoff

- [ ] Run focused capture tests, workspace tests, and workspace Clippy with warnings denied.
- [ ] Update `handoff.md`, rebuild Clipline, and open the app for a static-screen manual check.
