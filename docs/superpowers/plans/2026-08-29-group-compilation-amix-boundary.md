# Group compilation mixed-audio boundary fix

## Goal

Keep mixed microphone/output audio alive through each clip's declared duration so FFmpeg can
advance the concat filter to the next group member.

## Reproduction

- [ ] Export the real two-member subset beginning with
      `clip_1787638013_trim_001500_030008.mp4` and observe FFmpeg fail at the first segment EOF
      with `-1094995529 (Invalid data found when processing input)`.
- [ ] Confirm both source files decode cleanly and the former single-audio graph succeeds.

## TDD implementation

- [ ] Add a failing argument contract requiring the multi-track `amix` output to be padded and
      trimmed to the member's known movie duration before its timestamps are reset.
- [ ] Bound the existing mixed-audio branch with FFmpeg's native `apad` and `atrim` filters.
- [ ] Rerun the exact two-file reproduction and the full five-file compilation.
- [ ] Run workspace tests and strict Clippy, update `handoff.md`, commit, and relaunch Clipline.
