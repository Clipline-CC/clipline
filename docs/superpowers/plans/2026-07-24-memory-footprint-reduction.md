# Memory Footprint Reduction

> **For agentic workers:** Execute this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for
> tracking and remain unticked by repository convention.

**Goal:** The replay ring retains only the footage a Save Replay can use, the in-app RAM meter
attributes memory to the process that owns it, and the capture readback path stops allocating a
frame-sized buffer every frame.

**Save-semantics guarantee (qualified):** once the ring has accumulated at least `replay_window_s`,
and while encoded output stays within the byte cap's overshoot envelope, a Save Replay still covers
the full window and still starts on a keyframe. Before fill, coverage is necessarily whatever has
accumulated. Outside the envelope the byte bound wins — it has to, or the process grows without
limit — so a sufficiently large overshoot can still discard footage the window wanted. Task 1 pins
which bound wins in that case rather than pretending the conflict cannot arise.

**Observed:** with capture live on a debug build, `clipline-app.exe` held 296.8 MB private and its
descendants a further ~250 MB, for the 547.4 MB the in-app meter reports. A region walk of the Rust
process showed 279 MB PRIVATE / 205 MB IMAGE / 25 MB MAPPED.

**Root cause of the ring share:** `estimated_buffer_bytes` (`settings/mod.rs:230`) sizes the byte
cap as `2.0 × bitrate × (replay_window_s + 15 s)` — `ENCODER_OVERSHOOT_HEADROOM` exists so a
bitrate overshoot cannot evict footage the window still needs. But eviction is byte-only
(`ring.rs:25` → `planning::eviction_count`, `planning.rs:90`) and nothing else drops a segment, so
the ring never settles at `buffer_seconds`; it grows to the whole 2× cap and stays there. The
headroom stops being a cap and becomes a target. Measured against the real `ReplayRing`:

| Config | Retained in RAM | Ever saveable | Wasted |
|---|---|---|---|
| 30 s window, 720p Sharp (8 Mbps) | 85.8 MB / 90 s | 28.6 MB / 30 s | 57.2 MB |
| Defaults: 60 s, Balanced/Source (12 Mbps) | 214.6 MB / 150 s | 85.8 MB / 60 s | 128.7 MB |
| 120 s window, Maximum/Source (40 Mbps) | 1287.5 MB / 270 s | 572.2 MB / 120 s | 715.3 MB |
| 30 s window, encoder at 40% of target | 85.8 MB / **225 s** | 11.4 MB / 30 s | 74.4 MB |

The last row is the sharp edge: fewer bytes per second means more seconds fit under the cap, so
low-motion content — already known to make this encoder undershoot — retains ~5× the intended span.
A duration bound makes retention independent of how well the encoder tracks its target.

## Task 1: Failing tests for one combined eviction planner

Byte and duration eviction must be planned **together**. Run as two independent passes, either can
leave the front mid-GOP, because the byte planner removes the minimum count with no keyframe
awareness (`planning.rs:90`).

- [ ] Add `planning.rs` tests for a single `eviction_plan`: byte pressure alone matches today's
      counts; duration pressure alone drops whole GOPs; when both apply the **larger** count wins;
      the count then advances forward to the next keyframe-starting segment so the front is never a
      headless GOP continuation.
- [ ] Test that the plan may evict every *existing* segment but never the *incoming* one — the
      incoming segment is not yet in the deque, so this preserves today's
      `never_evicts_the_only_segment` behavior.
- [ ] Test the conflict case the guarantee above admits: under overshoot large enough that the byte
      count exceeds the duration count, the byte bound wins and retention drops below
      `buffer_seconds`. Pin that direction deliberately.
- [ ] Add `ring.rs` tests: at target bitrate the ring settles at `buffer_seconds`, not the 2× byte
      cap; at 40% of target it *still* settles at `buffer_seconds`; `bytes()` stays consistent with
      retained segments; after long steady-state pushing `save_window(replay_window_s, None)` covers
      the full window and starts on a keyframe.
- [ ] Add the same coverage in `disk.rs`, plus: evicted segment files are unlinked, and a failed
      deletion leaves the ring consistent.
- [ ] Run the focused tests and confirm they fail on current code.

## Task 2: One keyframe-aligned eviction planner for both rings

- [ ] Replace `planning::eviction_count` with `planning::eviction_plan`, generic over the existing
      `pub(crate) ReplayWindowSegment` trait (`planning.rs:6`) — it already exposes `pts_end_s` and
      `starts_with_keyframe` for both `Segment` and `DiskSegment`, so one planner serves memory and
      disk.
- [ ] Compute against *logical existing + incoming*, since the incoming segment is not in the deque
      yet: byte count from `current_bytes + incoming_bytes` vs `max_bytes` (today's logic); duration
      count from front segments where `incoming.pts_end_s() - seg.pts_end_s() > retention_s`. Take
      the maximum, then advance that single count forward while the new front does not start a
      keyframe.
- [ ] If no remaining segment starts a keyframe, do not evict past the computed count — leave
      `save_window`'s existing first-keyframe fallback (`planning.rs:73`) to handle it.
- [ ] Carry `retention_s` on `ReplayRing` and `DiskReplayRing`. Byte eviction stays as the backstop
      for genuine overshoot — that is what the 2× headroom was for.
- [ ] Leave `estimated_buffer_bytes` and `MIN_BUFFER_BYTES` unchanged. Once retention is
      time-bounded the byte cap is only ever a ceiling, so the 64 MB floor stops being a floor on
      steady-state RAM.
- [ ] In `DiskReplayRing::push`, preserve the current transaction ordering: the incoming segment
      stays separate (written, renamed, `OwnedFile` still armed) until every fallible `fs::remove_file`
      has succeeded, so a mid-eviction failure discards the new segment and leaves the ring
      consistent rather than half-updated (`disk.rs:95`).

## Task 3: Plumb retention, derived rather than read

`AppSettings::buffer_seconds` is **not** safe to read at runtime. `save_to` normalizes only a clone
(`persistence.rs:216`), leaving the live object un-normalized, and `prepare_settings_restart`
(`app.rs:903`) feeds the request straight into `options_for` → `to_service_options`. Validation only
enforces `replay_window_s <= buffer_seconds` (`validation.rs:73`), so `buffer_seconds ==
replay_window_s` passes and yields **zero** headroom.

- [ ] In `to_service_options` (`settings/mod.rs:193`) derive retention with
      `replay_buffer_seconds(self)` (`settings/mod.rs:226`) instead of reading the field. One
      definition, no ordering dependency on persistence.
- [ ] Add a settings test with a deliberately stale `buffer_seconds` (equal to `replay_window_s`,
      which passes validation) asserting the derived retention still carries the full
      `BUFFER_HEADROOM_S`.
- [ ] Add `retention_s` to both `ReplayStorageConfig` variants (`pipeline.rs:34`) and thread it into
      the ring constructors in `Recorder::new_with_replay_storage` (`pipeline.rs:228`).
- [ ] `Recorder::new` (`pipeline.rs:210`) takes `f64::INFINITY` — resolved now, not at
      implementation time: seconds cannot be derived from a byte budget. Add an explicitly bounded
      constructor for callers that want retention, and leave `new` byte-only so existing callers and
      tests keep their current behavior.
- [ ] Add `buffer_seconds` to `ServiceOptions` alongside `replay_window_s` / `buffer_bytes` and
      source `service.rs:880` from it. Do not recompute the headroom service-side.
- [ ] Confirm `ReplayStorageOptions::Disk` gets the same retention — the disk ring has the identical
      byte-only bug, where the cost is cache disk rather than RAM.

## Task 4: Release WebView2 rendering resources while hidden

**Measured:** with the window hidden for four minutes, child private working set moved less than
1 MB (230.0 → 230.4) and the GPU process did not budge from 132.0 MB. Windows is not reclaiming it
either, so ~230 MB of resident memory stays live whenever Clipline sits in the tray — more than the
entire Rust process. An earlier measurement of *committed* private could not distinguish "nothing
released" from "paged out"; private working set can, and it says nothing is released.

**Cause:** `send_main_window_to_tray` (`app.rs:1524`) calls `WebviewWindow::hide()`, which is
`self.window.hide()` (`tauri/src/webview/webview_window.rs:2200`) — the **native window only**. The
WebView2 controller is never told it is off-screen.

**No COM and no new unsafe.** `Webview::hide()`/`show()` (`tauri/src/webview/mod.rs:1541`) dispatch
to wry's `set_visible`, which does `ShowWindow` **and** `controller.SetIsVisible(visible)`
(`wry-0.55.1/src/webview2/mod.rs:1487`). `WebviewWindow` implements `AsRef<Webview>`
(`webview_window.rs:1446`), so `window.as_ref().hide()` reaches it safely. (The Rust method is
`SetIsVisible`; `put_IsVisible` is the C++ name.)

- [ ] Extend the existing closure-based reveal test (`unresponsive_main_window_reveals_existing_handle`,
      `app.rs:4249`) to pin reveal order: **webview show → native show → unminimize → focus**, so the
      native window never appears before the webview can paint.
- [ ] Add hide-order coverage: **native hide → webview hide**, so a native-hide failure can never
      leave a visible but blank window. The existing settings tests already cover close/minimize
      routing — do not add another artificial decision layer.
- [ ] Treat webview visibility as best-effort: log failures, never let one strand Clipline in the
      tray or leave the window unrevealable.
- [ ] **Cover the autostart path — the most important hidden session and the one this task exists
      for.** The window is created hidden (`tauri.conf.json:20` `"visible": false`) and `app.rs:2440`
      skips `open_main_window` entirely under `--autostart`, while wry initialises the controller
      from `attributes.visible` (`webview2/mod.rs:544`). A cold autostart therefore renders
      indefinitely. Hide the webview explicitly during autostart setup.
- [ ] Verify the first reveal after an autostart-hidden start: initial frontend work is gated behind
      two animation frames in `main.js`, which may be deferred while invisible.
- [ ] Cover all four transitions: tray hide, autostart-hidden init, existing-window reveal, and
      rebuilt-window reveal.

### Acceptance — external harness, not the in-app meter

The meter cannot measure this: `main.js:727` only polls while `!document.hidden` and refreshes on
`visibilitychange`, so by the time it samples, the controller is visible again.

- [ ] **Primary gate: aggregate private working set across all `msedgewebview2.exe` processes drops
      by ≥ 40 MiB while hidden.** Not GPU-only — Microsoft documents qualitative memory benefits and
      cache purging, not a GPU-specific reduction.
- [ ] Record the browser/GPU/renderer/utility breakdown as diagnostics only.
- [ ] Secondary check: total process-tree private working set **and** private commit, to prove memory
      was released rather than shifted.
- [ ] Protocol: stable visible baseline, hide 90–120 s, compare the final 30 s median, repeated
      across ≥ 3 clean release-build launches. Re-enumerate helper processes each sample — WebView2
      processes can restart and change PID.
- [ ] Behavioural acceptance, since `IsVisible(false)` is not suspension but does throttle animations
      and some tasks and purge caches: hide > 5 minutes; save several clips via the global hotkey;
      reopen and confirm recorder status, library contents, and notices are current (status is
      event-driven at `main.js`, not polled); hide while review playback is active and confirm clean
      unload/restore; repeat hide/show cycles with no white frame, stale frame, delayed input, or
      memory rebound.
- [ ] The readiness watchdog is **not** sufficient evidence — it checks the native host and latches
      frontend readiness; it does not prove the controller became visible or repainted.
- [ ] **Revert the commit** if the 40 MiB gate is missed or any restore behaviour regresses.

## Task 5: Attribute memory honestly in the meter

The descendant total **cannot** be labeled "WebView2". `current_process_tree_memory` (`memory.rs:80`)
sums every descendant except `conhost.exe`, and `ffmpeg_encoder.rs:174` spawns a long-lived
`ffmpeg.exe` child. The earlier measurement saw only WebView2 children because that build had no
bundled ffmpeg and ran a hardware encoder.

- [ ] Write the Rust tests first: root-only figure, descendant total, and the summed field kept for
      backward compatibility. Extend the existing `child_process_ids_from_entries` fixtures
      (`memory.rs:496`) rather than adding a second tree walk.
- [ ] Extend `MemoryStatus` (`memory.rs:22`) with the root process figure and the descendant total
      as separate fields, keeping the existing summed field for one release.
- [ ] Split the accumulation inside the existing walk in `current_process_tree_memory` so one
      snapshot backs every number and the 1 s cache TTL is unchanged.
- [ ] Label the secondary line **"Child processes"**, or classify `msedgewebview2.exe` separately
      from other children and label each. Do not label the whole descendant total as the webview.
- [ ] Update `ui/app-core.js:456`, then the three `ui_contract.rs` assertions that pin the current
      single meter and must move together: `:750`, `:2992` (pins the exact `Using -- RAM`
      placeholder in `ui/index.html:33`), and `:3017` (pins the `.memory-usage` tabular-nums rule).

## Task 6: Release baseline, with unambiguous encoder identification

- [ ] `encoder_label` (`service.rs:394`) formats only `backend · codec` and discards `EncoderApi`,
      so an MFT and an FFmpeg path both render e.g. "AMD AMF · H.264". Before relying on the
      baseline, log the `EncoderApi` (`service.rs:1391`/`:1406` already branch on it) or determine
      the path by checking for a live `ffmpeg.exe` child.
- [ ] Build release and record, with capture live at default settings: process-tree breakdown,
      committed PRIVATE/IMAGE/MAPPED split, steady-state `ring_bytes()` / `buffered_span_s()`, and
      the resolved `EncoderApi`. The 205 MB IMAGE figure above is debug info and will not survive
      release; the Task 1–3 win is arithmetic on retained bytes and holds regardless of profile.
- [ ] Confirm the ring fix landed: retained span ≈ `buffer_seconds`, retained bytes roughly halved
      at default settings.
- [ ] If the resolved path is MFT, **skip Task 7 and continue to verification** — frames stay on the
      GPU and there is no readback to optimize. Do not abandon the plan. Alternatively force an
      FFmpeg path deliberately to measure Task 7 on the path it affects.

### Task 6 outcome — release baseline, and Task 7 gated out

`encoder_label` was the ambiguity it was predicted to be: the label reads `AMD AMF · H.264` on this
machine while the resolved API is **`Mft`**. Added an `encoder_selected` diagnostic logging api /
backend / codec, cross-checked by the absence of an `ffmpeg.exe` child.

Release baseline, capture live at the machine's settings (30 s window, 720p Sharp, MFT/AMF/H.264),
one minute uptime:

| | Release | Debug (earlier) |
|---|---|---|
| App private | 201.9 MB committed / 151.0 MB working set | 296.8 MB committed |
| Committed regions | PRIVATE 192.3 / IMAGE 191.3 / MAPPED 25.6 MB | PRIVATE 279 / IMAGE 205 / MAPPED 25 |

Correction to this plan's earlier claim that the IMAGE figure "will not survive release": it largely
did — 191.3 MB vs 205 MB. The release profile keeps debuginfo, and IMAGE also counts mapped system
and WebView2 DLLs. It is file-backed and shared, so it does not inflate private bytes, but the
stated reason was wrong.

**Task 7 is skipped, per this task's own instruction:** the path is MFT, so frames stay on the GPU
and there is no per-frame CPU readback to optimize. `nv12.rs`'s allocation churn is real but
unreachable on this configuration — revisit only if the FFmpeg encoder path becomes a default.

## Task 7: Stop allocating per frame on the readback path (SKIPPED — MFT path)

`read_nv12` (`nv12.rs:298`) and `read_bgra` (`nv12.rs:433`) are stateless free functions returning
owned buffers, so there is nowhere to hang reusable state — the API has to change. The CPU fallback
then allocates a *second* per-frame NV12 vector in `CpuVideoConverter::convert`
(`cpu_video.rs:108`), which the previous revision of this plan missed.

- [ ] Write failing tests first: `*_into` variants produce byte-identical output to today's
      allocating functions, and reuse across differing frame sizes is correct (grow, and do not
      leak stale bytes from a larger previous frame into a smaller one).
- [ ] Add `read_nv12_into` / `read_bgra_into` taking `&mut Vec<u8>`, and a `convert_into` on
      `CpuVideoConverter`. Keep the owned-return wrappers so existing callers and tests are
      unaffected.
- [ ] Hold the reusable scratch buffer and the dimension-keyed staging texture on the
      encoder/readback object that owns the frame loop, not in the free functions. Rebuild the
      staging texture only when capture dimensions change — the video-processor rebuild-on-resize
      comment (`nv12.rs:80`) already establishes that pattern.
- [ ] Evaluate the per-conversion NV12 output texture (`nv12.rs:152`). Ownership crosses into the
      encoder, so pool it only if that can be done without holding a frame the encoder still
      references; if it cannot, say so in the commit body and leave it.
- [ ] **Acceptance metric is allocation rate and frame time, not working-set size.** Retaining
      scratch capacity can leave RSS flat or slightly higher while still removing the churn. Revert
      rather than keep a change that does not measurably improve allocation rate or frame time.

## Task 8: Documentation and verification

- [ ] Update the `ReplayRing` rustdoc (`ring.rs:6`): it currently describes byte-budgeted eviction
      and asserts dropping from the front "never strands a partial GOP" — the keyframe advance in
      Task 2 is what actually makes that true. Document the dual bound and which one wins under
      overshoot.
- [ ] Update `ddoc.md` §6 for the dual bound, since it is the architecture source of truth.
- [ ] Review and leave documented, not changed: `MAX_PENDING_GOP_BYTES` 64 MB (`pipeline.rs:15`),
      `MAX_FRAMER_BUFFER` 32 MB (`framing.rs:23`), `FULL_SESSION_QUEUE_MAX_BYTES` 128 MB
      (`pipeline.rs:19`). These are per-run ceilings that do not preallocate, and
      `MAX_PENDING_GOP_DURATION_S` already bounds the unsealed GOP by time. Note in `handoff.md`
      that the full-session queue stacks on top of the ring when that mode is active.
- [ ] `cargo test --workspace` green; `cargo clean -p clipline-buffer -p clipline-capture` then
      warning-denied Clippy on those crates, then the workspace.
- [ ] Manual smoke: default settings, capture live >5 minutes — the RAM meter settles instead of
      climbing to the byte cap; then Save Replay and confirm the clip is the full window, plays, and
      starts clean.
- [ ] Manual smoke: repeat on a static desktop (the encoder-undershoot case) and confirm retention
      settles at `buffer_seconds` rather than stretching to ~5×.
- [ ] Update `handoff.md`. Commit as separate conventional changes: eviction planner (Tasks 1–2),
      settings/pipeline plumbing (Task 3), webview visibility (Task 4), meter attribution (Task 5),
      readback reuse (Task 7), docs (Task 8).

### Task 4 outcome — gate missed, kept deliberately

Three clean release launches, library open and a clip decoding, hidden 120 s, final-30 s median.
Helper enumeration validates descendant **and** creation time — an earlier harness without the
creation-time check reported an impossible 3,886 MB tree from PID reuse against the ~19 unrelated
`msedgewebview2` processes on this machine.

| Run | WebView2 base → hidden | Δ | Gate | GPU | Renderer |
|---|---|---|---|---|---|
| 1 | 232.1 → 217.4 | 14.7 | ✗ | −3.4 | −12.7 |
| 2 | 269.2 → 226.2 | 42.9 | ✓ | −27.2 | −13.2 |
| 3 | 243.9 → 223.1 | 20.7 | ✗ | −5.5 | −15.2 |

**1 of 3 passed; median 20.7 MiB against the 40 MiB gate.** The renderer sheds 12.7–15.2 MiB
reliably; GPU reclamation is erratic (−3.4 to −27.2) and drives the whole spread. The 40 MiB gate
was effectively a bet on the GPU process, which `SetIsVisible` does not reliably reclaim —
it is documented to stop *rendering*, and reclamation was inferred rather than promised.

Kept anyway: not rendering an invisible window is correct independent of the number, and the
autostart path was rendering indefinitely. Recorded here as missing its stated gate rather than
retro-fitting the threshold.

## Task 4b: MemoryUsageTargetLevel.Low while hidden

The documented API for shrinking an *inactive* WebView, as opposed to merely not drawing it.
Scripts and network connections keep running, so the tray-hidden window still handles events —
unlike `TrySuspend`, which pauses scripts and would need a state re-sync on `Resume`. Microsoft
advises using `Low`/`Normal` **or** `Suspend`/`Resume`, never mixing them: stay in `Low`/`Normal`.

- [ ] Add `webview2-com` to the `cfg(windows)` dependencies, pinned to the version Tauri 2.11.2
      already resolves (0.38) so the `ICoreWebView2Controller` types match and no second COM
      binding enters the tree.
- [ ] Confine the `unsafe` to a safe wrapper under `src/windows/` per platform discipline:
      `controller.CoreWebView2()` → cast to `ICoreWebView2_19` (`webview2-com-sys-0.38.2`
      declares `SetMemoryUsageTargetLevel` there) → set
      `COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_LOW`.
- [ ] Reach the controller with `WebviewWindow::with_webview` (`tauri/src/webview/mod.rs:1668`);
      `PlatformWebview::controller()` (`:180`) returns the typed interface. The closure runs on the
      webview thread and is `FnOnce + Send + 'static`, so treat it as fire-and-forget best-effort
      exactly like Task 4's visibility calls.
- [ ] Set `Low` alongside the existing hide (tray **and** autostart), `Normal` on reveal. A missing
      `ICoreWebView2_19` on an older runtime must degrade silently — the cast failing is not an
      error worth surfacing, and Win10 users on old WebView2 already caused one shipped incident.
- [ ] Same acceptance harness and the **same 40 MiB gate**, measured on top of Task 4 — report the
      incremental delta, not the combined figure, so the two changes stay separable.
- [ ] Behavioural checks unchanged from Task 4, plus: confirm the hotkey still saves while hidden in
      `Low` (scripts continue, but verify rather than assume), and that reveal from `Low` repaints
      without a stale frame.
- [ ] Revert if the incremental gate is missed. Do **not** escalate to `TrySuspend` in this task —
      that is a separate behavioural change needing its own re-sync design.

### Task 4b outcome — gate cleared by ~4.5×

Same harness and gate as Task 4, measured on top of it. Three clean release launches, library open
and a clip decoding, hidden 120 s, final-30 s median:

| Run | WebView2 base → hidden | Δ | Gate | GPU | Tree total |
|---|---|---|---|---|---|
| 1 | 234.2 → 45.8 | 188.3 | ✓ | 116.1 → 5.4 | 339.1 → 153.8 |
| 2 | 229.1 → 29.9 | 199.2 | ✓ | 124.4 → 5.7 | 333.7 → 155.8 |
| 3 | 223.7 → 45.9 | 177.9 | ✓ | 125.6 → 33.8 | 335.3 → 184.4 |

Median 188.3 MiB against the 40 MiB gate. Idle in the tray, the tree's resident set drops from
~335 MB to ~155 MB.

Three corrections to how this should be stated:

- **"Trimmed from the resident set", not "released" or "reclaimed".** The harness sampled private
  working set only; this plan's secondary private-commit check was **not** performed. So ~188 MiB
  demonstrably left the resident set, but whether it was decommitted or merely paged out is
  unmeasured. `scripts/measure-hidden-webview-memory.ps1` now records commit alongside working set
  so a re-run can settle it.
- **This is the combined figure, not `Low`'s increment.** It measures playback suspension +
  `SetIsVisible` + `Low` together, where this task asked for the incremental delta. Subtracting the
  visibility-only median (20.7 MiB) puts `Low`'s own contribution near **168 MiB** — still an
  overwhelming pass, but the honest number.
- **Two confirmed runs plus one corroborating run**, not three clean ones. Run 3's playback probe
  returned empty and it ended at 33.8 MiB GPU rather than ~5 MiB, so it likely measured with the
  grid rendered but no active decode. The harness now **fails** rather than proceeding when
  click/playback confirmation is missing.

## Investigated and deliberately not changed

- **`ENRICHMENT_PASSES` (`osu_api.rs:23`) is not a leak.** An earlier revision of this plan called
  it an unbounded set growing one entry per enriched clip. That was wrong: it is a per-root
  single-flight registry. `EnrichmentPassLease::try_acquire` inserts, `Drop` removes
  (`osu_api.rs:45`), and `enrichment_pass_lease_coalesces_per_root_and_releases_on_drop`
  (`osu_api.rs:1005`) already proves release and reacquisition. It holds at most one entry per
  concurrently active pass. Bounding it would damage the single-flight behavior it exists to
  provide. No change.
