# Group Upload Audio and Cloud State

**Goal:** Preserve microphone audio in group compilations and make the group upload icon follow the
same Upload → Copy cloud link/Open cloud page state machine as normal clips.

## Confirmed causes

- Compilation input discovery collapses `audio=2` to `has_audio=true`; FFmpeg then maps only
  `[input:a:0]`, dropping microphone stream 1 before export or upload.
- `syncUploadClipButton` returns from an active-group special case before resolving the generated
  compilation and its persisted `cloud.uploads` record, so uploaded/shareable state is ignored.

## TDD steps

- [ ] Add a failing Groups unit test proving a two-audio-stream member emits both stream labels and
      `amix=inputs=2`, while zero-audio members still receive generated silence.
- [ ] Retain each member's audio track count, normalize every stream to 48 kHz stereo, `amix` with
      longest duration/zero dropout/normalization, then feed the existing cross-clip concat.
- [ ] Add a failing UI contract for resolving the latest versioned `source_group` compilation, consulting
      `clipCloudRecord`, rendering uploaded/shareable state, and routing clicks through copy/open.
- [ ] Reuse an existing current compilation for Copy/Upload; invalidate it in-session when group
      order or membership changes.
- [ ] Run a real FFmpeg two-track smoke confirming microphone-bearing mix output, Node checks,
      workspace tests, warning-denied Clippy, update docs/handoff, commit, and relaunch.
