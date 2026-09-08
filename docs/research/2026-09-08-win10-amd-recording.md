# Windows 10 AMD capture and recording validation

## Result

Clipline builds, launches, records, saves, and plays H.264 display capture on this
physical Windows 10 machine without an observed yellow border. **Isolated game
capture is not established.** The DWM probe returns blank, unchanged surfaces for
an AMD-accelerated WebGL browser window, both windowed and browser fullscreen.
Do not integrate this probe into production recording based on the working GDI
fixture or the display-recording result.

## Environment and prerequisites resolved

- Windows 10 Home 22H2 build 19045, Ryzen 9 7940HS, Radeon 780M.
- Official AMD Adrenalin 26.8.1 installation completed during this session;
  Windows now reports `AMD Radeon 780M Graphics`, driver `32.0.31041.1004`, status OK.
  No reboot was initiated. This supersedes the earlier Basic Display Adapter baseline.
- Microsoft Visual Studio 2022 Build Tools and Windows SDK installed successfully
  with the official installer, exit 0. Rust stable 1.98.1 now builds the app.
- The original probe executable is unchanged and its official app-local Microsoft
  runtime remains beside it. Both `--help` and `--list` pass.
- The browser process open during driver installation retained the Microsoft
  software renderer. A separate ordinary Edge test profile launched afterward
  reports `ANGLE (AMD, AMD Radeon 780M Graphics (0x000015BF) Direct3D11 ...)`.
  This test used screenshots and ordinary window controls, no remote debugging.
- The pinned LGPL FFmpeg archive and every allowlisted payload hash were verified.
  Initial staging attempts failed with DLL access denied during cleanup; a later
  diagnostic retry succeeded. The original error was masked by cleanup, so the
  cause is unresolved, not attributed conclusively to the driver or antivirus.
  The running debug app uses the verified local FFmpeg executable through
  `CLIPLINE_FFMPEG`; thumbnail generation then works.

Local evidence: `C:\Users\Dain\Desktop\CliplineWin10E2E-20260908-031609`.
Keep desktop images, raw logs, browser profile, local support ZIP and recordings
local; they can include unrelated desktop content. No private support report was sent.

## DWM experiment on the AMD adapter

| Test | Reads | Changed samples | Errors | Observed result |
|---|---:|---:|---:|---|
| 22-second controlled fixture | 1,117 | 665 | 175 | Correct red/blue/green; covering magenta window excluded; resize and restore recovered. All errors were minimized-target errors. |
| 300-second controlled fixture | 17,530 | 10,427 | 177 | 58.43 reads/s; all errors while minimized. Mean read 1.347 ms, max 52.127 ms. |
| 30-second accelerated WebGL, windowed | 1,741 | 0 | 0 | White client surface despite visibly moving AMD-rendered scene. |
| 30-second accelerated WebGL, browser F11 fullscreen | 1,758 | 0 | 0 | Gray surface despite visibly moving AMD-rendered scene. |

The five-minute probe's samples after 30 seconds stayed within 29.42–31.02 MiB
working set, 26.41–28.02 MiB private memory, and 737–742 handles. Working set ended
at 29.47 MiB and private memory at 26.44 MiB, without sustained growth in this run.
Clipline recording ran concurrently for part of this test; this is a bounded
feasibility sample, not an isolated performance benchmark.

Evidence subdirectories: `amd-fixture`, `amd-soak`, `amd-webgl-windowed`, and
`amd-webgl-fullscreen`. Renderer screenshots are `webgl-fresh-driver.png` and
`webgl-fullscreen.png`. White/gray BMPs and constant hashes/update IDs corroborate
the visible capture failure; successful API reads alone do not establish useful
frames. The fixture result does not prove synchronized or tear-free delivery.
Browser F11 is not an exclusive-fullscreen game test.

## Production display recording

Settings used: Desktop Duplication, display source, automatic game switching off,
Auto encoder, 720p60, 5 Mbps, 30-second memory replay buffer. Microphone disabled.
The driver changed display identity/resolution; Clipline reported its existing
full-display recovery instead of failing on the saved Basic-driver display ID.

- F6 saved `clip_1788852437.mp4`: 30 seconds, 1,800 frames, 18,786,709 bytes.
  In-app playback and seeking worked. FFmpeg decoded the complete file with exit 0.
- The updated no-fallback build saved `session_1788853114.mp4`: 126.50 seconds,
  7,590 frames, H.264 High, 1280x720, 60 fps, BT.709, approximately 5 Mbps.
  Complete decode exited 0. Extracted frames at 60 and 62 seconds show correct
  colors, different square positions, and WebGL counters 22,745 and 22,865.
- A locally prepared support bundle records actual backend `desktop_duplication`
  and encoder `AMD AMF · H.264`, with a full 30-second replay buffer and no recent
  recorder error. This is runtime evidence, not just the configured preference.
- Sampled live desktop views show no yellow border. Display capture includes
  overlapping windows as expected; it does not satisfy isolated game capture.
- Windows returned `WASAPI: Element not found (0x80070490)` for default output
  audio. Recording continued video-only; **audio remains unvalidated**. Audio
  device entries exist, but an installed device is not proof of an active endpoint.

The debug executable after the change has SHA-256
`8D2A2A85D1EFB4F149EF67F709A6F4C8C18D09CE21CDD701CE3CB3F8102750F4`.

## No-fallback fix and checks

The existing production code silently used WGC after DXGI initialization or first
frame failure and for detected window sources, even with Desktop Duplication
selected. A neutral capture policy now returns DXGI errors directly and rejects
window sources before invoking either backend. Auto and explicit WGC retain their
existing behavior and persisted settings values. Settings explains display pixels,
overlapping windows, the game-switching restriction and stop-on-failure behavior.

Two regression tests failed against the extracted old decision. All four policy
tests pass with the fix, including unchanged Auto/WGC success/error behavior.
`cargo test --workspace --locked` passes on this AMD Windows machine; after
`cargo clean -p clipline-app`, warning-denied workspace Clippy also passes.
Build and `cargo run -p clipline-app --locked` succeed. Independent code review
found no remaining WGC constructor path for explicit Desktop Duplication.

## Remaining acceptance

No League/Valorant installation was found in the earlier uninstall/standard-path
inspection, and no usable game/login was supplied during this run. Practice-game
windowed, borderless fullscreen and exclusive fullscreen are untested. Sustained
game recording, game-only overlap exclusion, physical/game audio, frame synchronization and
device-loss recovery still need separate acceptance. Do not broaden window capture
to the desktop silently or claim this meets the complete game-only requirement.

The no-fallback change reached PR #200 at `c5960c3`; Windows and Ubuntu CI both
passed in run `34200829938`. The PR description includes the accelerated DWM failure.

## Virtual audio follow-up on the PiKVM host

The user installed the official VB-CABLE package after the agent's elevated launch
was blocked by automatic approval review. Windows exposes an active `CABLE Input`
playback endpoint (also the Windows default), a `CABLE In 16ch` endpoint and a
`CABLE Output` recording endpoint. The working driver reports status OK; two extra
VB-Audio driver entries report Device Manager code 10 and were not modified.
No reboot was initiated by the agent.

Clipline was restarted and configured explicitly to capture **CABLE Input** with
output volume 100%, output audio enabled, microphone disabled, and process splitting
off. A local SoundPlayer/WinForms fixture plays 30 seconds of 48 kHz stereo PCM:
440 Hz left, 880 Hz right, alternating one second of tone and one second of silence.
Its visual timer is approximate and is not a sample-synchronized A/V reference.

`session_1788854412.mp4` is 40,438,535 bytes, duration 64.10 seconds, with 1280x720
H.264 video and a 48 kHz stereo Opus audio track. Complete audio/video decoding
succeeds (3,840 video frames). All 15 tone bursts are present, with intervening
silences about 0.997–1.000 seconds. In the 2.0–2.5 second tone interval, channel RMS
levels are -17.013/-16.989 dBFS and zero-crossing rates are 0.018333/0.036667 per
sample, consistent with the intended 440/880 Hz channel assignment. The in-app
player opens the file with one selected audio track. This validates the real WASAPI
virtual-output path through Opus encoding, MP4 saving and decoding without speakers.

F6 replay saving was tested separately with the same fixture.
`clip_1788854665.mp4` is 19,069,134 bytes, 30.00 seconds, 1,800 video frames and
48 kHz stereo Opus. Full decoding exits 0; both audio channels contain the test
signal (whole-replay RMS about -20.60 dBFS, including silent intervals). Its SHA-256
is `D7D9EB035187DFA1250C515C5E375742C90314DE716B748CD3E906B7FF30E0CD`.
The app is left open with the virtual endpoint selected and recording paused.

An earlier settings-triggered capture restart displayed `DuplicateOutput failed:
The parameter is incorrect (0x80070057)`; a subsequent manual start succeeded and
produced the recording above. The cause of that transition failure remains open.
It did not switch to WGC. Audio-driver availability does not resolve the separate
accelerated DWM capture failure, physical microphone or game-process audio acceptance.

Evidence: `C:\Users\Dain\Desktop\CliplineAudioTest-20260908-035219`, including the
original WAV, fixture script, selected settings, decode/silence/channel analyses
and local screenshots. Recorded MP4 SHA-256:
`966FDC63754F5C632AEEFAC6956B46E085ACBF3A318F299862EAE0F3527C21FC`.
