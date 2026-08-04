# Review Video Edit-List Flicker

> **For agentic workers:** Execute this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for
> tracking and remain unticked by repository convention.

**Goal:** Stop WebView2 from flashing a stale first frame at unobservable video timeline gaps while
preserving real capture and audio gaps.

## Measured cause

The reported 29.007-second trim contains 1,740 uniquely decoded H.264 frames. The first decoded
frame occurs only once, frame timestamps are strictly increasing, FFmpeg reports no decode error,
and the same file plays normally in a desktop player. Selecting only Output Audio in Clipline does
not change the symptom, excluding the hidden audio-sidecar transport.

The finalized video track instead contains seven edit-list entries: four media runs separated by
three empty edits exactly one 90 kHz video tick (11 microseconds) long. Their presentation boundaries
are approximately 5.003, 14.506, and 27.006 seconds. These edits originate when capture-clock
rounding places a fragment one tick after the writer frontier. `HybridMp4Writer` currently preserves
that unpresentable gap as an empty edit; WebView2 exposes the transition by briefly presenting its
stale initial surface even though native players hide it.

## Task 1: Failing writer regression

- [ ] Add one focused `clipline-mp4` writer test proving that an internal video gap shorter than the
      preceding frame is folded into that frame and produces no edit list.
- [ ] In the same test, prove that a frame-sized video gap remains an explicit edit.
- [ ] Run the focused test and confirm it fails before implementation.

## Task 2: Normalize only unpresentable video gaps

- [ ] At the shared `HybridMp4Writer::set_track_decode_time` boundary, absorb a positive internal
      video gap smaller than the preceding sample duration by extending that preceding sample.
- [ ] Update the final sample-duration run, media duration, and presentation run together so the
      finalized `stts`, track duration, and edit list remain internally consistent.
- [ ] Preserve leading video gaps, video gaps at least one frame long, every audio gap, and the
      existing rejection of backward decode time.

## Task 3: Verify and document

- [ ] Run the focused MP4 writer tests, `cargo test --workspace`, and warning-denied workspace
      Clippy.
- [ ] Confirm a remuxed equivalent of the supplied clip has no internal one-tick video edits while
      retaining its two audio tracks and duration.
- [ ] Update `handoff.md` with the supplied-clip evidence, fix, and manual WebView retest points.
- [ ] Rebuild and open Clipline for a local smoke test.

## Manual retest

1. Play a new 30-second clip containing both Output Audio and Microphone in Clipline from beginning
   to end; no stale first-frame flash should appear.
2. Repeat with only Output Audio selected to cover direct embedded-audio playback.
3. Seek repeatedly and replay the clip; video and audio must stay aligned.
4. Confirm an intentional capture interruption of at least one video frame is still represented as
   a timeline gap rather than silently shortened.
