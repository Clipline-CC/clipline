# Review Audio Handover

> **For agentic workers:** Execute this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for
> tracking and remain unticked by repository convention.

**Goal:** Opening a clip in the review player no longer produces a brief audio repetition at the
start. The clip's opening audio is preserved — silence is not an acceptable trade — and the sidecar
handover becomes deterministic rather than racing an unresolved seek.

**Observed:** for a split second at the start of every clip the audio sounds like it plays twice.
Seeking back to the beginning and replaying is clean, and the same file is clean in VLC. Both
observations point at the handover rather than the recording: on a replay the player is already in
sidecar mode with the drift timer aligned, and VLC only ever plays the file's own tracks.

**Root cause.** The player does not play the clip's embedded audio during normal playback. It plays
separate sidecar `<audio>` elements against a muted video so multi-track selection works
(`reviewAudioOutputDecision`, `player-core.js:2014`: `direct` = video audible, `sidecars` = sidecars
audible, `muted` = neither). `activatePreparedReviewAudioSidecars` (`review-player.js:248`) performs
**two** forced seeks before switching output:

1. First sync (`:256`) — sidecars were prepared with `allowPlayback: false` so they are paused.
   `shouldPlay` therefore pushes a play promise, which *is* awaited. Sidecars are now playing.
2. Second sync (`:267`) — the sidecars are already playing, so `if (audio.paused)`
   (`:174`) is false, **no promise is pushed**, and `await Promise.all(playPromises)` (`:179`)
   resolves against an empty array. The `audio.currentTime = decision.seekTime` assignment (`:172`)
   has started a seek that nothing waits for.
3. `:275-277` mutes the previous set and flips `reviewAudioMode` to `sidecars`, unmuting the new
   sidecars **while that seek is still in flight**.

Setting `currentTime` starts an asynchronous seek; per the HTML media spec the element may report
the target time before data for that position is decoded, and `seeked` is the completion boundary.
So the sidecar can emit pre-seek audio while audible — the repetition comes from *within one
sidecar*, not from video and sidecar overlapping.

**Why not simply start muted.** Rejected: `review_audio_defaults_to_direct_fallback_and_explicit_tracks_need_preview`
(`tests/player_core.rs:1180`) pins immediate direct fallback as deliberate design; cold sidecar
generation spawns FFmpeg and can take on the order of seconds, so muting until ready would discard
real opening audio; `handleReviewAudioSidecarFailure` (`:183`) reverts to `direct` only on failure,
so a pending-muted state has no exit for clips that never request sidecars or carry no track
metadata. Masking the symptom would also leave the unresolved-seek race in place for every later
drift correction.

## Task 1: Instrument the handover before changing behaviour

No behaviour change in this task. The mechanism above is read from source; confirm it on the wire
first so the fix is measured against something real.

- [ ] Add temporary diagnostics around the activation path: each `currentTime` assignment with its
      target, `seeking`, `seeked`, play-promise resolution, the `reviewAudioMode` switch, and the
      first drift-timer tick — each with a monotonic timestamp and the sidecar's `audioTrackId`.
- [ ] Capture traces for a **cold** cache (no `audio-preview-*.mp4` present, forcing extraction) and
      a **warm** cache, since the timing differs by orders of magnitude.
- [ ] Confirm the predicted ordering: the second forced seek's `seeked` lands *after* the mode
      switch. Record the gap. If it lands before, this root cause is wrong — stop and re-diagnose
      rather than implementing against a theory.
- [ ] Note whether the first drift tick (`:209`, 500 ms) issues its own backward seek, which would
      be a second, independent source of repetition.

### Task 1 outcome — ordering confirmed, magnitude complicates it

Captured on a release build via CDP, cold cache (`audio-previews` emptied) and warm.

**The predicted ordering is confirmed verbatim, in both runs.** Cold:

```
205.9ms  sync_exit                    phase=activate_second  awaited=0
206.0ms  output_switched_to_sidecars  seeking=true (both sidecars)
207.8ms  sidecar_seeked               phase=activate_second  muted=False
```

`awaited=0` proves the second pass pushed no play promise, and the switch happens with
`seeking: true` on every sidecar. Warm is identical in shape (switch 92.3 ms, `seeked` 94.2 ms).
So the plan's stop-condition did **not** trigger — the root cause stands.

**But the wall-clock window is only ~2 ms**, which alone is too short to perceive as a repeat. Two
readings, and the fix covers both:

1. **The leaked *audio* is not bounded by that 2 ms.** A media element can have a decoded buffer of
   tens of milliseconds already queued; unmuting for even 2 ms mid-seek can let a whole pre-seek
   buffer through. This remains the most likely explanation and is consistent with the symptom.
2. **The video→sidecar crossover is a second candidate.** Until the switch the video's own audio is
   audible (mode `direct`, 206 ms cold / 92 ms warm of real playback). Muting a media element does
   not necessarily flush already-queued audio, so both copies of the same content can briefly
   overlap at the instant of handover.

A stationary handover (Task 3) addresses both, because switching while paused means no audio is
flowing from either source at the switch instant. Note this is a *different* reason than "await
`seeked`" alone — keep the pause, do not reduce Task 3 to just gating on `seeked`.

**Two further measurements worth recording:**

- **The drift timer never corrects.** Cold, at the first tick the sidecars sit ~71 ms behind the
  video (video 0.224 s, sidecars 0.153 s) and every subsequent sync reports `forceSeek=False` with
  no seek assigned — `audioSidecarSyncDecision` treats that offset as within tolerance. So there is
  a persistent, uncorrected A/V offset after handover, separate from the artefact under
  investigation. Out of scope here; worth its own look.
- **Cold sidecar generation measured ~202 ms, not 0.5–2 s.** That was one argument for rejecting
  muting-at-open, and on this hardware and clip length it does not hold. The decision stands on the
  other grounds — the tested direct-fallback invariant, and no exit path from pending-muted for
  clips that never request sidecars — but the timing claim should not be repeated as fact.

## Task 2: Failing tests for a completion-gated stationary handover

- [ ] Extend `player-core.js` with a pure decision for the handover — given video state, sidecar
      states and whether a seek is outstanding, report whether output may switch yet. Test via
      `tests/player_core.rs` (boa_engine) per the repo's DOM-free-logic convention: switching is
      refused while any current-generation sidecar has an unsatisfied seek target, permitted once
      all are satisfied, and unaffected by a *stale* generation's outstanding seek.
- [ ] Test that the decision preserves the paused/playing state it was given, so a user who paused
      during extraction is not resumed by the handover.
- [ ] Confirm `review_audio_defaults_to_direct_fallback_and_explicit_tracks_need_preview` still
      passes untouched — the direct-fallback invariant is not what is changing.
- [ ] Run the focused tests and confirm they fail on current code.

## Task 3: Stationary, completion-gated handover

- [ ] In `activatePreparedReviewAudioSidecars`, replace the second unguarded forced seek with:
      pause the video, capture that now-stationary time, seek every still-muted current-generation
      sidecar to it, and **await `seeked` on each** before switching output.
- [ ] Switch output while paused, then restore the transport to the state captured before the pause
      — resume only if it was playing. Route the resume through the existing play-state path so
      `syncPlayState` and the drift timer are not fighting it.
- [ ] Bound the wait: a sidecar that never fires `seeked` must not hang the handover. On timeout,
      fall back to `handleReviewAudioSidecarFailure`'s existing `direct` recovery rather than
      unmuting into an unknown position.
- [ ] Preserve every existing escape hatch: stale generation checks (`previewRequestStillCurrent`),
      cancellation, preview failure, and manual Play during the handover.
- [ ] Do **not** crossfade. Crossfading two misaligned copies produces echo or comb filtering —
      strictly worse than the current artefact. Alignment must be proven by Task 1's trace first.

## Task 4: Fallback if the stationary handover proves unreliable

Only if Task 3's trace still shows repetition or the pause is perceptible as a hitch.

- [ ] Conditional deferred autoplay: delay autoplay **only** for selections that require sidecars,
      leaving direct and no-metadata clips immediate. This preserves the clip's opening rather than
      discarding it.
- [ ] It needs a real pending-start state covering preview failure, cancellation, manual Play before
      readiness, and stale generations — the absence of which is why muting-at-open was rejected.
      Do not add it without those paths covered.

## Task 5: Verify

- [ ] Remove Task 1's temporary diagnostics, or promote only what is durably useful behind the
      existing structured-diagnostics facility.
- [ ] `cargo test --workspace` green; fresh-cache warning-denied Clippy.
- [ ] Manual, cold cache: open several clips and confirm the opening audio is present, correct, and
      free of repetition. Cold is the important case — the extraction window is where the race lives.
- [ ] Manual, warm cache: same, plus switching audio-track selection mid-playback, which re-enters
      the same activation path.
- [ ] Manual: pause during extraction and confirm the handover does not resume playback; press Play
      during extraction and confirm it is honoured.
- [ ] Manual: a clip with a single audio track and a clip with no track metadata both play audio
      immediately, confirming direct fallback is untouched.
- [ ] Update `handoff.md`; commit as one conventional change per task.
