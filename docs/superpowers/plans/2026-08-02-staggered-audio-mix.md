# Staggered Audio Mix Plan

**Goal:** Let clipboard shares and cloud uploads mix selected Opus tracks whose packet timelines start at different 48 kHz ticks without producing overlapping MP4 samples.

## Reproduce

- [ ] Add a finalized MP4 fixture with two valid stereo Opus tracks offset by half of a 20 ms packet.
- [ ] Exercise the file-backed mixed-audio remux and prove the current event-at-a-time mixer fails with `overlapping or backward sample presentation times`.

## Fix

- [ ] Mix decoded input packets on one continuous 48 kHz PCM timeline and emit non-overlapping 20 ms Opus packets.
- [ ] Preserve long presentation gaps without encoding unbounded silence, and average simultaneous sources before clamping.
- [ ] Route the in-memory helper through the same streaming implementation so upload/share paths cannot diverge.

## Verify

- [ ] Run the focused staggered-mix regression and the complete `clipline-mp4` test suite.
- [ ] Run `cargo test --workspace` and `cargo clippy --workspace --all-targets -- -D warnings`.
- [ ] Update `handoff.md`, rebuild, and open Clipline for a live selected-audio share/upload check.
