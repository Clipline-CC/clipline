# Review Audio Alignment

> **For agentic workers:** Execute this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for
> tracking and remain unticked by repository convention.

**Goal:** No sidecar is ever audible while its alignment is outstanding, and playback intent survives
every handover. The measured cause of the clip-start repeat already shipped (#109); this addresses the
remaining class of the same defect on scrub, track-switch, and overlapping-selection paths.

## Why this is a rebuild and not a continuation

A previous attempt (#108, closed) fixed the same class by accretion: a stationary handover transaction,
then per-sidecar seek tokens, then a transport epoch, then seek silencing. Four review rounds found P1
concurrency defects, **two of them introduced by the previous round's own fix**, and its runtime
evidence was circular — token convergence was reported as proof while a safety timer forced that
convergence unconditionally.

The lesson is recorded here because it dictates the design: every one of those bugs lived in the
*interaction* between mechanisms that each wrote `audio.muted` from a callback. So the invariant is
**one writer over derived state**, and the gate comes first and must be proven red.

## The invariant

`audio.muted` is written by exactly one reconcile function, computed from state. Callbacks *request*
reconciliation; they never write. A sidecar may be audible only if **all** hold:

- The output mode makes sidecars audible at all (`sidecars`, not `direct` or `muted`).
- It is the **current** generation's active set — prepared, stale-generation, and inactive sidecars are
  ineligible **by construction**, not by check order.
- It has **no outstanding alignment**: authoritative state, not `settledToken >= seekToken`.
  Clearing it requires the current claim **and** `!audio.seeking` **and**
  `|currentTime - target| <= tolerance`.

A timeout may warn, retry, or roll back. It may **never** manufacture readiness — that is precisely how
#108's evidence became circular.

## Task 1: The gate, proven red on current HEAD

**No production code changes in this task.** If the gate cannot be made to fail on `develop`, it does
not measure the defect and nothing built behind it can be trusted.

- [ ] Add `scripts/gate-review-audio-alignment.ps1`. It **asserts and exits non-zero**; it does not
      print and leave interpretation to a human.
- [ ] Hard-fail rather than pass on: a CDP evaluation error, an empty sidecar set, a missing clip, or a
      page that is not Clipline. Two harnesses in the previous attempt reported clean while measuring
      nothing — once because it queried `<audio>` elements that are created with `new Audio()` and
      never appended, once because it aligned against the wrong content. **Silence must fail.**
- [ ] Install an in-page sampler that records **every** `muted` transition during the race, with
      `seeking`, `currentTime`, and a timestamp — not a snapshot afterwards. The violation is a sample
      where `muted === false` **and** `seeking === true`: audible while its seek is in flight.
- [ ] Drive **real UI paths** — the clip card, the seek bar, the track selector, the play control —
      rather than assigning `video.currentTime` directly, which bypasses the code under test.
- [ ] Assert **zero timeout-based settlements**: any alignment that completes only because a timer
      fired is a failure, not a pass.
- [ ] Assert the final state: the selected track ids match what was requested, and desired playback
      matches actual.
- [ ] **Run it on `develop` and confirm it fails.** `review-player.js:171` assigns `currentTime` on an
      audible element with no muting, so a scrub during playback must produce violations. Record the
      failing output in this plan.

### Task 1 outcome — a valid negative control, not yet a durable gate

`scripts/gate-review-audio-alignment.ps1` failed on `develop` as required: **34 samples** with a
sidecar observed `muted === false` while `seeking === true`, both tracks, reproducible across runs.
That is real evidence that `develop` exposes a large, observable unsafe state.

**It cannot become the pass criterion, because a green from it would prove nothing.** Polling at 4 ms
via `setInterval` can miss a `false → true → false` transition entirely — and the windows measured
earlier in this investigation were ~2 ms. Absence of samples is not absence of the state. Recorded
here so no future run is read as proof:

- **Sampling cannot prove a negative.** The durable assertion has to come from instrumenting the
  single writer (Task 3), where every write to `muted` is recorded *by construction* with its
  justification. An audit log can be complete; a poll cannot.
- **It observes only `activeReviewAudioSidecars`.** Prepared, outgoing, and stale sets — the track-switch
  participants that matter most — are invisible to it.
- **It checks native `seeking`, not authoritative outstanding alignment.** Assigned-but-not-yet-seeking,
  and off-target after a stale completion, both pass.
- **`muted === false && seeking === true` proves output *eligibility*, not emission.** Paused state and
  volume are not recorded, so an unmuted-but-paused element counts as a violation and an unmuted
  zero-volume element counts as audible. The loopback comparison remains the only direct audio evidence.

**Two claims made when reporting this run were unsupported and are withdrawn:**

- "2 muted transitions in 2733 samples" — the sampler counts a first observation (`!prev`) as a
  transition, so those two were the two tracks' initial reads. There were **zero** actual transitions,
  which strengthens the finding (`develop` never mutes) but the figure as stated was wrong.
- "Violations cluster in the scrub window, not at clip open" — the sampler is installed *after* both
  clip opens, so this run contributes **no clip-open observation at all**. Violations begin near
  +2009 ms because the script sleeps two seconds and then scrubs. #109's correctness rests on its own
  traces, not on this run.

Harness defects to fix before it is trusted for anything but the negative control, each a false-green
risk:

- [ ] Every exercise's return value is discarded (`| Out-Null`, 9 sites). `no-seek-path`,
      `single-track`, and a failed play must fail the gate, not pass silently.
- [ ] Playback uses `video.play()` with the rejection suppressed and always reports `playing`; it must
      click the real play control and verify the result.
- [ ] The track-switch exercise toggles the last checkbox, which on an output+microphone fixture likely
      lands in **output-only `direct` mode** — bypassing sidecar preparation and activation entirely.
      The observed terminal `mode=direct` corroborates this. It must assert it actually entered a
      sidecar switch.
- [ ] Fabricated `pointerdown/move/up` in one task hit `setPointerCapture(ev.pointerId)`
      (`review-player.js:1457`), which may throw for a pointer id that was never active. Use genuine
      CDP `Input.dispatchMouseEvent`, and subscribe to `window.error` / `unhandledrejection` so an
      async handler failure cannot pass unnoticed.
- [ ] Terminal assertions promised by the plan are absent: no zero-timeout-settlement check, and
      desired playback is never compared with actual (`videoPaused` is collected and unused).
- [ ] Active ids are compared with `currentReviewAudioTrackIds` — two app-produced values that can be
      stale together. Compare against the selection the **harness** requested, captured before acting.
- [ ] `Cdp` inspects `exceptionDetails` but not protocol-level errors or a missing result.

## Task 2: Deterministic state-transition tests

Pure, in `player-core.js`, run under Boa on both CI OSes. Each is a sequence, not a single decision.

- [ ] **Stale `seeked`:** claim A, claim B supersedes it, A's completion arrives — must not clear B's
      outstanding alignment nor make the element eligible.
- [ ] **Timeout:** an alignment that has not satisfied its target stays outstanding; the timeout
      surfaces and may roll back, but never reports ready.
- [ ] **Volume/mute change mid-alignment:** reconciling for a volume change must not make an
      outstanding sidecar audible.
- [ ] **Play → seek:** a user seek bumps the position revision without clearing `desiredPlaying`; a
      handover that began paused and then received Play finishes playing.
- [ ] **Internal pause:** the transaction's own pause does not change `desiredPlaying`.
- [ ] **Overlapping activations:** a second request cannot snapshot the first's artificial pause, and a
      stale transaction restoring playback cannot let a newer one commit against a moving playhead.
- [ ] **Stale completion after commit** and **play rejection**: both yield a structured
      `current | stale | committed-with-error`, never a silent success.
- [ ] Confirm each fails before implementation.

**These must drive one reducer that production actually uses.** The previous attempt's failure mode was
a collection of individually-correct pure helpers wired together by untested DOM glue — every P1 lived
in the glue, not in any helper. So the sequences above exercise a single state machine that
`review-player.js` delegates to, and a contract test pins that the DOM layer holds no decision logic of
its own.

## Task 3: Single-writer reconciliation, and the durable gate it enables

- [ ] One `reconcileSidecarOutput()` computes and assigns `muted` for every sidecar from the invariant
      above. Nothing else assigns `muted` — grep for it as a contract test.
- [ ] Callbacks (`seeked`, volume, mode change, generation change) only request reconciliation.
- [ ] Eligibility is structural: prepared and stale sets are not reachable from the audible path, so no
      ordering mistake can expose them.
- [ ] Replace `applyReviewAudioOutput`'s global write with the reconcile pass, so a volume change
      during a seek cannot unmute mid-flight.

**The durable gate belongs here, not in Task 1.** Because the single writer is the only place `muted`
changes, it can emit a complete audit record — and a complete record supports a negative, which
sampling never can.

- [ ] Every reconciliation appends `{ element, from, to, mode, eligible, outstandingAlignment, reason }`
      to an in-page audit log, behind a flag so it is off in normal use.
- [ ] The gate asserts over the **audit log**, not samples: no entry may set `muted: false` while that
      element had an outstanding alignment, was not in the current active set, or was ineligible by
      mode. This is exhaustive over writes by construction.
- [ ] Assert **zero timeout-manufactured clearings**: an alignment cleared because a timer fired, rather
      than because the current claim satisfied `!seeking` and position-within-tolerance, is a failure.
- [ ] Keep Task 1's sampler as a **cross-check only** — if it ever records a violation the audit log
      does not explain, the single-writer invariant has been bypassed and that is itself a failure.
- [ ] Loopback comparison still required for emission (Task 6). The audit proves eligibility was never
      wrong; only audio proves nothing escaped.

## Task 4: Transport intent

- [ ] `desiredPlaying: boolean`, updated by user actions only, kept **separate** from the
      position revision. Seeking bumps the revision; it does not touch intent.
- [ ] Consistent semantics across every path that starts or stops playback: library rails
      (`library.js`), rename restoration, Settings open/close, window close, ended playback, and
      handover restoration. Route them through one helper; a contract test pins that no path calls
      `video.play()` directly.

## Task 5: One activation owner

- [ ] Serialize pause/align/commit/restore so two transactions cannot interleave. Preparation may still
      overlap — only the transaction is exclusive.
- [ ] Recheck request ownership after **every** await, and return
      `current | stale | committed-with-error`.
- [ ] The **caller** is the sole place that updates track bookkeeping and success/error status, so a
      stale request cannot overwrite a newer one's state or have its warning immediately overwritten.

## Task 6: Verify

- [ ] The Task 1 gate now passes, including zero timeout-based settlements.
- [ ] `cargo test --workspace` green; fresh-cache warning-denied Clippy. Remember `include_str!`:
      touching a file a test embeds requires touching the test, or the run is meaningless.
- [ ] **Loopback waveform verification** for the audible result. DOM state cannot prove decoded audio
      never escaped. Requires a single-audio-track clip, a save window tight around the playback, the
      clip's own `audio-preview-*.mp4` as the reference, and a correlation gate rejecting the run below
      ~0.85 — the previous attempt aligned 11.7 min into a 22-min source at 0.66 and reported findings
      that were correlation noise.
- [ ] Manual: scrub during playback, switch tracks mid-playback, cancel a track change mid-alignment,
      press Play during a handover, and hide/restore — none may produce silence, a repeat, or stranded
      pause.
- [ ] Update `handoff.md`.

## Out of scope

- The brief echo when switching audio tracks mid-playback, if it survives: the outgoing set is muted in
  the same tick the incoming set is unmuted, and muting does not flush already-queued audio, so two
  *aligned* copies overlap. Accepted by the user. Fix by draining a frame before unmuting — **not** by
  crossfading, which turns misaligned copies into comb filtering.
- The unwired no-overlap smart mode (`service.rs` passes `None` for `exclude_before_s`).
