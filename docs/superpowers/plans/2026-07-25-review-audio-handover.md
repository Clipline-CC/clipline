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

### Task 1 outcome — the predicted race is confirmed; its audible effect is not yet proven

Captured on a release build via CDP, cold cache (`audio-previews` emptied) and warm.

**The predicted ordering is confirmed verbatim, in both runs.** Cold:

```
205.9ms  sync_exit                    phase=activate_second  awaited=0
206.0ms  output_switched_to_sidecars  seeking=true (both sidecars)
207.8ms  sidecar_seeked               phase=activate_second  muted=False
```

`awaited=0` proves the second pass pushed no play promise, and the switch happens with
`seeking: true` on every sidecar. Warm is identical in shape (switch 92.3 ms, `seeked` 94.2 ms).
The stop-condition did not trigger: **the predicted race is real and that invariant is broken.** But
it does not follow that this race is what is audible — see the stronger candidate below.

**A second, unguarded forced seek fires on already-audible sidecars.** The full warm trace shows,
*after* the sidecars are unmuted at 92.3 ms:

```
117.7ms  sync_enter   phase=sync  forceSeek=False       <- drift path, assigns no seek
118.1ms  sync_enter   phase=sync  forceSeek=True
118.2ms  sidecar_seek_assigned  from=0.014 target=0.000845 muted=False  <- BACKWARD, audible
118.4ms  sidecar_seek_assigned  from=0.014 target=0.000845 muted=False
124.0ms  sidecar_seeked         target=0.000845 currentTime=0.014 muted=False
```

That is a **backward seek on an audible element**, ~26 ms after it became audible and resolving ~6 ms
later — the textbook shape of a brief repeat, and a far better fit for the symptom than a 2 ms
control-plane window. The harness issued no seek. The caller is
`syncReviewAudioSidecars({ forceSeek: true })` (`review-player.js:1317`) inside `seekTo` (`:1288`),
which also applies `video.currentTime` at `:1311` — the initial video source/seek settlement drives a
forced sidecar seek.

**Task 3's activation-only barrier would not cover this.** Establish the origin first (Task 1b), then
decide whether activation must wait for the initial video seek to settle, or whether *every* forced
sidecar seek needs completion gating while audible.

**Corrections, each of which overstated the evidence:**

- **The ~2 ms figure is not a measurement of audible audio.** `output_switched_to_sidecars` is logged
  *before* `applyReviewAudioOutput()`, and console/CDP logging perturbs the event loop. It bounds a
  control-plane ordering and nothing more.
- **"206 ms cold / 92 ms warm of real direct playback" was wrong.** Those are wall-clock times since
  source assignment, not media time. At the cold switch the video timeline was ~50 ms in; at the warm
  switch it was still at **0**. So the video→sidecar crossover cannot explain the warm case, and that
  candidate is much weaker than claimed.

- **The ~202 ms is one clip's cold time-to-handover, not isolated FFmpeg generation.** The trace
  starts at source assignment with no backend invoke start/end markers. The historical 0.5–2 s figure
  remains valid for the 31-minute clip it came from. Neither number is a universal expectation, and
  neither should be used to re-argue muting-at-open — that decision stands on the tested
  direct-fallback invariant and the missing exit path from pending-muted.
- **The "drift never corrects" claim is overstated.** The `drift_first_tick` flag is consumed by the
  next `syncReviewAudioSidecars` call from *any* source — play, pause, timeupdate, seeking, rate
  change, or the timer — and cold shows it 159 ms after the switch, so it is not the 500 ms interval.
  It proves one ~71 ms clock-skew snapshot; the later absence of seeks proves only that skew stayed
  under the intentional gross-discontinuity threshold, and `player_core.rs` deliberately permits
  ±100 ms during playback. Drift-controller changes stay out of scope, but post-resume skew becomes an
  acceptance measurement for Task 3, since a stationary handover may remove the startup offset for
  free.

## Task 1b: Establish the origin of the post-unmute forced seek

- [ ] Confirm what invokes `seekTo` at ~118 ms warm — the `loadedmetadata` / source-assignment resume
      path is the prime suspect. Trace the caller, not just the effect.
- [ ] Decide the boundary that implies: either activation must not complete until the initial video
      seek has settled, or **every** forced sidecar seek must be completion-gated while audible.
      Record the choice and its reasoning; it determines whether Task 3 suffices on its own.
- [ ] Check the same window on the cold trace, where ordering may differ because activation lands
      later relative to source settlement.
- [ ] Retain sanitized cold and warm traces in the repo (or attach them to the PR). The complete
      evidence currently exists only under `%TEMP%` and will not survive.

### Task 1b outcome — origin proven, and the fault is ordering-dependent

The origin is **not** `seekTo`. An earlier note in this plan mis-attributed it: line 1317 sits inside
the top-level `video.addEventListener("seeked", …)` handler (`review-player.js:1303`), not inside
`seekTo` (`:1288`). Instrumented and re-traced; no `seek_to` event appears at all, so no explicit
reposition is involved.

**Warm — the forced seek lands on audible sidecars:**

```
 86.8ms  output_switched_to_sidecars       (sidecars unmuted)
116.8ms  video_seeked_forces_sidecar_sync  videoTime=0.0014  metadataGeneration=1
                                           audioMode=sidecars  sidecarsAudible=True
116.9ms  sidecar_seek_assigned  from=0.021791  target=0.0014  muted=False   <- backward ~20ms
123.3ms  sidecar_seeked         target=0.0014  currentTime=0.014  muted=False
```

`sidecarsAudible=True` is now measured, not inferred. `metadataGeneration === sourceGeneration` and
`appliedTime` empty confirm this is the **initial source settlement**, not a user reposition.

**Cold — the same handler fires harmlessly:**

```
 99.4ms  video_seeked_forces_sidecar_sync  audioMode=direct  activeSidecars=0  sidecarsAudible=False
175.6ms  output_switched_to_sidecars
```

Cold contains **no** forced sidecar seek while unmuted at all.

**So the fault is ordering-dependent:** it occurs only when the sidecar handover completes *before*
the video's initial seek settles. Warm handover is fast (~87 ms) and loses that race; cold handover
is slow (~176 ms) and wins it. Since a populated preview cache is the normal steady state, warm is
the case users actually hit — consistent with the artefact being reported on *every* clip.

This also gives a discriminator between the two candidate mechanisms: the activation race (unmute
mid-seek) is present in **both** cold and warm, whereas the audible backward seek is **warm-only**. An
artefact heard consistently therefore points at the warm-only seek.

**The seek is also pure churn.** `forceSeek: true` bypasses the tolerance entirely
(`seekTime: validVideoTime && forceSeek ? videoTime : null`), and the drift was ~20 ms against
`AUDIO_SIDECAR_HARD_SEEK_TOLERANCE_S = 0.5` / `AUDIO_SIDECAR_DRIFT_DEADBAND_S = 0.025`. The
non-forced drift sync 0.4 ms earlier assigned no seek. After seeking backward to 0.0014 the element
landed at 0.014 — where it already was. It pays an audible cost for nothing.

**Boundary decided: Task 3's activation barrier is necessary but NOT sufficient. Both are required.**

- **Primary:** the initial source-settlement `seeked` must not force a sidecar seek. Forcing is
  correct for a genuine user reposition — after a scrub you want immediate realignment rather than up
  to 500 ms of tolerance — so distinguish settlement from reposition rather than dropping the force
  outright.
- **Defence in depth:** never issue a seek on an *audible* sidecar without completion gating. The same
  unguarded pattern applies to user scrubs, where it is merely masked by the scrub itself.

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

### Task 2b outcome — five corrections to the first test pass

**A guard was already failing and I reported green.** `ui_contract.rs:2134` builds a prohibited
identifier as `["pending", "Seek"].concat()` — deliberately obfuscated so the guard cannot trip on
itself — and scans `tests/player_core.rs` via `include_str!`. My first test block used exactly that
name. It did not show up because `cargo test --workspace` reused a **stale `ui_contract` binary**:
cargo did not rebuild it when only `player_core.rs` changed, so the embedded copy was the old file.
It only surfaced after `touch`ing `ui_contract.rs`. **Any change to a file consumed by `include_str!`
needs the consuming test touched, or the run is meaningless.** `reviewSeekRevision` and
`audioPreviewSeq` are prohibited in `review-player.js` too.

The identifier ban is not cosmetic: a single shared pending-seek flag previously caused ownership
bugs. Several seeks overlap inside one generation — prepare, first activation, final alignment, and
possibly a user seek — so a late `seeked` can clear a flag belonging to a newer assignment.
**Ownership is therefore per-sidecar and token-revisioned:** each carries `seekToken` and
`settledToken`, settlement matches tokens, stale generations are ignored rather than globally
cleared, and disposal aborts its own listeners and timeout.

Four further gaps the first pass left open, now pinned:

- **Provenance across source replacement.** `beginSourceAssignment` deliberately carries a pending
  logical target; if that target came from the user it must keep `targetSource: "user"` rather than
  being relabelled `assignment`, or a replacement silently downgrades a real reposition.
- **Stale completions must not confirm newer targets.** `assignment(0) → user(12) → seeked(0)` must
  yield `confirmed=false`, `confirmedSource=null`, `applyTime=12`, and state still carrying
  `targetSource="user"`. Only `seeked(12)` confirms.
- **A target already satisfied settles without `seeked`.** Browsers may skip the event for a no-op
  seek; without this the backstop timeout becomes the normal path.
- **Mid-handover user seeks.** A user seek does not advance the sidecar-selection generation, so
  sidecars can settle at the captured `t0`, look "ready", and switch while the video targets `t1`.
  The transaction carries a transport revision and **restarts** the alignment when it changes.

**The timeout decision in the first pass was wrong, not merely incomplete.**
`hadAudibleSidecars ? "sidecars" : "direct"` discards a valid previous `muted` mode. Reachable: the
user selects no tracks (entering `muted`), then requests tracks needing sidecars, and preparation
times out — the clip's embedded audio would become audible against an empty selection. It now
restores the complete previous state: mode (`direct` / `sidecars` / `muted`), the previous sidecar
set, and transport.

**Wiring is now guarded separately.** Every pure test above would pass with the helpers exported but
never called, leaving behaviour unchanged. `review_player_wires_the_audio_handover_decisions` pins
that the `seeked` handler consults `sidecarRealignmentForced` with `confirmedSource`, that
`sidecarHandoverDecision` is consulted **before** the output switch (by source position, not just
presence), and that the resume and timeout decisions are used rather than a raw snapshot.

## Task 3: Stationary, completion-gated handover

- [ ] Use per-sidecar `seekToken` / `settledToken` ownership from Task 2b. Do **not** reintroduce a
      shared pending flag — `ui_contract.rs` prohibits that identifier, and the ownership bug it
      caused is the reason. Disposal and cancellation must abort their own listeners and timeout
      rather than clearing shared state.
- [ ] **Pause all three sets, not just the video.** Pausing the video does not pause the prepared
      sidecars: they were started during the first activation pass but are not yet in
      `activeReviewAudioSidecars`, so no video pause handler reaches them. "Nothing is flowing" is
      false unless this explicitly pauses (a) the video/direct source, (b) every *prepared* sidecar,
      and (c) the currently active sidecars when this is a mid-play track change. Otherwise
      early-finishing sidecars keep advancing while slower ones seek.
- [ ] In `activatePreparedReviewAudioSidecars`, replace the second unguarded forced seek with: pause
      everything above, capture the now-stationary time, seek every still-muted current-generation
      sidecar to it, and **await `seeked` on each** before switching output.
- [ ] Switch output while paused, then restore the transport to the **latest user intent**, not merely
      the state captured before the pause — the user may have hit play, pause, or seek during the
      internal pause, and their action must win. Route the resume through the existing play-state path
      so `syncPlayState` and the drift timer are not fighting it.
- [ ] Bound the wait: a sidecar that never fires `seeked` must not hang the handover. On timeout,
      **preserve whatever was previously audible** — for a sidecar-to-sidecar track change, calling
      `handleReviewAudioSidecarFailure` unconditionally would destroy a valid existing selection and
      drop back to `direct`. Only fall back to `direct` when there was no prior audible sidecar set.
- [ ] Measure post-resume skew as acceptance (see Task 1b's correction): a stationary handover should
      not leave the sidecars tens of milliseconds behind the video.
- [ ] **Stop the initial source settlement from forcing a sidecar seek** (`review-player.js:1303`).
      Per Task 1b this is the warm-only, audible, backward seek and the likeliest cause of the
      reported artefact — the activation barrier alone does not touch it. Distinguish settlement from
      a genuine reposition rather than removing the force, which scrubs still need.
- [ ] **Gate any seek issued to an audible sidecar** on `seeked`, so the same pattern cannot bite on
      user scrubs where it is currently just masked.
- [ ] Re-trace cold **and** warm after the fix. Warm is the case that must change; cold must not
      regress, and its handover currently wins the race by being slow — do not let a faster handover
      silently introduce the warm fault into cold.
- [ ] Preserve every existing escape hatch: stale generation checks (`previewRequestStillCurrent`),
      cancellation, preview failure, and manual Play during the handover.
- [ ] Do **not** crossfade. Crossfading two misaligned copies produces echo or comb filtering —
      strictly worse than the current artefact. Alignment must be proven by Task 1's trace first.

### Task 3 outcome — both mechanisms gone from the traces

Re-traced on a release build, cold and warm. **Neither run now contains a single sidecar seek
assigned while unmuted** — the query that previously returned the warm backward seek returns nothing.

| | Before | After |
|---|---|---|
| Warm: audible backward seek | `from=0.014 → target=0.000845`, `muted=False` | **none** |
| Warm: state at output switch | `seeking: true` on both sidecars | `seeking: false` on both |
| Warm: settlement `seeked` | forced a realignment | `confirmedSource=assignment`, `forceSeek=False` |
| Cold: sidecar vs video at switch | — | exact match (both `0.061838`) |
| Cold: post-resume skew | ~71 ms | **~27.5 ms** |

The stationary alignment lands sidecars exactly on the video's position, and `handover_resume`
reports `play=True intent=internal-pause wasPlaying=True` — the transaction's own pause is correctly
not read as user intent. Post-resume skew improved as the plan speculated it might, and is inside the
±100 ms the drift controller permits by design.

Two contracts were **deliberately changed**, not merely repaired:

- `audio_sidecar_transport_follows_only_the_video_clock` pinned the literal
  `syncReviewAudioSidecars({ forceSeek: true });` — i.e. it *encoded the bug*, requiring every video
  `seeked` to bypass the drift tolerance. It now pins provenance-driven forcing instead.
- `valid_sidecar_activation_reads_latest_player_state_without_swapping_video` pinned activation's
  old snapshot literals. Its intent — read live state, never swap the video source — is preserved and
  arguably strengthened, since the playhead is now re-read per alignment attempt. It now also pins
  that the prepared *and* previous sets are paused, which is the correction that made "stationary"
  true.

**Still outstanding: the loopback waveform comparison.** Everything above is control-plane. The
traces prove no audible element is seeked and no unmute happens mid-seek, which removes both
candidate mechanisms — but only a recording of the actual output can confirm the repeat a listener
hears is gone.

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
- [ ] **Loopback waveform comparison, not DOM state.** Record the system output while opening a clip
      and compare the first second against the source audio. DOM and trace state cannot prove what
      reached the speakers, and every timing figure in this plan is control-plane only. This is the
      only evidence that closes the question of whether the repeat is fixed.
- [ ] Manual, cold cache: open several clips and confirm the opening audio is present, correct, and
      free of repetition. Cold is the important case — the extraction window is where the race lives.
- [ ] Manual, warm cache: same, plus switching audio-track selection mid-playback, which re-enters
      the same activation path.
- [ ] Manual: pause during extraction and confirm the handover does not resume playback; press Play
      during extraction and confirm it is honoured.
- [ ] Manual: a clip with a single audio track and a clip with no track metadata both play audio
      immediately, confirming direct fallback is untouched.
- [ ] Update `handoff.md`; commit as one conventional change per task.
