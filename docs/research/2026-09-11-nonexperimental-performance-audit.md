# Nonexperimental capture performance audit

The user reports lower **in-game FPS** on Windows 11 using Automatic/WGC after
updating. The experimental backend is not selected. Their GPU, game, actual
encoder and recording-on/off comparison are still unknown. The local validation
host is Windows 10 build 19045, Ryzen 9 7940HS / Radeon 780M, driver
32.0.31041.1004. These are different environments.

## Scope and isolation

Compared exact released commits:

- Nightly 1.0.4: `c455423898fc227f0a2dba8fae41b199827d871c`.
- Nightly 1.0.5: `a9a214eeb15f3597920fc4657d98e5cf62f56fd1`.
- Separately inspected unmerged PR #201 at `78eaa1053ce25b31c7b97a432706f963e87a3ee7`.

| Area | Finding |
|---|---|
| Backend routing | Auto/WGC opens WGC; only `ExperimentalHybrid` opens the hybrid backend and its PrintWindow worker. |
| Persisted settings | Auto remains the default. Missing/unknown backend values become Auto; existing selections remain. No migration enables hybrid. |
| Capture cadence | New cadence waits and eager-frame fallback apply only to hybrid. WGC/DD retain the prior timeout behavior, with an extra clock read/branch. |
| Recurring UI work | No new timers, capture workers or polling for Auto/WGC. Existing status events add a backend label. |
| WGC and encoder selection | WGC implementation, encoder selection/dimension code and app runtime have identical Git blobs across the two releases. |
| Dependencies | Cargo.lock changes only the app version. FFmpeg pin and WebView2 CAB/version/hash are identical; WebView2 review dates changed. |
| Shared released changes | CPU/GPU color conversion changed. Explicit DD lost its WGC fallback and gained strict window rejection; display selection also changed. These are not wholly isolated behind the experimental setting. |
| PR #201 | Canvas selection is hybrid-only. Its only shared runtime addition is four dimension fields in the existing once-per-session encoder log. This unmerged fix is absent from installed 1.0.5. |

The experimental worker and policy are isolated, but a blanket claim that the
entire 1.0.5 change cannot affect other users would be incorrect. Three audit
passes covered routing/settings/cadence, shared conversion, and measured behavior.

## Reproduced CPU regression and fix

The released CPU converter checks the destination bounds in `source_pixel` for
every luma/chroma sample, approximately twice per output pixel. This added a
measurable cost even when source and output have matching aspect ratios.
`SoftwareMftH264Encoder` and FFmpeg's `MfSoftware` path use this converter, including
when capture uses WGC. Hardware encoder paths use the GPU converter instead.

The fix initializes the NV12 background to limited-range black (Y=16, U=V=128)
and converts only the fitted content rectangle. Its even origin and dimensions
keep every 2x2 chroma block entirely inside content or background, allowing removal
of the repeated clipping guard. Crop, scale, color math, input validation and
encoder selection are unchanged.

Optimized, alternating comparisons of exact source copies:

| Input -> output | 1.0.5 / 1.0.4 median paired duration | Fixed / 1.0.4 median paired duration |
|---|---:|---:|
| 1920x1080 -> 1920x1080 | 1.402 | 1.024 |
| 5120x1440 -> 1920x540 | 1.389 | 1.020 |
| 5120x1440 -> 2560x720 | 1.400 | 1.017 |

These are separate alternating benchmark runs; each ratio compares paired rounds
within its own run. The fix removes most of the observed 39-40% conversion slowdown.
This percentage is **not** a measured game FPS change.

Method: rustc 1.98.1 / LLVM 22.1.8, x86_64-pc-windows-msvc, opt-level 3,
codegen-units 1, generic x86-64 target. Source copies remove only `thiserror`
metadata for standalone compilation. Deterministic varied BGRA input, `black_box`,
8 warmups, 12 alternating-order rounds of 16 conversions; allocation/free included,
GPU readback and encoding excluded. No concurrent cargo/GPU benchmark was started;
ordinary background load was not controlled. No timing threshold is added to CI.

The actual implementation matches 1.0.5 bytes or constructor rejection across
216 size/crop/stride combinations, including tiny/odd/portrait sources and padded
rows. Existing CPU correctness tests cover colors, black bars, cropping, pitch
and invalid inputs. Independent review found no correctness issue.

Local gates: all 1,546 workspace tests pass (two explicitly ignored tests), and
workspace/all-targets Clippy passes with warnings denied after cleaning the
capture crate cache. Scoped rustfmt with style edition 2024 and `git diff --check`
pass. The six focused CPU converter tests also pass.

## GPU comparison

The shared GPU converter gained per-frame layout/background state calls in 1.0.5.
An exact-source comparison on the local Radeon 780M did not show a consistent
slowdown:

| Input -> output | 1.0.4 mean ms/frame | 1.0.5 mean ms/frame | Ratio |
|---|---:|---:|---:|
| 1920x1080 -> 1920x1080 | 0.2664 | 0.2592 | 0.973 |
| 5120x1440 -> 1920x540 | 0.5515 | 0.5558 | 1.008 |
| 5120x1440 -> 2560x720 | 0.5950 | 0.5941 | 0.998 |

Eight alternating rounds, 20 warmups, 300 conversions per implementation per round
in five batches; the last texture in each batch is mapped for readback to wait
for GPU completion. Timings include conversion resource/view creation and five
readbacks per 300 frames. The hardware D3D11 device uses an initialized static
gray BGRA texture; old/new output bytes match. The unchanged D3D11 helper required
only a visibility adjustment for the standalone harness. Build uses opt-level 3
and the repository's Windows bindings. No GPU production code was changed.

This is not WGC acquisition plus encoding, a game workload, GPU-saturation testing,
or a Windows 11/other-driver test. It does not establish that the user's hardware
path is unaffected. DD also has additional per-frame validation; it is not the
reported backend and no sustained DD performance defect is established here.

## Evidence and remaining diagnosis

Local evidence:
`C:\Users\Dain\Desktop\CliplineNonexperimentalAudit-20260911`.
`CPU_AUDIT.md`, `cpu-ab.csv`, `cpu-final-ab.csv`, `cpu-final-verify.log` and their
source/harness copies preserve the CPU comparisons. `gpu-ab.csv` and `gpu/`
preserve the GPU comparison. The final benchmarked CPU source SHA256 is
`7FF380C2D0AEA959E0540F0A3287D63DFE58B033B3D4F849CAF5B5B25F13E848`.

The CPU fix is a separate PR from #201. The user's Windows 11 in-game slowdown
remains unconfirmed: obtain their GPU/game, selected and actual encoder, recording
resolution/FPS, and FPS with recording active, paused and Clipline fully exited.
A same-scene 1.0.4/1.0.5 comparison is useful if those observations implicate
Clipline. Do not label this CPU fix as the solution to their report without that
evidence.
