# Group compilation mixed-audio boundary fix

## Goal

Give every mixed microphone/output audio frame a monotonic timestamp so FFmpeg can advance the
concat filter to the next group member.

## Reproduction

- [ ] Export the real two-member subset beginning with
      `clip_1787638013_trim_001500_030008.mp4` and observe FFmpeg fail at the first segment EOF
      with `-1094995529 (Invalid data found when processing input)`.
- [ ] Confirm both source files decode cleanly and the former single-audio graph succeeds.

## TDD implementation

- [ ] Add a failing argument contract requiring the multi-track `amix` output to rebuild timestamps
      from audio sample count with `asetpts=N/SR/TB`.
- [ ] Replace the mixed branch's `PTS-STARTPTS` reset with the sample-count expression; keep
      `duration=longest` so the longer microphone/output tail is preserved.
- [ ] Rerun the exact two-file reproduction and the full five-file compilation.
- [ ] Run workspace tests and strict Clippy, update `handoff.md`, commit, and relaunch Clipline.
