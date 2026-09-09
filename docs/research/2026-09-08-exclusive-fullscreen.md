# Windows 10 exclusive fullscreen investigation

Explicit full-monitor Desktop Duplication now records the exclusive blt and flip
mocks through Clipline's AMD H.264 encoder, WASAPI audio and F6 replay pipeline.
This does **not** establish game-only isolation: the source is a monitor and can
include other windows after focus/presentation changes. DWM and PrintWindow
remain separate experiments, with exclusive window capture still failing.

Host: Windows 10 Home 19045, Radeon 780M, AMD driver 32.0.31041.1004, existing
VB-CABLE playback endpoint. No driver/runtime installation, reboot, WGC, injection,
protection bypass or third-party capture implementation was used.

## Reproduced failures and repair

The mock reports `GetFullscreenState=TRUE` and changes the physical display from
1280x720 to 720x480. This establishes a DXGI fullscreen transition and a real mode
change, but does not distinguish all Fullscreen Optimizations presentation paths.
League and Valorant remain unavailable; these are first-party fixtures, not games.

Before the repair, an app F6 replay decoded successfully but all sampled frames
were the same pre-fullscreen desktop image. A fixed 1280x720 region also becomes
invalid when the source shrinks to 720x480. The old geometry branch classified
this as retryable, allowing the cadencer to repeat its last frame indefinitely.

Two bounded production repairs retain the existing capture selection:

- Drop an invalid DXGI duplication interface before opening its replacement.
  Failed reopening leaves an empty slot; subsequent calls retry the same output
  with the existing wall-clock budget/backoff. Rate-limited diagnostics record
  failed reopen HRESULTs. A recovered interface must emit a new seed without
  resetting the monotonic timestamp floor.
- Validate fixed-region geometry before pointer-only filtering. A region that
  no longer fits returns a non-timeout `SourceChanged` error; cadence propagates
  it instead of encoding stale video. It is never clamped or enlarged.

Microsoft requires release-before-recreate after access loss. The original
ordering did nevertheless return success in one instrumented run on this driver;
do not attribute the entire freeze solely to an `E_INVALIDARG` failure, which was
not observed. The repaired run encountered transient `0x80070005`, retried, and
delivered actual 720x480 BGRA frames. The live evidence validates the combined
recovery changes, not each change in isolation.
[AcquireNextFrame](https://learn.microsoft.com/en-us/windows/win32/api/dxgi1_2/nf-dxgi1_2-idxgioutputduplication-acquirenextframe),
[DuplicateOutput](https://learn.microsoft.com/en-us/windows/win32/api/dxgi1_2/nf-dxgi1_2-idxgioutput1-duplicateoutput).

## Passing explicit display controls

Settings were temporarily `capture_mode=primary_monitor`,
`capture_backend=desktop_duplication`, game auto-detection off, 720p60, system
loopback audio, native AMD H.264 MFT. The selected source was checked in a local
trace. Full-monitor frames follow the new dimensions; encoding remains 1280x720.
The existing converter stretches the 720x480 source to that output, so aspect-ratio
preservation across mode changes is not claimed.

| Control | Replay frames | Advancing counters | Repeats | Invalid counter pixels | Pulse video minus audio |
| --- | ---: | ---: | ---: | ---: | ---: |
| Flip exclusive, 40 seconds | 1,800 | 1,798 | 1 | 0 | mean -13.49 ms, range -17.52..4.69 ms |
| Blt exclusive, 65 seconds | 1,800 | 1,796 | 3 | 0 | mean -8.09 ms, range -25.38..-2.30 ms |

Counters were decoded from every replay frame, not inferred from container frame
counts or pixel hashes. Both 30-second replays decode, contain stereo Opus, and
show correct red/blue anchors and moving green content. Live screenshots show no
yellow border. Binary counter progress does not prove synchronized, tear-free
frames. Audio estimates match the fixture's one-second visual/audio pulse edges;
they are not a universal synchronization guarantee.

The blt run remained foreground and reported exclusive in all 574 telemetry rows.
After ten seconds, Clipline private bytes ranged 99,794,944..109,506,560; main-process
CPU advanced 4.8125 seconds over 64.94 seconds. This excludes WebView child CPU and
is a short sustained test, not a long-session performance certification.

A 75-second flip control entered exclusive, switched to windowed at 15 seconds,
back to exclusive at 20, windowed at 35 and exclusive at 40. All 667 telemetry
rows kept the fixture foreground. The full session decodes; sampled images show
fresh game content across all three exclusive intervals and the intervening
windowed desktop. The final F6 replay decodes. Clipline private bytes after
warm-up ranged 100,438,016..112,648,192, with 5.890625 main-process CPU seconds.
`session_1788888236.mp4` and `clip_1788888311.mp4` preserve this control.

Restoring the original fixed 1280x720 region and entering exclusive now stops
capture. The app visibly reports that the selected region no longer fits the
720x480 display and asks the user to select the region again. This is a passing
failure-handling control, not successful fixed-region fullscreen recording.

## Window-only negative controls

PrintWindow flags 0, 1, 2 and 3 all failed exclusive capture on both mocks.
Blt returned a stalled counter and hit the two-second guard. Flip failed anchor
or counter preflight. Recreating the DIB for every call with flag 3 did not help.
Windowed flag-3 positive controls passed on both mocks. Client-only flags now use
the correct client-sized DIB with no nonclient crop; options are recorded beside
each result. See the [native recorder](2026-09-08-print-window-recording.md).

A purported nonactivating overlap control actually stole focus and returned the
mock to windowed mode. It is retained as a failed exclusive-overlap test. Neither
that control nor successful monitor capture proves exclusion of other windows.

## Evidence and limits

Local evidence: `C:\Users\Dain\Desktop\CliplineExclusiveTest-20260908-125432`.
It contains local traces, scripts, telemetry, live screenshots, decoded images,
counter results, audio edge measurements and failed controls. Temporary source
instrumentation is removed from the committed implementation. One attempted
control wrote a UTF-8 BOM that caused settings quarantine and never started
capture; it is excluded. Subsequent settings writes used UTF-8 without BOM.

Media under `C:\Users\Dain\Videos\Clipline\2026-09-08 13-20`:

- `clip_1788888042.mp4` (flip), SHA-256
  `A6652ECF3C685E3E1D34D2544F12BE10C1DC0AA65A1A9E869DC6CCD4BB7371BF`.
- `clip_1788888186.mp4` (blt), SHA-256
  `CBAC253EBF5052E5ABA76CA33947FEB21837F0DBDBD2E621033616A7CC345087`.

At this checkpoint the UI persisted a display dropdown selection as a fixed `display_region`.
That cannot safely be reinterpreted as permission to follow an entire monitor:
full-display intent needs its own persisted representation before seamless mode
tracking can be exposed for a selected display. No game-window source was mapped
to a monitor. Automatic game-only capture remains blocked; real-game validation,
exclusive overlay behavior and long-session testing remain outstanding.

Follow-up: [explicit full-display selection](2026-09-08-full-display-selection.md)
now provides that persisted intent in the UI. Strict game-only exclusive capture
remains unresolved.
