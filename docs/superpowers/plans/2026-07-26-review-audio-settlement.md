# Review Audio Settlement

> **For agentic workers:** Execute this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for
> tracking and remain unticked by repository convention.

**Goal:** Stop the split-second audio repeat at the start of every clip, with the smallest change that
addresses its measured cause. Nothing else.

**Scope discipline is the point of this plan.** The investigation that found this cause also built a
much larger hardening layer — a stationary handover transaction, per-sidecar seek tokens, a transport
epoch, and seek silencing. That work is preserved as reference on PR #108 (branch
`review-audio-handover`, plan `2026-07-25-review-audio-handover.md`, with cold/warm traces) but is
**deliberately not included here**: four review rounds found P1 concurrency defects in it, two of them
introduced by the previous round's fix. It is being rebuilt from `develop` around a smaller state model
as separate work. This plan ships only the cause fix.

## Measured cause

The review player does not play the clip's embedded audio. It plays separate sidecar `<audio>` elements
against a muted video so multi-track selection works. `audioSidecarSyncDecision` normally tolerates
small drift, but `forceSeek: true` bypasses the tolerance entirely — and the `video` `seeked` handler
passed it unconditionally.

The **initial source settlement** reaches that handler too. Traced on a warm cache (full traces on
#108):

```
 86.8ms  output_switched_to_sidecars       (sidecars become audible)
116.8ms  video_seeked  confirmedSource=assignment  sidecarsAudible=true
116.9ms  sidecar_seek_assigned  from=0.021791  target=0.0014  muted=false   <- backward, audible
123.3ms  sidecar_seeked         target=0.0014  currentTime=0.014  muted=false
```

A backward seek on an audible element, ~26 ms after it became audible. Setting `currentTime` starts an
asynchronous seek and the element may keep emitting already-decoded pre-seek audio until `seeked`, so
the same fragment is heard twice.

The seek was also **pure churn**: drift was ~20 ms against `AUDIO_SIDECAR_HARD_SEEK_TOLERANCE_S`
of 0.5 s, the non-forced drift sync 0.4 ms earlier assigned no seek at all, and after seeking backward
to 0.0014 the element landed at 0.014 — where it already was.

**Ordering-dependent, which is why it looked intermittent.** Warm handover (~87 ms) beats the video's
initial seek (~117 ms) and loses the race; cold handover (~176 ms) arrives after it and is unaffected. A
populated preview cache is the steady state, so warm is what users hit.

## Task 1: Failing tests for seek provenance

- [ ] `beginSourceAssignment` tags its resume seek `"assignment"`; `requestLogicalSeek` tags `"user"`
      and wins over a pending assignment.
- [ ] A carried user target keeps `"user"` across a source replacement — `beginSourceAssignment`
      deliberately carries a pending target, and relabelling it would downgrade a real reposition.
- [ ] `seekedDecision` reports `confirmedSource` when it confirms, and a completion for a superseded
      position re-applies the current target with provenance intact so only the real arrival confirms.
- [ ] `sidecarRealignmentForced` is true only for `"user"`.
- [ ] Run the focused tests and confirm they fail on current code.

## Task 2: Drive forcing from provenance

- [ ] Thread `targetSource` through the logical seek state; `metadataSeekDecision` already spreads
      state, so it carries automatically.
- [ ] `seekedDecision` returns `confirmedSource`.
- [ ] The `video` `seeked` handler passes
      `forceSeek: PlayerCore.sidecarRealignmentForced(decision.confirmedSource)`.
- [ ] Update `audio_sidecar_transport_follows_only_the_video_clock`, which pins the literal
      `forceSeek: true` — it encodes the bug. Assert the provenance call instead, and that
      `forceSeek: true` is gone.
- [ ] Keep forcing for user repositions: after a scrub the sidecar is far from the new position and
      tolerance alone would leave it there.

## Task 3: Verify

- [ ] `cargo test --workspace` green; fresh-cache warning-denied Clippy.
- [ ] Manual, **warm** cache: open a clip, go back, open it again, and confirm the repeat is gone.
      Warm is the case that carries the fault; a cold first open never had it.
- [ ] Manual: scrub during playback and confirm audio still realigns immediately — this is what the
      retained `"user"` forcing protects, and losing it would be a regression this change could cause.
- [ ] Manual: a single-track clip and a clip with no audio-track metadata both still play audio
      immediately, confirming the direct-fallback path is untouched.

## Explicitly out of scope

Tracked for the rebuild, not fixed here. None is a regression from this change; all predate it.

- The activation handover can still unmute a sidecar while its seek is in flight (the second forced
  activation seek awaits no play promise). Present in both cold and warm, and **unproven as audible** —
  the measured artefact is the warm-only settlement seek above.
- User scrubs still seek audible sidecars, the same class of exposure, merely masked by the scrub.
- `applyReviewAudioOutput` writes `muted` globally, so a volume change during a seek can unmute
  mid-flight.
- Preview activations can overlap, because the queue advances its state before awaiting activation.
- Post-commit failures and stale completions are reported as success.
- A brief echo when switching audio tracks mid-playback: the outgoing set is muted in the same tick the
  incoming set is unmuted, and muting does not flush already-queued audio, so two *aligned* copies
  overlap. Confirmed by ear and accepted.
