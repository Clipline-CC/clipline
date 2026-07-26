# Memory Follow-up: Replay Peaks, Capture Surfaces, and Background UI

> **For agentic workers:** Execute this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for
> tracking and remain unticked by repository convention.

**Goal:** Build on PR #106 so Save Replay does not duplicate encoded payloads, replay retention
keeps only the exact keyframe-aligned footage a save can use, capture does not pin or queue avoidable
full-resolution textures, and background/large-library UI work cannot silently rebuild the memory
that tray mode just trimmed.

**Order matters.** Exact retention must not ship by changing `BUFFER_HEADROOM_S` alone: the existing
duration planner advances *forward* to a keyframe and can then retain less than the requested save
window. First teach the planner to preserve the actual keyframe at-or-before the save cutoff, then
remove the fixed margin. Likewise, the UI must gain a tested background-generation gate before
taskbar minimize starts requesting WebView2's Low memory target; otherwise native restore has no
reliable way to rehydrate stale asynchronous work.

**Memory invariants:**

- A memory-backed replay save borrows the ring's encoded video/audio allocations.
- A disk-backed replay save opens one segment file at a time and streams samples through the MP4
  writer's existing 64 KiB copy buffer. It never materializes the selected window.
- Prefix audio packets that begin before the selected video origin are omitted without mutating or
  copying the retained segment.
- Duration retention starts at the latest keyframe at-or-before `end - replay_window_s`. Byte
  pressure may move that start forward, because bounded memory wins over coverage during genuine
  encoder overshoot.
- Entering a background state invalidates stale UI requests, releases playback, disconnects poster
  work, and removes heavy gallery DOM. While backgrounded, events update lightweight state and mark
  one coalesced refresh dirty; they do not recreate cards, posters, or video.

## Task 1: Failing replay-save borrowing and disk-streaming tests

- [ ] In `clipline-buffer/src/disk.rs`, add a payload-free `DiskTrackRef` descriptor and failing
      tests proving video/audio offsets describe the single on-disk payload contiguously without a
      load.
- [ ] Add a failing truncation test for `DiskSegment::open_payload`: a shortened segment file must
      return `UnexpectedEof` before muxing starts.
- [ ] In `clipline-capture/src/pipeline.rs`, introduce the test-facing shape of
      `ReplayWindow::{Memory, Disk}` and add a failing assertion that a selected memory segment is
      pointer-identical to the segment retained by `ReplayRing`.
- [ ] Replace the mutating replay-origin audio tests with failing tests for a non-mutating
      `SampleSelection { first_sample, first_byte, pts_start_s }`. Pin the existing rule that a
      packet whose start precedes the video origin is dropped even when it straddles that origin.
- [ ] Add coverage that stale audio prefixes are selected out of every segment without changing
      the original payload or sample metadata.
- [ ] Strengthen RAM/disk save equivalence with two audio tracks and a mid-stream window so prefix
      selection executes; require byte-identical MP4 output and identical end PTS.
- [ ] Add a malformed disk-sample test proving video metadata cannot read through the declared
      video region into the first audio track.
- [ ] Run focused buffer/capture tests and confirm the new tests fail for the intended reasons.

## Task 2: Borrow memory segments and stream disk segments

- [ ] Replace `ReplayStorage::load_window` with a lifetime-bearing
      `ReplayWindow<'a> { Memory(Vec<&'a Segment>), Disk(Vec<&'a DiskSegment>) }`.
- [ ] Export `DiskTrackRef`; add `DiskSegment::video_track`, `audio_tracks`, and `open_payload`.
      Remove the whole-payload `DiskSegment::load` API so future save code cannot regress to
      materializing disk windows.
- [ ] Extract common duration quantization so borrowed `FragSampleRef` and file-backed
      `SourceSample` construction use exactly the same absolute-timeline rounding.
- [ ] Implement non-mutating audio sample selection with checked duration, byte-offset, and
      track-boundary validation.
- [ ] Have `Recorder::save_replay` mux memory samples with
      `write_fragment_multi_borrowed` and disk samples with
      `write_fragment_multi_from_source`. Keep only per-sample metadata plus the writer's 64 KiB
      transfer buffer live.
- [ ] Preserve empty-window, no-overlap, Opus pre-skip, delayed/gapped audio, truncated-cache, and
      partial-output cleanup behavior.
- [ ] Run `cargo test -p clipline-buffer -p clipline-capture`.
- [ ] Commit as one logical `perf(capture): stream replay saves without payload copies` change.

## Task 3: Failing exact keyframe-retention tests

- [ ] In `planning.rs`, add a continuation-segment case where forward duration alignment would lose
      requested footage. Require the keyframe at-or-before the cutoff to survive.
- [ ] Add an exact-boundary case with one-second, keyframe-led segments: a three-second window
      ending at seven seconds starts at four, not three.
- [ ] Add a byte-pressure case where the incoming keyframe is the next valid alignment point and all
      existing headless continuations must be evicted.
- [ ] Preserve tests proving no-pressure pushes do not realign gratuitously, byte pressure wins
      during overshoot, and the incoming segment itself is never discarded.
- [ ] In both memory and disk rings, add non-aligned GOP tests proving the retained/saved window
      starts at the latest covering keyframe and discarded disk files are unlinked.
- [ ] Tighten steady-state expectations from `window + 15 s + crossing segment` to the minimal
      keyframe-aligned coverage.
- [ ] Run focused buffer tests and confirm the new expectations fail on the old planner.

## Task 4: Exact retention and compatibility-field cleanup

- [ ] Change `eviction_plan` to receive the incoming segment, compute duration pressure from the
      latest keyframe at-or-before `incoming.end - retention_s` across logical
      `existing + incoming`, then apply byte-pressure forward alignment only when bytes demand it.
- [ ] Remove `BUFFER_HEADROOM_S`. Derive runtime retention and the byte budget from
      `replay_window_s`.
- [ ] Keep persisted `AppSettings::buffer_seconds` only as a compatibility mirror normalized to
      `replay_window_s`. Do not validate it independently: the UI permits a five-second replay
      while the legacy field's minimum is ten seconds.
- [ ] Remove redundant runtime `ServiceOptions::buffer_seconds` if no caller still needs it.
- [ ] Update the hidden `#set-buffer` field and `settings.js` serialization to mirror the replay
      value instead of adding fifteen seconds.
- [ ] Add settings regressions for a five-second replay, stale legacy buffer values, load/save
      normalization, and the new default.
- [ ] Update `ddoc.md` to describe exact covering-keyframe retention and the qualified byte-cap
      override.
- [ ] Run buffer, app settings, and UI-contract tests.
- [ ] Commit as one logical `perf(buffer): retain only the keyframe-aligned replay window` change.

## Task 5: Seed ownership and sealed-GOP allocation tests

- [ ] Add a platform-neutral `CadencedCapture` test that records the CPU seed `Vec` pointer and
      proves construction moves that allocation into `last_data` rather than cloning it.
- [ ] Add a recorder test with irregular packet sizes proving sealed video/audio payload and sample
      vectors retain no geometric growth slack.
- [ ] Change `CadencedCapture::new` to consume `Frame`; move its payload into `last_data`.
- [ ] Pre-size sealed video from `pending_bytes`, pre-size each selected audio track from its exact
      payload/sample count, and consume packets as they are copied.
- [ ] Run `cargo test -p clipline-capture -p clipline-app`.
- [ ] Commit as `perf(capture): release seed texture and size GOP buffers exactly`.

## Task 6: Latest-only WGC queue

- [ ] Add a Windows unit test using the production queue capacity: after two sends without a
      receive, only the newest frame may remain.
- [ ] Change the application-owned WGC queue capacity from two to one. Leave the WinRT frame-pool
      count at two.
- [ ] Keep the generic capacity-two/drop-oldest channel test.
- [ ] Run capture tests and a live Windows WGC idle/busy capture soak, checking cadence, stop/save
      responsiveness, and dropped-frame diagnostics.
- [ ] Commit separately as `perf(capture): keep only the latest queued WGC frame`.

## Task 7: Failing background-lifecycle tests

- [ ] Add a DOM-free `window-lifecycle-core.js` with Boa tests for revisioned native snapshots,
      enter/leave generations, dirty-refresh coalescing, and rejection of async work captured
      before a background transition. Start pessimistically unknown/backgrounded so autostart
      cannot perform heavy work before native state arrives.
- [ ] Add a managed Rust `Foreground | Tray | Taskbar` lifecycle state with a monotonically
      increasing revision. Have `frontend_ready` return the current snapshot with its warnings so
      cold autostart cannot miss an event emitted before frontend listeners exist.
- [ ] Add UI contracts requiring background entry to invalidate local/cloud request gates, clear
      both gallery roots, disconnect poster observation, release review media, and avoid calling
      `renderClips`.
- [ ] Add contracts proving saved/enrichment/upload events mark one pending refresh while
      backgrounded and a foreground event performs exactly one refresh.
- [ ] Add a cloud-media regression: a download completing after its captured lifecycle generation
      is stale must not call `openClip`.
- [ ] Add app lifecycle tests for exact tray hide/reveal and taskbar minimize/restore ordering.
      Failed native minimize/hide must publish no background snapshot and must not tear down a
      still-visible UI. Older native snapshots must not override a newer revision.
- [ ] Run lifecycle-core, UI-contract, and focused app tests; confirm failures.

## Task 8: Durable tray, autostart, and taskbar background state

- [ ] Change tray ordering to native hide, publish the revisioned background snapshot, hide the
      controller, then request Low. Invalidate request gates, release media/mic Web Audio, clear
      heavyweight gallery/poster DOM, and defer refreshes only after native hide succeeds.
- [ ] Change reveal ordering to request Normal, show the controller, show/unminimize/focus the
      native window, then publish the foreground snapshot.
- [ ] Generation-guard cloud media opens and any other asynchronous completion capable of creating
      a media source after background entry.
- [ ] Extend taskbar minimize with recorded native state: minimize, publish background, hide the
      controller, request Low. On `Focused(true)`, restore only from `Taskbar` in the order Normal,
      controller show, foreground publish; repeated focus and Tray/Foreground focus are no-ops.
- [ ] Add a native-confirmed visibility-change fallback for Win+D/direct taskbar-icon minimize.
      Never interpret ordinary focus loss or Alt-Tab as a background transition.
- [ ] Keep tray controller hide/show ordering unchanged and preserve current clip/time semantics for
      ordinary taskbar minimize if product behavior requires restoration.
- [ ] Add a harness scenario: hide, save repeatedly by global hotkey, complete an in-flight cloud
      operation, then verify no hidden gallery/video reinflation before reveal.
- [ ] Commit as `perf(app): keep background webview memory trimmed`.

## Task 9: Bound gallery posters and extraction work

- [ ] Add pure pagination/window calculations with Boa tests covering local/cloud lists, filters,
      grouping boundaries, empty pages, and page reset after data/filter changes.
- [ ] Render a bounded page/window of cards rather than the entire library. Keep selection keyed by
      path so paging does not lose bulk-selection state.
- [ ] Observe cached and uncached posters uniformly; remove off-window image nodes/sources instead of
      permanently attaching every decoded bitmap after first intersection.
- [ ] Add a backend per-canonical-path single-flight plus a global one-or-two-process semaphore for
      local poster extraction. Cache the resolved FFmpeg path and apply the existing timeout,
      kill, and reap discipline.
- [ ] Bound `posterCache` URL/unavailable entries and delete keys for deleted, renamed, account-, and
      media-root-invalidated clips.
- [ ] Add large-library tests/harness fixtures at 50, 500, and 2,000 clips; assert bounded card/image
      counts and bounded concurrent FFmpeg children while scrolling every page.
- [ ] Commit UI and backend poster bounds in separate logical commits if either can stand alone.

## Task 10: Bound upload and long-session conditional growth

- [ ] Add upload tests proving multipart request bodies stream a bounded file slice rather than
      allocating the server's entire part, and that the global upload semaphore caps concurrent
      parts.
- [ ] Replace `read_chunk_for_part`'s up-to-64 MiB `Vec` with a replayable bounded file reader/body
      per retry; keep checksum, content length, part ordering, and retry behavior unchanged.
- [ ] Add MP4 writer tests for online duration-run aggregation and sync-sample index storage. Verify
      final `stts`/`stss` boxes remain byte-equivalent to the current tables.
- [ ] Store duration runs incrementally and sync sample numbers rather than one duration and bool per
      sample. If size/chunk tables still dominate multi-hour runs, add a separately tested spool
      threshold instead of an unbounded in-memory vector.
- [ ] Add a replay-only marker-log retention test and prune events older than the replay window when
      no full-session sink needs them.
- [ ] Commit upload and full-session work separately.

## Task 11: Verification, measurements, and handoff

- [ ] Run `cargo test --workspace`.
- [ ] Run fresh-cache Clippy for every changed crate, then
      `cargo clippy --workspace --all-targets -- -D warnings`.
- [ ] Extend memory measurement to sample root/child private working set and private commit at
      50-100 ms during repeated Save Replay. Record GPU local/non-local usage separately so D3D
      surfaces are not mistaken for ordinary private heap.
- [ ] Measure minimum/default/maximum replay configurations in memory and disk modes; record
      steady-state ring size and peak delta during save.
- [ ] Measure visible, taskbar-minimized, tray-hidden, hidden-after-save, and reveal states with both
      a small and large library.
- [ ] Update `handoff.md` with measured outcomes, remaining trade-offs, and exact manual-test steps.
- [ ] Stop any existing `clipline-app.exe`, run `cargo run -p clipline-app`, and leave the app open
      for user acceptance.
