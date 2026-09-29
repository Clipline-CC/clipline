**Clipline Windows 10 support research — 7 September 2026**

Recommendation: retain Rust/Tauri, Windows Graphics Capture (WGC), DXGI Desktop Duplication, and WASAPI. Establish a tested Windows 10 22H2 x64 target, repair the standalone installer’s runtime permissions, and make capture selection account for Windows 10’s limitations. The evidence supports focused compatibility work, not a replacement recording framework.

The user clarified that **the yellow border is a deal breaker**. For that requirement, the recommended border-free path must not silently fall back to WGC on Windows 10. Use DXGI display/region capture; on failure, recover within DXGI where possible or report that border-free capture is unavailable. Switching to bordered capture must be an explicit user choice. This supersedes the earlier repository preference for silent DXGI-to-WGC fallback.

This is research and an implementation recommendation, not a claim that the reported failures have been reproduced or fixed.

**Scope and evidence**

The supplied worktree is at `9e09d76c` (0.1.34, July 16). The public Nightly inspected on September 7 is **1.0.4**, published September 6, at **`c455423898fc227f0a2dba8fae41b199827d871c`**. Findings below were rechecked against that release through `git show`/`git grep`; the working branch was not changed. Links to application code use the immutable release commit. [Nightly release](https://github.com/Clipline-CC/clipline/releases/tag/nightly)

Sources include Microsoft API and deployment documentation, Tauri’s pinned installer source, OBS’s official application-audio documentation, current Clipline source, and repository history. An independent read-only code examination corroborated the capture/audio findings. No third-party implementation was copied.

The repository records an older Windows 10 WebView startup failure and a yellow-border report. The current private support-report population was not supplied or analyzed. Consequently, this report cannot rank failures by frequency or assert that one cause explains them all. [Recorded startup history](https://github.com/Clipline-CC/clipline/blob/9e09d76c/handoff.md#L697), [border issue #42](https://github.com/Clipline-CC/clipline/issues/42)

**Does Clipline depend on a Windows 11-only API?**

Partly: border suppression is unavailable on normal Windows 10 client builds. The underlying capture pipeline is not Windows 11-only.

| Component | Windows 10 reality | Current Clipline behavior |
|---|---|---|
| WGC window/display capture | The HWND/HMONITOR interop path requires 1903, build 18362. The free-threaded frame pool requires 1809. | Uses these APIs directly; advertising 1803 as sufficient for this path is inaccurate. |
| WGC yellow-border suppression | `IsBorderRequired` starts at build 20348, above normal Windows 10 22H2’s 19045. | Calls the setter best-effort and ignores failure. Capture can work while the border remains. |
| DXGI Desktop Duplication | Available since Windows 8; captures display pixels. | Already implemented for display/region sources, but attempted only on explicit selection. |
| WASAPI mixed output/microphone | Not a Windows 11-only feature. | Existing baseline audio path; mixed output is also retained with split process tracks. |
| WASAPI process loopback | Microsoft’s sample specifies 20348+, while OBS officially supports Windows 10 2004+. | Gates at 19041 and attempts bounded activation. An OS-version gate alone is not proof of successful capture. |
| WebView2 | Microsoft states Windows 10 22H2 runtime updates continue until at least October 2028. | Regular installer uses Evergreen; standalone bundles Fixed Version 152.0.4191.62. |
| Software H.264 | Microsoft provides a Windows Media Foundation encoder. | Native software MFT and FFmpeg `h264_mf` paths already exist in 1.0.4. |

API references: [WGC window interop](https://learn.microsoft.com/en-us/windows/win32/api/windows.graphics.capture.interop/nf-windows-graphics-capture-interop-igraphicscaptureiteminterop-createforwindow), [free-threaded frame pool](https://learn.microsoft.com/en-us/uwp/api/windows.graphics.capture.direct3d11captureframepool.createfreethreaded), [border control](https://learn.microsoft.com/en-us/uwp/api/windows.graphics.capture.graphicscapturesession.isborderrequired), [Windows 10 release builds](https://learn.microsoft.com/en-us/windows/release-health/release-information), [DXGI overview](https://learn.microsoft.com/en-us/windows/win32/direct3ddxgi/desktop-dup-api), [Microsoft audio sample](https://learn.microsoft.com/en-us/samples/microsoft/windows-classic-samples/applicationloopbackaudio-sample/), [OBS audio support](https://obsproject.com/kb/application-audio-capture-guide), [WebView2 lifecycle](https://learn.microsoft.com/en-us/deployedge/microsoft-edge-support-lifecycle), [H.264 MFT](https://learn.microsoft.com/en-us/windows/win32/medfound/h-264-video-encoder).

Do not interpret the SDK label “Windows 10, version 2104 / 20348” as an ordinary consumer Windows 10 update users can install. Do not promise that Windows 11 alone guarantees borderless WGC either: Microsoft documents access requirements and circumstances where the border remains. [Border-control contract](https://learn.microsoft.com/en-us/uwp/api/windows.graphics.capture.graphicscapturesession.isborderrequired)

**Finding 1: automatic capture selection does not implement a Windows 10 strategy**

In 1.0.4, `open_screen_capture` attempts DXGI only for an explicit `DesktopDuplication` selection and a display/region source. `Auto` goes to WGC. A DXGI construction or first-frame failure is logged and falls back to WGC. Thus the no-border alternative already exists, but users must find it and it can return to bordered capture. [Current selection logic](https://github.com/Clipline-CC/clipline/blob/c455423898fc227f0a2dba8fae41b199827d871c/apps/clipline-app/src/service/capture.rs#L267)

**Critical for automatic game recording:** `RuntimeState::options_for` unconditionally replaces the configured capture source with the detected game's `WindowHandle`. The capture backend setting remains present, but the DXGI display/region guard excludes that source, so the game uses WGC. Selecting Desktop Duplication in Settings does not eliminate the border for this path. [Detected-game override](https://github.com/Clipline-CC/clipline/blob/c455423898fc227f0a2dba8fae41b199827d871c/apps/clipline-app/src/app/runtime.rs#L250)

The complete border-free solution therefore needs an explicit game-display capture mode: retain detection, game identity, event markers, and automatic recording lifecycle, but resolve the game's monitor and capture it with DXGI. Prefer borderless-fullscreen games for this mode. A visible monitor-capture explanation and focus-loss behavior are part of the product change; do not silently broaden existing per-window capture. Recheck the selected monitor on recording restarts or game moves. Merely changing the backend enum or its default is insufficient.

Proposed policy, after the hardware checks below pass:

| User’s source | Automatic choice | Fallback behavior |
|---|---|---|
| Windows 10 display or region, border-free required | DXGI on validated configurations | Recover within DXGI where possible; report failure if unavailable. Offer WGC explicitly with its border caveat. |
| Individual game/window | WGC | Explain the Windows 10 border limitation. Offer an explicit display/region choice if the user wants border-free capture. |
| Detected game with explicit border-free display mode | Resolve game's monitor and use DXGI; preserve game metadata/markers | Honor the no-WGC guarantee. Handle focus loss and monitor changes without unexpectedly capturing unrelated desktop content. |
| Windows 11 display or region | Retain current WGC default initially | Change only when comparative measurements justify it. |
| Explicit backend selection | Preserve the requested preference | For an explicit no-border selection, do not silently substitute WGC. Explain failure and offer alternatives. |

A cropped desktop image is not equivalent to isolated window capture: overlapping windows and whatever occupies the region can appear in the recording. Do not silently convert a window source into a monitor source. There is no basis here for promising universally reliable, isolated, border-free game-window capture on Windows 10 within the current no-injection architecture.

**Finding 2: DXGI needs monitor-adapter handling before it becomes the Windows 10 default**

Clipline creates the shared D3D11 device on the default adapter. Its DXGI backend enumerates only that adapter’s outputs, rejecting a target monitor attached to another GPU. It also explicitly rejects rotated displays. These are intentional implementation limits, but they undermine an automatic Windows 10 display strategy on hybrid laptops and external monitors. [Device creation](https://github.com/Clipline-CC/clipline/blob/c455423898fc227f0a2dba8fae41b199827d871c/crates/clipline-capture/src/windows/d3d11.rs#L43), [output selection](https://github.com/Clipline-CC/clipline/blob/c455423898fc227f0a2dba8fae41b199827d871c/crates/clipline-capture/src/windows/dxgi_dup.rs#L330)

Resolve the selected monitor’s adapter before constructing the single shared capture/encode device. Revalidate encoder selection on that device; do not assume changing adapters preserves the available encoders. Microsoft explicitly requires `DuplicateOutput` to receive a device created on the output’s adapter. [DXGI contract](https://learn.microsoft.com/en-us/windows/win32/api/dxgi1_2/nf-dxgi1_2-idxgioutput1-duplicateoutput)

For rotated displays, expose WGC as an explicit alternative until DXGI rotation handling is implemented and tested; strict border-free mode must report its limitation. Existing DXGI access-loss retry logic should be exercised, not rewritten. Test lock/unlock, display-mode changes, disconnect/reconnect, and fullscreen transitions: Microsoft documents that these can invalidate duplication. [AcquireNextFrame behavior](https://learn.microsoft.com/en-us/windows/win32/api/dxgi1_2/nf-dxgi1_2-idxgioutputduplication-acquirenextframe)

**Finding 3: standalone WebView2 deployment omits a documented Windows 10 permission step**

Microsoft requires unpackaged Win32 apps shipping Fixed Version WebView2 120+ on Windows 10 to grant the runtime folder inherited read/execute permissions for `S-1-15-2-1` and `S-1-15-2-2`. [Microsoft deployment requirements](https://learn.microsoft.com/en-us/microsoft-edge/webview2/concepts/distribution)

Current Clipline bundles runtime **152.0.4191.62**. Its postinstall hook is empty; the runtime staging/verification scripts check payload identity and freshness but do not apply those permissions on the destination machine. The pinned Tauri CLI 2.11.2 NSIS template also contains no such grant. This is a **confirmed deployment omission and plausible startup-failure cause**, not a reproduced diagnosis of the reported machines. Existing inherited permissions can affect whether a particular installation fails. [Standalone config](https://github.com/Clipline-CC/clipline/blob/c455423898fc227f0a2dba8fae41b199827d871c/apps/clipline-app/tauri.standalone.conf.json), [installer hooks](https://github.com/Clipline-CC/clipline/blob/c455423898fc227f0a2dba8fae41b199827d871c/apps/clipline-app/windows/hooks.nsh), [pinned Tauri template](https://github.com/tauri-apps/tauri/blob/tauri-cli-v2.11.2/crates/tauri-bundler/src/bundle/windows/nsis/installer.nsi)

Use the existing postinstall hook to apply the documented grants to the exact installed runtime folder, check the result, and verify fresh installs and updates under a standard Windows 10 user account. Setting ACLs only on the build machine is insufficient. Do not broaden permissions on the user profile or disable the Chromium sandbox.

Keep the regular Evergreen installer as the small default and the existing standalone variant as the isolated-runtime alternative. Both are supported Tauri distribution modes. An offline Evergreen installer addresses download access, while a Fixed Version runtime addresses a different deployment requirement; neither is a universal repair for profile or driver failures. [Tauri installer options](https://v2.tauri.app/distribute/windows-installer/)

**Finding 4: the native startup diagnostic overstates what it knows**

Current code classifies a failed Tauri message or frontend-readiness timeout, then tells every affected user to install/repair WebView2. Runtime diagnostics only read Evergreen registry entries. Those entries can be absent on a legitimate Fixed Version installation, so their absence does not prove that the bundled runtime is missing. [Current diagnostic implementation](https://github.com/Clipline-CC/clipline/blob/c455423898fc227f0a2dba8fae41b199827d871c/apps/clipline-app/src/app/webview.rs#L33)

Use the existing local diagnostic/support machinery to record the installer variant, selected/actual runtime version and location, creation error, user-data-folder location/access result, and relevant process-failure details. Report “interface failed to start” until there is evidence of a missing runtime. A readiness timeout can also reflect frontend boot or IPC failure.

WebView2 exposes process-failure kinds, reasons, and exit information; renderer, browser, and GPU failures have different recovery behavior. Collect that distinction before prescribing repairs. [Process diagnostics](https://learn.microsoft.com/en-us/microsoft-edge/webview2/concepts/process-related-events)

For suspected profile corruption, test with a separate temporary WebView profile while preserving the original. Microsoft documents that an unwritable user-data folder can prevent startup; security software can also deny runtime execution or profile writes. Provide a native local diagnostic export route when the web interface is unavailable. [User-data folders](https://learn.microsoft.com/en-us/microsoft-edge/webview2/concepts/user-data-folder), [execution and permission failures](https://learn.microsoft.com/en-us/microsoft-edge/webview2/concepts/measures)

**Finding 5: retain the current audio and encoder fallbacks, then test their Windows 10 behavior**

Do not remove process capture from Windows 10 solely because Microsoft’s sample has a higher minimum. Retain the current build eligibility check plus real activation. Validate using two independently audible applications: the selected process track must contain its sound and exclude the other app. Silence by itself proves neither success nor failure. OBS also documents application-specific audio incompatibilities. [OBS guide](https://obsproject.com/kb/application-audio-capture-guide)

The current service always attempts a mixed-output safety track and adds individual process tracks when requested and available. Splitting is off by default. Explain clearly that mixed output contains all audio on the selected endpoint; a safety track is not game-only audio. [Audio construction](https://github.com/Clipline-CC/clipline/blob/c455423898fc227f0a2dba8fae41b199827d871c/apps/clipline-app/src/service/capture.rs#L414)

Software H.264 is already wired, including a native `SoftwareMftH264Encoder`. Recoverable WASAPI device errors already reactivate capture and cover outages with silence. Earlier worktree notes describing those as missing are obsolete. Test those existing paths instead of adding substitutes. [Encoder wiring](https://github.com/Clipline-CC/clipline/blob/c455423898fc227f0a2dba8fae41b199827d871c/apps/clipline-app/src/service/encoders.rs#L223), [audio recovery](https://github.com/Clipline-CC/clipline/blob/c455423898fc227f0a2dba8fae41b199827d871c/crates/clipline-capture/src/windows/wasapi/capture.rs#L323)

Include Windows 10 N without/with Media Feature Pack: Microsoft documents that N editions omit media components, including Media Foundation functionality. Detect an unavailable codec/component and give the corresponding repair, rather than labeling it a generic Windows 10 failure. This is a test case, not a diagnosed cause in the supplied reports. [Media Feature Pack](https://support.microsoft.com/en-us/windows/experience/platform-variants/media-feature-pack-for-windows-10-11-n-february-2023)

**Support policy and validation**

Proposed primary Windows 10 target: **22H2 x64, build 19045, with its applicable updates and supported graphics driver**. This is a product-support recommendation, not a claim that earlier versions cannot run. Treat 21H2 LTSC as a separately validated target if users need it. Retire the current misleading 1803 and “Windows 10 20348+” support categories. Separate minimum API availability, intended support, and actually tested configurations. [Current matrix](https://github.com/Clipline-CC/clipline/blob/c455423898fc227f0a2dba8fae41b199827d871c/docs/COMPATIBILITY.md)

Keep Windows servicing status separate from Clipline capability: continuing WebView2 updates are not a guarantee for all Windows components. Microsoft’s stated Windows 10 22H2 WebView2 horizon gives no reason to replace Tauri solely for Windows 10 support. [Runtime lifecycle](https://learn.microsoft.com/en-us/deployedge/microsoft-edge-support-lifecycle)

The existing `windows-latest` jobs and build-time benchmarks do not establish Windows 10 desktop compatibility. GitHub’s hosted x64 Windows images are Server images, and several real-device tests skip under `CI`. Add an actual client-OS acceptance gate. [GitHub runner images](https://github.com/actions/runner-images), [Clipline CI](https://github.com/Clipline-CC/clipline/blob/c455423898fc227f0a2dba8fae41b199827d871c/.github/workflows/ci.yml)

| Gate | Minimum useful coverage | Evidence required |
|---|---|---|
| Installer/startup | Clean Win10 22H2 VM; regular without Evergreen; standalone without Evergreen; standard user; upgrade from an affected version | Actual shipped installer opens a responsive UI, sends `frontend_ready`, survives reopen/reboot, and uses the intended runtime. Standalone ACLs verified on the installed files. |
| Capture | Physical AMD, NVIDIA, and Intel/hybrid configurations; WGC window and DXGI display/region | Actual backend recorded; first frame, motion, resize, replay save, full-session recording, and valid playback. Compare performance against WGC on the same machine. |
| Display lifecycle | External monitor, different adapter, rotated display, lock/unlock, reconnect and fullscreen transitions | Expected fallback/recovery, bounded waits, no unnoticed permanent frozen video, and no unintended widening of the capture source. |
| Audio | Split off/on; two distinct test sounds; microphone; endpoint switch/reconnect; initially quiet source | Correct isolation, labeled mixed track, continuity and A/V sync through recovery. |
| Encoding/playback | Hardware H.264 and existing software fallback; N edition media components; regular/standalone variants | Saved MP4 plays and seeks in Clipline; fallback meets a measured usable resolution/FPS budget. |

A VM is useful for installer coverage. It does not substitute for physical GPU, driver, audio-device, and hybrid-display testing.

**Recommended delivery order**

1. Correct the support matrix and capture exact app version, Windows edition/build/revision, installer variant, symptom, actual backend, and driver in the existing report flow. Reuse collected fields; add only missing evidence.
2. Reproduce and fix the Fixed Version permission omission with one fresh-install and one upgrade test on Windows 10. Improve the native diagnostic so it identifies the runtime variant and does not prescribe Evergreen repair for every failure.
3. Validate/harden DXGI monitor-adapter selection, then enable DXGI-first Auto for eligible Windows 10 display/region sources. Keep WGC window capture and explicit backend preferences.
4. Run the audio isolation and H.264 fallback matrix against the shipped installer variants. Expand implementation only for a reproduced remaining failure.
5. Gate the compatibility release on the Win10 acceptance results and document the configurations actually exercised. Apply the repository’s plan-first, tests, clean clippy, and release verification workflow when implementing.

Given the clarified border requirement, prioritize item 3: make the existing no-border selection strict, route the explicit border-free detected-game mode through monitor capture, address adapter limitations, and validate the Windows 10 default. Current 1.0.4 exposes **Settings > Capture > Capture backend > Desktop Duplication (no Windows 10 border)**; this only works for display/region capture when no detected game overrides the source. That current option can also silently fall back, so it does not yet provide the strict guarantee requested. [Current settings](https://github.com/Clipline-CC/clipline/blob/c455423898fc227f0a2dba8fae41b199827d871c/apps/clipline-app/ui/index.html#L446)

No runtime code, installer configuration, app settings, or releases were changed during this research.

**Follow-up: alternatives if custom capture is acceptable**

DXGI is the straightforward supported display-capture solution, not the only technically possible capture mechanism. Genuine border-free game-only capture is possible through graphics hooks; experimental compositor access also merits investigation if maintaining no injection is important.

| Approach | What it offers | What remains to solve |
|---|---|---|
| Custom graphics hook | Capture the game's rendered frames before desktop composition; avoids WGC and its border. | Code inside the game process, graphics-API coverage, shared-texture synchronization, crash isolation, signing, and game/anti-cheat compatibility. This changes Clipline's existing no-injection policy. |
| Experimental DWM shared-surface capture | Read a particular window's compositor surface without using WGC or injecting into the game. | An undocumented interface with no application compatibility guarantee; test whether each target game exposes a usable surface, including fullscreen/flip modes, occlusion, resize, and hybrid GPUs. |
| BitBlt / PrintWindow | Older window-capture paths usable for some applications. | Application/rendering compatibility and performance; not established as a universal modern game-recording backend. |
| DXGI with a tracked game rectangle | Border-free display capture cropped to follow the game. | Cropping does not isolate the underlying game from overlapping windows. |

OBS documents direct DirectX/OpenGL game capture and an anti-cheat compatibility hook. Its June 2025 certificate-transition test matrix shows Vanguard and FACEIT working with the new dual-signed hook after the previous certificate configuration failed. Therefore the repository's categorical claim that injection is blocked by these systems is too strong. This historical result does not confer approval or compatibility on a new Clipline hook. [Game capture](https://obsproject.com/kb/game-capture-source), [certificate compatibility results](https://obsproject.com/kb/capture-hook-certificate-update)

For a custom hook, start with the graphics API used by the target games and feed copied GPU frames into the existing encoder/buffer path. Do not replace the whole recorder. Validate with vendors' permitted integration paths; signing alone is not evidence of acceptance. OBS also lists games where hook capture is unavailable and recommends other capture methods. [OBS troubleshooting](https://obsproject.com/kb/game-capture-troubleshooting)

There is concrete prior implementation evidence for the DWM route: BetterGI's `SharedSurfaceCapture` uses `user32!DwmGetDxSharedSurface`, opens the shared D3D surface, and reads frames. That establishes an implemented technique, not a tested Clipline solution or broad game support. The separately named Microsoft-documented `DwmDxGetWindowSharedSurface` is a driver/runtime interface whose documentation is restricted to Windows 7; it must not be confused with a supported Windows 10 application capture contract. [Implementation evidence](https://github.com/babalae/better-genshin-impact/blob/main/Fischless.GameCapture/DwmSharedSurface/SharedSurfaceCapture.cs), [Microsoft's distinct driver interface](https://learn.microsoft.com/en-us/windows/win32/dwm/dwmdxgetwindowsharedsurface)

If the desired combination is game-only, border-free, and no injection, the next research step is a bounded independent DWM prototype on Windows 10 22H2 with the actual target games. Its acceptance criteria should include moving frames while another window overlaps the game, no WGC session/border, resize and focus transitions, correct colors, stable memory, and measured capture cost at the target FPS. Treat minimization and exclusive fullscreen as separate cases rather than assuming continuous rendering. Respect capture exclusions. Stop if the games do not expose usable surfaces; a working screenshot of a simple window is insufficient. No such prototype or physical Windows 10 validation has been run yet, and no third-party capture code has been copied into Clipline.

BitBlt is a legitimate OBS option but documented as less efficient and application dependent. PrintWindow is synchronous and relies on the owning application to render into a device context. These are fallback candidates to measure, not evidence of a general high-FPS game solution. [OBS window capture](https://obsproject.com/kb/window-capture-sources), [PrintWindow contract](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-printwindow)

Two other apparent shortcuts do not establish a general solution: Microsoft's `CreateFromVisual` captures visuals owned by the calling application, and NVIDIA deprecated NvFBC on Windows 10 and later, with 1803 as its last supported Windows 10 version. [Owned visual capture](https://blogs.windows.com/windowsdeveloper/2019/09/16/new-ways-to-do-screen-capture/), [NVIDIA Capture SDK](https://developer.nvidia.com/capture-sdk)

**Prototype follow-up (user authorized implementation)**

The isolated `dwm_probe` example now exists; [tester instructions](../dwm-probe.md) describe its use. The branch was fast-forwarded to the inspected 1.0.4 commit before implementation. A controlled Windows 11 26200 / RX 6700 XT experiment returned changing target-window pixels, survived resize and minimize/restore, and saved target content while a magenta window covered it. The 22-second run had 973 successful reads, 859 changed samples, and 176 minimized-window errors; the 8-second overlap run had 368 reads, 266 changes, and no errors. Sampling cost includes CPU readback/BMP packing, and compilation overlapped the second run. These are feasibility observations, not game or performance acceptance. Windows 10 and actual games are still untested.

The prototype does not use WGC or injection and is not wired into the recorder. It does not establish synchronized complete-frame reads: the initial unbuffered GDI fixture produced a snapshot with partial repaint content, and equal opaque update IDs sometimes accompanied different pixel hashes. The fixture now double-buffers; robust game rendering and synchronization still need investigation. No third-party implementation was copied. Workspace tests and the probe's argument/BMP checks pass; unsafe/lifetime review found no experimental-shipping blocker.

**September 8 physical Windows 10 follow-up:** the original probe package failed to
load because it omitted `vcruntime140.dll`. Official Microsoft app-local deployment
repairs startup without changing the executable. Controlled overlap and lifecycle
capture, including a five-minute resource sample, works on Microsoft Basic Render
Driver; the AMD graphics driver is not installed. Hardware-accelerated game capture
remains unverified. See the [Windows 10 validation record](2026-09-08-dwm-probe-win10-validation.md)
for measurements, packaging fixes, and remaining driver/game prerequisites.
