# Experimental DWM window capture

**Windows 10 AMD validation:** the controlled fixture works, but AMD-accelerated
WebGL returns unchanged white/gray surfaces in windowed and browser fullscreen
tests. This probe is not ready for general window recording. See the
[AMD recording results](research/2026-09-08-win10-amd-recording.md).
The [native mock-game matrix](research/2026-09-08-mock-game-validation.md) now
reproduces a presentation distinction: blt windowed/borderless captures correctly,
while flip presentation and DXGI exclusive modes return stale surfaces. Build the
[mock executables](mock-games.md) to reproduce without a real-game installation.
The [blocker investigation](research/2026-09-08-window-capture-blockers.md) identifies
an alternative full-content PrintWindow path that captures the flip mock in
windowed/borderless modes. Exclusive fullscreen still fails. It remains a separate
diagnostic, with no change to production recording.

The [exclusive investigation](research/2026-09-08-exclusive-fullscreen.md) repairs
production DXGI display recovery and validates explicit monitor recordings through
fullscreen transitions. It does not repair exclusive DWM/PrintWindow window capture
or introduce a monitor fallback for game-window sources.

The [classified native-exclusive followup](research/2026-09-08-dwm-native-exclusive-followup.md)
now confirms static direct-DWM pixels under Legacy Flip. A thumbnail-host plus
PrintWindow combination works in the windowed control but returns stale
pre-transition game content under the same measured native-exclusive mode.

This standalone probe tests border-free window capture using an undocumented DWM surface
export. It does not change Clipline, inject into the target, run WGC, or fall back to display
capture. Use it only on a window you intend to record. Snapshots and window titles stay local.

From an extracted probe ZIP, open PowerShell in that directory:

```powershell
.\dwm_probe.exe --help
.\dwm_probe.exe --list
.\dwm_probe.exe --hwnd 123456 --seconds 30 --fps 60 --out league-test
```

For an intentionally animated target, add `--require-motion`. This returns a
nonzero exit after saving evidence if every readable sample is identical. The
controlled target runner now uses it. Without that flag static-image sampling is
still allowed. A passing motion check does not establish sustained freshness or
frame synchronization.

Check `$LASTEXITCODE` immediately after each native command. Both preflight commands
must exit zero before starting capture. `-1073741515` / `0xC0000135` means a required
DLL could not be loaded; it is **not** a DWM capture failure. The original test ZIP
omitted `vcruntime140.dll`, which reproduced this failure on clean Windows 10.

The package must include the official Microsoft x64 `vcruntime140.dll` beside
`dwm_probe.exe`, or the machine must have the appropriate Microsoft Visual C++
Redistributable installed. Use a runtime at least as recent as the build toolset.
App-local deployment keeps this test independent of a global runtime installation.
Do not obtain DLLs from third-party download sites. Microsoft documents both
[runtime deployment](https://learn.microsoft.com/en-us/cpp/windows/redistributing-visual-cpp-files)
and [official redistributable downloads](https://learn.microsoft.com/en-us/cpp/windows/latest-supported-vc-redist).
The packager checks x64 architecture, original DLL name, Microsoft signature, and
actual `--help` / `--list` exit codes; it records runtime provenance and hashes.
Its preflight logs are a sibling directory, outside the distributable, because window
enumeration can contain private desktop titles.

For an automated controlled run from the extracted package:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\test-dwm-probe.ps1 -OutputDirectory .\fixture-test
```

This starts the fixture only after successful loader/enumeration checks, preserves
native exit codes and output (including on timeout), and fails immediately when the
probe cannot run. A failed probe must never be reported as a missing target window.
The execution-policy option applies only to that PowerShell process.

Replace `123456` with the first-column HWND for the actual game window. Alternatively use
`--window "unique title"`; ambiguous matches fail instead of choosing an arbitrary window.
Output directories must be new. `--help` describes limits and defaults. Stop with Ctrl+C if
needed (a completed summary is not guaranteed after interruption).

For each Windows 10 22H2 game test:

1. Keep the game animating. Check there is no yellow capture border from this probe.
2. Cover part of the game with another window around halfway through the run. Inspect
   `middle.bmp`: it should show the game, not the overlapping window, with current motion.
3. Repeat for windowed and borderless fullscreen, then resize, minimize/restore, and move to
   another monitor. Test exclusive fullscreen separately; a minimized game may stop rendering.

Inspect `first.bmp`, `middle.bmp`, and `last.bmp` for correct colors, fresh content, and
partial/black frames. `samples.csv` records read time, dimensions, format, opaque DWM update
IDs, resource changes, pixel hashes, and errors. `summary.txt` aggregates results. Record
Windows edition/build, GPU/driver, game, render mode, and actual on-screen behavior alongside
the files. Review snapshots before sharing them.

The probe supports 32-bit SDR surfaces only; HDR, unusual layouts, protected windows, failed
capture-affinity checks, and keyed-mutex surfaces are rejected. Full compositor-surface
snapshots can include window padding/non-client pixels; game-client cropping is not implemented.
Matching adapter selection is implemented but hybrid-GPU behavior still needs live validation.

**Read rate is not game FPS.** Polling can return duplicate/stale frames. Changed hashes include
dimension changes and do not prove tear-free capture. `read_ms` includes surface acquisition,
GPU-to-CPU copying, and BMP packing; CPU hashing and disk writes also affect total sampling rate.
This intentionally uses CPU readback to inspect pixels. No encoder integration or producer
synchronization guarantee is claimed. Map waits for the probe's copy, not a documented DWM
frame boundary. Do not promote this to the recording path on screenshot evidence alone.

Build and run from the repository:

```powershell
cargo test -p clipline-capture --example dwm_probe
cargo build -p clipline-capture --release --example dwm_probe
.\target\release\examples\dwm_probe.exe --list
```

Stage a tester package with a redistributable DLL from Microsoft's documented
Visual Studio VC Redist directory (x64, release runtime, matching or newer toolset).
Set `$runtimeDll` to that DLL's full path and `$runtimeSource` to its source/version:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File scripts/package-dwm-probe.ps1 -ProbePath target/release/examples/dwm_probe.exe -RuntimePath $runtimeDll -RuntimeSource $runtimeSource -Destination dwm-probe-x64
Compress-Archive -Path dwm-probe-x64/* -DestinationPath dwm-probe-x64.zip
powershell -NoProfile -ExecutionPolicy Bypass -File scripts/tests/test-dwm-probe-runner.ps1
```

Destination and output directories must be new. The source build on a development
machine does not test clean-machine loader dependencies. Do not use `crt-static`
alone as a repair: the attempted static-CRT link still failed on vendored Opus
imports `__imp_realloc` and `__imp__wassert`.

For a controlled local fixture, run `powershell -NoProfile -File scripts/dwm-probe-target.ps1`
in another terminal, then capture `--window "Clipline DWM Probe Target"`. The fixture animates
a green square with red/blue reference blocks, covers it with a magenta window at 4 seconds,
resizes at 12 seconds, minimizes at 15, restores at 18, and closes at 28. An 8-second probe
started promptly exercises overlap; a 22-second run exercises resize/minimize/recovery.

The ZIP also includes that fixture at its root: run
`powershell -NoProfile -File .\dwm-probe-target.ps1` from the extracted directory.

Local release smoke result (Windows 11 26200, RX 6700 XT, controlled SDR window): 467 readable
samples in 8 seconds, 305 changed hashes, no errors, 58.36 reads/s, mean probe read cost 1.77 ms.
The saved overlap image showed the target and excluded the magenta covering window. Working
set after startup was about 36 MB during this short run; this is not a memory soak or game FPS
measurement. Windows 10 and game acceptance remain pending.

Research and API limitations: [Windows 10 report](research/2026-09-07-windows-10-support.md).

Physical Windows 10 findings and remaining prerequisites:
[September 8 validation](research/2026-09-08-dwm-probe-win10-validation.md).

Latest display-control aspect fix and exclusive presentation-classification
prerequisite: [fullscreen aspect report](research/2026-09-08-fullscreen-aspect.md).
This display recording result does not establish game-only DWM compatibility.
