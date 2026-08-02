# Slint presentation spike protocol

This protocol fixes the Milestone 4 evidence boundary. The spike is a reversible presentation
experiment, not a distributed replacement frontend. A result is accepted only when the sampler,
Slint adapter, playback session, and presentation telemetry all agree on readiness and teardown.

## Build and presentation identity

The spike pins Slint `1.17.1` with exactly these features:

- `accessibility`
- `backend-winit`
- `compat-1-2`
- `raw-window-handle-06`
- `renderer-software`
- `std`
- `system-tray`

Slint renders the application chrome with `winit-software`. Video has a separately declared path:

- `d3d11-child-window` is the production-candidate path. A child HWND, flip-model swap chain, and
  D3D11 video processor present playback-owned NV12 surfaces without a CPU readback.
- `cpu-shared-pixel-buffer-diagnostic` is opt-in diagnostic evidence only. It performs bounded
  D3D staging readback and Rec. 709 NV12-to-RGB conversion into one reusable RGB buffer, then keeps
  at most one Slint UI delivery outstanding. It is not an automatic fallback and cannot justify a
  cutover decision.

The program fails closed when the requested D3D path cannot be created. It must never silently
switch to CPU presentation. Final telemetry records the exact Slint version/features, chrome
renderer, video presentation path, swap-chain or CPU-mailbox counters, decoder acceleration and
adapter LUID, audio endpoint, ownership counters, and fixed buffer capacities.

Formal measurements use the spike's dedicated optimized profile and probe:

```powershell
cargo build --manifest-path apps/clipline-slint-spike/Cargo.toml --profile benchmark
apps/clipline-slint-spike/target/benchmark/clipline-slint-spike.exe --clipline-benchmark-probe
```

The probe must report optimization, debug assertions, and no autostart-registry mutation. Gate runs
pass `-RequireBenchmarkBuild`; the sampler rejects any other profile or an unsafe/invalid probe.

## Corpus

The matched presentation fixture is
`fixtures/playback/hybrid-writer-h264-two-opus-5s.mp4`, SHA-256
`8a32e046402aa5a6e7a1fce05a747d3705dc1a7dc868d08a8cc18573c0dd2a71`. It is listed in
`manifest.json` under `production_mux_oracles` and is hash-verified before launch. It retains the
encoded H.264 High and two Opus streams from the procedural first-party corpus but was remuxed by
`HybridMp4Writer` without FFmpeg's foreign mid-sample edit lists. The native index deliberately
rejects those edit lists; the sampler therefore uses the production-writer oracle by default for
Slint review scenarios.

Matched Tauri comparisons must pass the same production-writer path explicitly. Using the older
FFmpeg-muxed default for Tauri and the production-writer file for Slint is not matched evidence.

## Sampler and adapter contract

Use `scripts/measure-frontend-baseline.ps1` with `-Frontend slint` and
`-AdapterScript scripts/drive-slint-spike.ps1`. The sampler:

1. validates the selected file against either the manifest's procedural fixtures or its production
   mux oracles;
2. creates a disposable profile and records the entire fixture directory's hashes;
3. launches the spike with explicit fixture, scenario, readiness marker, stop, and final-telemetry
   paths;
4. verifies the root PID, process name, and creation time before accepting readiness or samples;
5. samples the creation-time-validated process tree with the frontend-neutral metric schema;
6. creates the stop file and requires clean process exit plus final telemetry; and
7. writes raw CSV and metadata only when all preceding checks succeed.

The Slint adapter accepts only the app's semantic `ready` marker. An `error` marker, changed root
identity, premature process exit, missing final telemetry, or failure to exit within the bounded
shutdown window rejects the run. The adapter does not infer readiness from elapsed time or window
existence.

Supported spike scenarios are `review-idle`, `review-playing`, `scrub-storm`, and
`reveal-close-100`. Broader library/settings/tray parity belongs to later milestones and is rejected
rather than simulated here.

Example diagnostic smoke:

```powershell
./scripts/measure-frontend-baseline.ps1 `
  -Exe apps/clipline-slint-spike/target/debug/clipline-slint-spike.exe `
  -Frontend slint `
  -Renderer winit-software-cpu-diagnostic `
  -Scenario review-playing `
  -FixturesDir fixtures/playback `
  -AdapterScript scripts/drive-slint-spike.ps1 `
  -WarmupSeconds 0 `
  -SteadySeconds 15 `
  -OutputDirectory artifacts/slint-presentation-smoke
```

Debug smoke output proves lifecycle and telemetry plumbing only. Formal memory/CPU comparisons must
use optimized, source-identical builds for both frontends and the full environment protocol in
`docs/slint/baseline-protocol.md`.

## Acceptance gates

The spike can pass only when all applicable evidence is accepted:

- p95 process-tree private working set is no more than 190 MiB;
- p95 A/V error is no more than 40 ms;
- late-or-dropped frames are below 0.5% of decoded eligible frames;
- p95 exact seek settlement is no more than 150 ms;
- MFT receives/releases remain balanced and all documented packet, queue, surface, swap-chain, and
  UI-mailbox bounds remain fixed;
- no handle, thread, or capacity growth occurs over 100 reveal/close and seek cycles;
- close releases playback, presentation, audio, decoder, and source-file ownership in order;
- resize, DPI change, minimize, occlusion, restore, adapter selection, and device recovery are
  correct on a real Direct3D-capable console session; and
- p95 PWS is at most 80% of matched Tauri and CPU is within one percentage point under the same
  scenario, corpus, machine, window state, and alternating run order.

Formal evidence requires at least three accepted five-minute `review-playing` runs and three
matched `scrub-storm` runs, plus the lifecycle/DPI matrix, 100-cycle run, and Milestone 3 hardware
media gates. Missing environment evidence stays pending; it is neither a pass nor a failure.

## 2026-08-02 local diagnostic evidence

The local console exposed Windows 11 Pro build 26200 and Microsoft Basic Display Adapter driver
10.0.26100.1. The functional smoke executable was a debug build with SHA-256
`6beb756579d5bd1eb615d01017faf9815c85557e5d01d60c3a4a429f94675400` from commit
`fac9d51191219c8de7696e7d382147b6de132e6c` plus the uncommitted Task 8 harness changes.

The complete sampler/adapter/stop/telemetry path accepted these CPU-diagnostic smokes:

| Run | Semantic result | Ownership | One-second diagnostic p95 PWS |
|---|---|---:|---:|
| `20260802T091120Z-slint-review-playing` | playback advanced; clean `Closed` | 71/71 MFT samples | 17.3 MiB |
| `20260802T091206Z-slint-scrub-storm` | ten seeks settled; clean `Closed` | 272/272 MFT samples | 18.1 MiB |
| `20260802T091211Z-slint-reveal-close-100` | 100 hide/reveal cycles; clean `Closed` | 1/1 MFT samples | 16.4 MiB |

Every run reported one RGB allocation, UI pending high-water one, no readback mailbox
backpressure, no stale result, and no late/drop. These short debug samples are diagnostic only and
do not pass the absolute or relative performance gates.

After the fail-closed p95/overflow and fixed-bound fields were added, final schema smoke
`20260802T092518Z-slint-scrub-storm` used harness 1.1.0 and executable SHA-256
`e58cb33852c68ac1c9a40b4948a78764e4485229d77349a8c3f80fb0e65f1563`. It closed with 273/273
MFT receives/releases, 0 ms p95 A/V error, 29 ms p95 seek settlement across 32 samples, no
histogram overflow, zero late/drop, and 17.3 MiB one-second debug p95 PWS. It also reported the
two-surface decoder pool and 64-update session bound from exported constants. This remains a short
debug diagnostic rather than an accepted gate run.

Optimized-profile plumbing smoke `20260802T093832Z-slint-scrub-storm` used executable SHA-256
`cdf56707a59e8bba34ace7f8388839b8105b292f027c8dc7825e86e02db7186e` with
`-RequireBenchmarkBuild`. The embedded probe reported opt-level 3, debug assertions, and no
autostart-registry mutation; the session closed cleanly with 273/273 MFT ownership, 0 ms p95 A/V
error, and 34 ms p95 seek settlement across 32 samples. Its 18.2 MiB one-second p95 PWS is still a
short smoke, not an accepted performance sample.

The D3D child smoke was rejected before readiness with
`query D3D11 video-processor device: No such interface supported (0x80004002)`. This is the expected
fail-closed result on Microsoft Basic Display Adapter and proves there was no silent CPU fallback.
It does not validate D3D presentation or constitute a correctness failure on the target hardware.

## Decision

The implementation, CPU diagnostic lifecycle, bounds, semantic marker path, and clean teardown are
green enough to retain the non-distributed spike and proceed with reversible controller extraction.
The D3D fast path, hardware decode, DPI/device matrix, formal three-plus-three runs, 1080p60 hardware
media gates, and matched Tauri PWS/CPU comparison remain **pending**. They require a quiet console
session with a real GPU. The matched Tauri runs additionally require the unrelated installed
Clipline process to be closed by the user; the harness must not stop it.
