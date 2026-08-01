# Native playback protocol

This protocol fixes the observable behavior of Clipline's neutral playback worker. Windows Media
Foundation, WASAPI, and Slint/D3D integrations implement the backend traits; fake backends exercise
the same contract on every CI operating system.

## Time and work identity

- Scheduler timeline values use 48,000 ticks per second, matching decoded Opus frames. Conversion
  from indexed media time uses a wide integer intermediate and floors toward the earlier tick.
- Monotonic command/latency timestamps use 100-nanosecond units. Metric histograms use conservative
  one-millisecond buckets; any sample above the fixed 512 ms range makes percentile queries fail
  closed instead of approximating a passing result.
- Public `PlaybackTime` inputs must have a non-zero timescale before state or generation mutation.
- Every asynchronous result carries a `PipelineToken`: the public open/seek `WorkGeneration` plus
  an internal pipeline revision. A seek/open/close changes the public generation. Backend flush,
  device recovery, or decoder recovery changes the revision. Both fields must match before index
  I/O, packet conversion, decoder calls, audio production, or frame publication.
- A stale result is released immediately and cannot change state, clocks, metrics, or events.

## Backend ownership

- A decoded video surface becomes playback-owned before it is returned by the decoder backend.
  Destroying its neutral frame wrapper releases that ownership.
- The scheduler retains at most one presentable frame. Decoder output may be preroll-discarded or
  scheduler-dropped, but no second presentable surface may remain retained.
- The publisher consumes a frame. Acceptance is the publication edge; rejection releases it and is
  an error, not a presentation.
- Audio writes are interleaved stereo at 48 kHz and may not exceed the bounded caller-owned chunk.
  The renderer reports a monotonic rendered-frame position. Device recreation begins a new backend
  epoch and requires an explicit clock rebase before its position is used.

## Playback clock

- While selected audio is active, the timeline is the renderer's monotonic position plus the
  current rebase offset. Pause freezes the timeline. Resume re-anchors the device position to the
  frozen timeline so device activity while paused cannot jump playback.
- Every accepted seek/flush re-anchors the clock at the settled seek target. Audio endpoint recovery
  re-anchors the replacement device at the last published timeline position.
- With no selected audio from the start, or after selected audio reaches its endpoint, the worker
  renders bounded silence and switches to a monotonic tail clock anchored at the current timeline.
  Video therefore continues through leading gaps and end-of-audio tails.
- Clock output never moves backward. A backward backend reading is a recoverable device error and
  requires a revision change and rebase.

## Video scheduling and metrics

An eligible frame is a successfully decoded frame from the current token whose presentation time is
at or after the settled seek target. Decode preroll frames are not eligible and do not enter live
metrics.

- `decoded_eligible_frames` increments when an eligible frame reaches the scheduler.
- In headless and live modes, a frame is **presented** when the publication backend accepts one
  playback-owned surface at the scheduler publication edge. `presented_frames` increments there.
- At that edge, **A/V error** is the absolute difference between the frame PTS and the rebased audio
  or tail clock sampled for that publication. The worker records the latest and maximum absolute
  error in timeline ticks.
- A frame is **late** when publication occurs strictly more than its indexed duration after its PTS.
  `late_frames` increments at most once per eligible frame.
- A **scheduler drop** is an eligible decoded frame released because a newer eligible frame is also
  due at the same scheduler tick. `scheduler_dropped_frames` increments for the released frame.
- `late_or_dropped_frames` is the union of eligible frames that were late at publication or were
  scheduler-dropped. `late_drop_ratio` is that union divided by `decoded_eligible_frames`; it is zero
  when no eligible frame has decoded.
- Seek latency begins when a seek command is accepted (a coalesced replacement keeps its own
  acceptance timestamp) and ends only when the final generation's exact target sample is accepted
  at the publication edge. Superseded seeks record no latency.
- `stale_results`, decoder/device recovery counts, audio mixed/silent/corrupt/dropped frames, and
  queue high-water values are reported separately from the frame ratio.

At each tick the scheduler publishes the newest due frame and releases every older due frame. A
future frame remains pending. Decoder production must back-pressure while that one future frame is
retained.

## State transitions

- Open validates and indexes media before announcing `Opened`. It leaves playback paused unless a
  later transport intent requests play.
- Seek flushes all affected backends, obtains `IndexedMovie::seek_plan`, restarts at its prior sync
  sample, decodes preroll, and discards samples before `video_preroll.samples.end - 1`. That exact
  sample is target-correct even when its PTS precedes a target inside the frame interval.
  `SeekSettled` is emitted only after that sample is accepted for publication.
- Step pauses playback, seeks by the requested signed source-frame count, and publishes exactly the
  settled frame. Subsequent playback requires a new Play command.
- Changing selected audio tracks invalidates premixed audio, resets the participating decoder mix,
  rebases the clock at the current timeline, and preserves transport intent.
- End of video emits `Ended` once and freezes at duration. Audio ending first starts the tail clock;
  it does not end playback.
- Close fences all work, releases the pending frame, clears bounded audio, closes every backend, and
  emits `Closed`. Close during open or seek cannot emit a later `Opened`, `SeekSettled`, or frame.
- A recoverable decoder or device loss increments the pipeline revision, releases pending output,
  recreates only the affected backend, and resumes from an indexed seek at the last committed
  timeline. An unrecoverable failure emits one `Error`, enters `Failed`, and retains no media buffer.

## Cancellation checkpoints

The worker compares the complete pipeline token immediately before and after each of these edges:

1. index/sample read;
2. MP4-to-decoder packet conversion;
3. video or audio decoder call;
4. audio mix and renderer write;
5. scheduler admission;
6. publication.

A newer command may therefore cancel work without waiting for a whole GOP or audio queue to finish.
Rapid alternating seeks are allowed to perform bounded discarded work, but only the final token may
publish or settle.
