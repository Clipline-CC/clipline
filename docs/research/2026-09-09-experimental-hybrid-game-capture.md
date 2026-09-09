# Experimental PrintWindow and fullscreen display capture

The user accepted the source tradeoff: isolated experimental window capture when
available, whole-display capture for fullscreen. The working windowed/borderless
experiment was **PrintWindow**, not direct DWM shared-surface capture. DWM's
accelerated flip-model failures remain unresolved and that probe stays separate.

## Selection and limits

Settings exposes `experimental_hybrid` as **Experimental game capture (no border)**.
It requires a window source, normally supplied by automatic detection of a selected
game. Existing Auto/WGC and explicit Desktop Duplication choices are unchanged.
The latter still rejects window sources and never falls back to WGC.

The experimental switch uses the documented Windows
[notification state](https://learn.microsoft.com/en-us/windows/win32/api/shellapi/ne-shellapi-query_user_notification_state).
`QUNS_RUNNING_D3D_FULL_SCREEN` is a **global heuristic**, not proof of the selected
process's swap-chain state or native Legacy Flip. Display acquisition additionally
requires exactly one monitor, the exact target HWND in the foreground, an alive
held process instance, no capture affinity, a visible/nonminimized window, and
physical client bounds exactly matching that monitor. These checks repeat after
acquisition. Geometry alone and unchanged pixel hashes never select display capture.

Unknown/unavailable states, failed fullscreen guards, and source transitions emit
black video. Audio continues. Entry is debounced for 150 ms; guard loss takes effect
immediately on the next capture call. Window capture may continue in the background
when Windows no longer reports fullscreen. Process/window identity changes and
protection failures stop recording. A closed automatically detected target emits
black until game detection finalizes the session and waits for the next game.

Display pixels can include overlays. Before/after queries are not atomic with
presentation and cannot guarantee zero desktop exposure during every race. Multiple
monitors currently disable fullscreen switching; uncertain fullscreen emits black.
Actual games, other drivers, mixed DPI, HDR, rotated outputs and complete-frame
synchronization remain unvalidated. Static menus cannot reliably distinguish a
legitimate unchanged scene from a successful but stale PrintWindow response.

## Signal control on this machine

Windows 10 Home 19045, Radeon 780M, installed AMD driver 32.0.31041.1004, one
1280x720 PiKVM display. A copied D3D11 flip mock was observed four times per stage.
The per-executable FSO compatibility change was restored after testing. No UAC,
driver installation, global display-policy change, WGC session or injection occurred.

| Mock mode | Shell state | Global exclusive / target VidPN, FSO on | FSO disabled |
| --- | --- | --- | --- |
| Windowed | 5 | false / success | false / success |
| Borderless | 2 | false / success | false / success |
| Requested fullscreen | 3 | false / success | true / `0xC01E0006` |
| Restored borderless | 2 | false / success | false / success |

The documented [global ownership query](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/d3dkmthk/nf-d3dkmthk-d3dkmtcheckexclusiveownership)
and [per-VidPN query](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/d3dkmthk/nf-d3dkmthk-d3dkmtcheckvidpnexclusiveownership)
were diagnostic controls only. They missed the FSO-enabled fullscreen state that
also needs display capture on this fixture; neither identifies the owner PID/HWND.
No PresentMon trace was collected for this control; previous traces do not classify
these new runs.

## Implementation

The first-party PrintWindow protocol is shared with the standalone experiment and
keeps its length, sequence, timestamp and geometry tests. The application dispatches
`--clipline-print-worker` before Tauri, singleton handling or elevation logic.
The parent supervises the child with a kill-on-close job. Only one request can be
pending; polling yields to recorder commands, with a two-second hard worker deadline.
The request retains its original observation context, discards responses older than
250 ms, and uses the capture timestamp rather than stamping delayed pixels as current.
Resize recreates the owned DIB. Capture/IPC failures produce bounded retries, then
stop; there is no fallback to another API because PrintWindow failed.

The seed frame now uses the initial validated client dimensions to establish stable
encoder output. Nightly 1.0.5 used the target monitor dimensions: a 16:9 game on a
32:9 monitor was consequently fitted into a 32:9 recording with unnecessary side
padding. Initial unavailable client geometry still falls back to monitor dimensions;
start the recording with a visible, restored game to establish its intended aspect.
Existing GPU/CPU aspect-preserving conversion handles changing input dimensions.
The session canvas stays fixed, so later aspect changes can still produce bars.
Both sources use the existing encoder device,
clock, audio and replay/session pipeline. Source changes appear in live status and
structured diagnostics as `experimental_print_window`,
`experimental_fullscreen_display`, or `experimental_waiting_black`. The recording
rail shows Display/Wait and source details in tooltips.

## Evidence

Local root: `C:\Users\Dain\Desktop\CliplineHybridTest-20260908-232342`.
Keep desktop screenshots and full settings backups local.

An initial application run automatically recorded the flip mock through windowed,
borderless, overlap, fullscreen, focus loss/restore, resize and minimize/restore.
The session (`session_1788925023.mp4`, 72,239,022 bytes) and 30-second F6 replay
(`clip_1788925134.mp4`, 19,103,803 bytes) completely decode. Replay has 1,800 valid
counter frames, 1,798 advances, one repeat, zero backward jumps, H.264 1280x720 and
48 kHz stereo Opus. Channel RMS is approximately -20.02/-20.01 dBFS. Inspected
borderless overlap output excludes the magenta covering window; fullscreen output
contains the mock; during inspected focus loss the isolated window path resumes.
Sampled live views show no yellow capture border. These are fixture observations,
not proof of tear freedom or actual game compatibility.

The initial run predates the delayed-packet and automatic-target-exit refinements.
Final build acceptance is recorded below after repeating the application matrix.

## Validation refinements

Review reproduced an eager-source cadence defect: repeated old PrintWindow
timestamps could spin without emitting a frame. A failing regression now verifies
bounded reads and wall-clock emission with an always-stale source. Pacing is opt-in
for the hybrid; the existing WGC/DD event-driven paths retain their behavior.
All eleven cadence regressions pass.

Initial repeat attempts `final-native` and `final-native-verified` are excluded
from full-matrix acceptance: the harness did not establish foreground ownership
and then unnecessarily restored/moved the exclusive mock. The corrected harness
verifies foreground and only restores an actually minimized window.
`acceptance-native` then reproduced a real mock error on focus loss:
`Present failed: 0x887A0001`. DXGI had changed fullscreen state but the mock
relied on WM_SIZE to rebuild its buffers. The fixture now marks buffers for resize
on observed swap-chain state changes and handles a confirmed transition racing
Present. Unrelated presentation errors remain fatal. This follows Microsoft
[flip-model transition requirements](https://learn.microsoft.com/en-us/windows/win32/direct3ddxgi/for-best-performance--use-dxgi-flip-model).
The rebuilt fixture SHA-256 is
`41671EE21B64B8C18A298A9DC1F8CA41532937047AFC1C51A0459D89E9639E58`;
MSVC /W4 /WX passes. The failed fixture run is not evidence of capture failure.

## Final hardware acceptance

Application SHA-256:
`778C3A387D2189CD19D54330695B7454BA67CCF5D943089CC05F4356C488278D`.
Only formatting/comments changed in the Rust implementation after this build.
Two complete approximately 115-second runs used the repaired fixture and the same
Clipline parent PID 14644, without restarting or rearming the app between games.
Each covered windowed, borderless, a nonactivating magenta overlap, requested
fullscreen, focus loss/restore, return to borderless, resize, minimize/restore,
and an F6 replay. Both mock stderr logs are empty. The copied FSO override is
removed after each run.

| Evidence directory | Full session / bytes | Replay / bytes | Replay counters: advances / repeats / backward | Fullscreen sample |
| --- | --- | --- | --- | --- |
| `acceptance-native-recovery` (FSO disabled) | `session_1788926473.mp4` / 73,362,919 | `clip_1788926585.mp4` / 19,106,900 | 1,702 / 97 / 0 | 360 valid frames, 359 advances |
| `acceptance-optimized` (FSO enabled) | `session_1788926603.mp4` / 73,237,744 | `clip_1788926715.mp4` / 19,104,059 | 1,798 / 1 / 0 | 360 valid frames, 359 advances |

All four files fully decode; both 30-second replays have 1,800 readable counter
frames, H.264 High 1280x720 60 fps BT.709 video and 48 kHz stereo Opus. Replay
channel RMS is -20.013/-20.007 and -20.038/-19.998 dBFS respectively. The native
run overlapped a cargo compilation; its repeated frames are retained in the result,
not described as perfect frame delivery. Neither test proves synchronized or
tear-free capture. Fullscreen samples begin three seconds after each recorded
stage transition and cover six seconds.

Inspected recordings exclude the magenta overlapping window in borderless mode,
preserve the red/blue/green fixture colors, show fresh fullscreen game pixels,
aspect-fit the resized window, emit black while minimized, and resume after
restore. Live screenshots show no yellow capture border. On focus loss Windows
leaves fullscreen, so the isolated window path resumes; do not describe every
unfocused interval as black. An earlier bounded `final-native-verified` control
held shell fullscreen with foreground false and logged waiting/black instead.

During the optimized run’s last 31.95-second borderless interval, parent plus worker
private memory ranged 113.4–132.3 MiB (116.7 at start, 119.2 at end). They consumed
17.5 CPU seconds in total, approximately 0.55 of one logical processor. The native
tail ranged 101.5–124.8 MiB and consumed 17.97 CPU seconds over 31.89 seconds.
These are short debug-build observations, excluding WebView/GPU memory, not a
long-duration leak or production performance certification.

No League/Valorant installation or usable login was available in the earlier
machine inspection. This validates the controlled D3D11 flip fixture only.
Actual games, long recordings, multiple monitors, DPI changes and HDR still need
validation before treating the experimental backend as a general replacement.

Local evidence includes per-stage UTC/QPC/focus/shell/resource CSVs, live and
decoded PNGs, source-change diagnostics, complete audio/decode logs, replay and
fullscreen counter analyses, settings backup and build hashes. A final workspace
check first encountered Windows’ running-executable file lock; the app was then
stopped before the clean rebuild and repeated gates.

Final local gates: cargo test --workspace (1,541 passed, CI=1 to skip real-device
unit tests; the separate app matrices exercised hardware), fresh application-cache
cargo clippy --workspace --all-targets -- -D warnings, and MSVC mock /W4 /WX
build all pass. Neutral policy/protocol regressions run on both CI platforms.

## PR review: display topology recovery

Cursor's review at `6f301fa` correctly identified that per-display re-lookups in
`observe` could return a fatal initialization error during an ongoing recording.
The hybrid now queries handles and monitor info together, once per observation.
A strict variant marks enumeration incomplete if any monitor-info query fails or
returns zero dimensions. Existing non-hybrid callers retain best-effort behavior.

During capture, failed/partial enumeration, an empty result or an absent target
monitor makes the observation unavailable. Existing source policy emits black,
releases capture resources and discards pending/acquired pixels, preserving the
session and audio while valid topology returns. Identity and capture-protection
validation remain outside this recovery path and still fail recording. Startup
still needs a valid target display to establish encoder geometry; this change
does not promise recovery from an initial construction failure.

Two extracted-observation regressions failed with the previous error propagation
and missing-target handling, then passed after the fix. Five added tests cover
failure, missing/partial topology, post-acquisition rejection, recovery and the
existing multiple-monitor restriction; the policy regression also runs on Linux.
All 1,546 workspace tests and fresh capture-cache warning-denied workspace Clippy
pass locally. Independent code review found no additional defects.

Evidence: `C:\Users\Dain\Desktop\CliplineTopologyTest-20260909` contains the
red/green and final gate logs. No physical monitor unplug, multiple-monitor, DPI
or HDR acceptance is implied by these injected failures. The earlier hardware
matrices remain evidence for their recorded build only.

### Five-minute borderless recording after the review fix

Build `a031ccd` (application SHA-256
`EC4388911C935D149EDB3130FF064A021C56EE42C4CC8BA0AC460EB19D4B9141`)
ran the unchanged flip fixture with `--borderless --seconds 300`. The same Win10
19045 / Radeon 780M / driver 32.0.31041.1004 machine and VB-CABLE settings were
used. No compatibility override, driver installation or WGC capture was used.
Clipline PID 17576 automatically selected PrintWindow with worker PID 17040.

`session_1788929174.mp4` is 189,871,425 bytes and fully decodes. It contains
17,891 video frames, with last video PTS 298.1819 s and decoded audio duration
298.18158 s. Of those frames, 17,860 have readable fixture counters: 17,858
advances, one repeat and zero backward jumps. The other 31 counter rows are black
at the initial debounce and target-exit tail (frames 0–11 and 17,872–17,890).
The inspected 60-second recording frame has the expected fixture colors/content.
The fixture exited after its configured duration with empty stderr; the first
PowerShell process wrapper did not retain its exit code, so no zero-exit claim
is made for that run.

A second 15-second fixture invocation recorded automatically in the same parent
instance and exited with code 0. `session_1788929548.mp4` (9,346,520 bytes) also
fully decodes. Both worker processes exited; Clipline remains open. No fresh
replay-hotkey or fullscreen-transition acceptance was performed in this run.

For the last 269.47 seconds of the five-minute invocation, parent plus worker
private memory ranged 108.20–115.52 MiB (108.20 at the start, 110.91 at the end).
Their combined CPU consumption was 141.83 seconds, approximately 0.53 of one
logical processor. The worker identity stayed constant and diagnostics logged
no intermediate source restart. This is a five-minute debug-build observation,
excluding WebView/GPU memory, not long-duration leak or release-performance proof.

The fixture's yellow/blue pulse follows its waveOut playback position. The local
analysis compares decoded video PTS at pulse edges with decoded stereo RMS tone
edges in 1 ms bins (threshold 0.03), resampling audio onto its zero-based timeline.
Across 291 matched edges, video leads audio by 71.44–72.44 ms; the first and last
minute medians are both 71.44 ms. Stereo RMS is -20.02/-20.03 dBFS. Reanalyzing
the earlier optimized replay `clip_1788926715.mp4` with the same method gives a
66–67 ms video lead. The offset remains to be localized between fixture/device
timing and recording; these observations show no accumulating drift over this
sample, not zero A/V offset, physical endpoint latency or synchronized tear-free
frames. No uncalibrated global timestamp correction was added.

The computer-use native pipe was unavailable during this validation, so no new
live-desktop yellow-border screenshot is claimed. Recording frames are not a
substitute for that check. Raw logs, resource samples, hashes, media inventory,
frame/pulse analysis and baseline comparison are in `CliplineTopologyTest-20260909`.
Windows/Ubuntu CI and dependency-security checks pass for implementation commit
`a031ccd` ([CI run](https://github.com/Clipline-CC/clipline/actions/runs/34312274775)).
