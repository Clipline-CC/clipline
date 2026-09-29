# Nonexperimental capture performance audit

The user reports lower **in-game FPS** on Windows 11 using Automatic/WGC after
updating. The experimental backend is not selected. They play League of Legends
on a Radeon RX 6700 XT; quitting/reopening Clipline restores FPS, and they do not
think it drops again afterward. Whether the same recording/encoder resumed is
unverified. The local validation host is Windows 10 build 19045, Ryzen 9 7940HS /
Radeon 780M, driver 32.0.31041.1004. These are different environments.

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

In the app this converter is reached only through `EncoderBackend::MfSoftware`,
the inbox last resort, via Software MFT or FFmpeg's `MfSoftware` path. Other FFmpeg
backends, including x264/x265/SVT-AV1, use GPU conversion; hardware MFT also uses
GPU conversion. This usually concerns software-only adapters/VMs or unavailable
hardware encoding, not every software encoder or Auto/WGC user. Capture backend
selection alone does not identify the affected population.

The released converter samples each pixel separately for Y and UV and checks
destination bounds on every sample. Those early returns also affect optimization
of the coordinate divisions; the 39-40% local slowdown must not be attributed
solely to the cost of four comparisons. Reviewer measurements on another host
reported larger slowdowns. Neither magnitude is a universal calibration.

The revised fix uses safe row slices and one loop over complete 2x2 blocks. Four
source samples supply both the individual Y values and the averaged U/V value,
halving source sampling relative to the separate loops. Background values are
derived from the same Rec.709 functions (Y=16, U=V=128). The constructor rejects
misaligned fitted bounds and the private sampler has debug assertions. Crop,
scale, rounding, encoder selection and hardware paths retain their behavior.

We measured the reviewed 648f706 implementation, a row-slice variant and fused
2x2 conversion before choosing fusion. Fusion won in all five local shapes.
Prototype assembly inspection confirms that row slices eliminate the UV output-store
bounds checks seen in 648f706; input checks remain. Some row division is hoisted,
but not all, so do not claim complete division elimination. The inspected fused
variant's derived prefill compiles to two constant `memset` calls. The final
version uses fixed-size array chunks as required by Clippy; no full-coverage
prefill branch was added. Final timings below measure that version directly.

The committed runner's exact final-source comparison:

| Input -> output | 1.0.5 median ms/frame | Revised median ms/frame | Median paired ratio |
|---|---:|---:|---:|
| 1920x1080 -> 1920x1080 | 12.214 | 6.656 | 0.545 |
| 5120x1440 -> 1920x540 | 6.255 | 3.387 | 0.542 |
| 5120x1440 -> 2560x720 | 11.105 | 6.021 | 0.543 |
| 1920x1080 -> 1920x1200 (letterbox) | 12.472 | 6.728 | 0.539 |
| 1080x1920 -> 1920x1080 (pillarbox) | 6.669 | 2.483 | 0.372 |

These are roughly 45-63% lower conversion times on this host, **not game FPS
gains**. A separate exact-final-source comparison against 1.0.4 measured paired
ratios 0.725, 0.719 and 0.714 for the three matching-aspect shapes (about 27-29%
less time). Do not compare 1.0.4's bars-case timing as equal work: that converter
stretched instead of fitting. The initial 648f706 revision
measured 1.017-1.024x 1.0.4 locally versus reviewer-reported 1.07-1.10x elsewhere;
those earlier numbers are superseded, not evidence of universal near-parity.

Method: rustc 1.98.1 / LLVM 22.1.8, x86_64-pc-windows-msvc, opt-level 3,
codegen-units 1, generic x86-64 target. Standalone source copies adapt only
`thiserror` metadata and module paths. Deterministic varied BGRA input, `black_box`,
8 warmups, 12 alternating-order rounds of 16 conversions; allocation/free included,
GPU readback and encoding excluded. No concurrent cargo/GPU benchmark was started;
ordinary background load was not controlled. Hardware, harness and compiler
code generation affect timings. No timing threshold is added to CI.

Reproduce from the repo with git history and rustc available:

```powershell
powershell -ExecutionPolicy Bypass -File scripts/benchmark-cpu-conversion.ps1
```

The runner snapshots released 1.0.5 and working sources, records compiler/source
metadata, verifies deterministic size/crop/stride cases, then saves CSV timings.
`-VerifyOnly` skips timings. Its 50,000 cases produced 41,749 byte-equal outputs
and 8,251 matching constructor rejections, including odd/1px inputs, random crops
and 0/7/14-byte row padding with no trailing last-row padding. The same final
snapshot passes all 50,000 cases with debug assertions enabled. Independent
fused-prototype verification also passed 216 fixed cases plus 32,768 accepted
randomized cases and 2,257 matching rejections.

Durable tests now check red content chroma placement against neutral bars in both
letterbox and pillarbox outputs, mixed-pixel Y and averaged UV, and a width-limited
fit whose unrounded height is odd (715). The new tests detect both the reported
content-relative UV-row mutant and removal of the height mask. The original
white-only bars test did not cover chroma placement; that gap is now closed.

Local gates: 1,548 workspace tests pass (two explicitly ignored tests), as do fresh
capture-cache workspace/all-targets Clippy with warnings denied, scoped rustfmt
2024-style checks, and `git diff --check`. All eight CPU converter tests pass.

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
`CPU_AUDIT.md` and adjacent sources/results preserve the superseded CPU comparisons;
`gpu-ab.csv` and `gpu/` preserve the GPU comparison. Revision evidence is under
`C:\Users\Dain\Desktop\CliplineCpuReview-20260911`: `REVIEW.md`, `final-*`,
`final-mutants.log`, `array-old-*` and `verified-head-run/`. The final benchmarked CPU source
SHA256 is `7A8811143725BAF7F1AF779F49817CD2ADCE0313CC789546D4063D059C75F86C`.

The CPU fix is a separate PR from #201. The user's Windows 11 in-game slowdown
remains unconfirmed. Obtain paired support reports while FPS is low and after
reopening when FPS is normal and recording resumes, to compare actual encoder,
capture state and errors. A same-scene 1.0.4/1.0.5 comparison may then help.
Do not label this CPU fix as the solution to their report without that evidence.
