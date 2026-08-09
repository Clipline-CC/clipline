# Manual Full-Session Origin Fix Plan

**Goal:** Make a full-session recording started while the replay buffer is already running finalize
normally, including when an audio packet straddles the first recorded GOP boundary.

## Root cause

The full-session writer uses its first video segment timestamp as the new file origin. When the
writer is attached mid-stream, that sealed GOP can contain an independently decodable audio packet
whose timestamp begins just before the GOP keyframe. The replay exporter already clips that
straddling packet at its selected origin; the full-session path currently keeps it and rejects its
negative relative timestamp during finalization.

## Plan-driven implementation

### Task 1: Reproduce the failure

- [ ] Add a capture-pipeline regression test that starts a full session midway through an active
      GOP with straddling audio and expects a finalized MP4.
- [ ] Confirm the test fails with `media sample timestamp precedes recording origin`.

### Task 2: Fix the shared writer boundary

- [ ] Apply the existing origin-aware audio selection to full-session segments.
- [ ] Keep later audio packets and all video intact; only discard audio samples before the new
      file's origin.

### Task 3: Verify and hand off

- [ ] Run the focused regression, full workspace tests, and warning-denied Clippy.
- [ ] Update `ddoc.md` and `handoff.md`, commit, rebuild, and relaunch Clipline.
