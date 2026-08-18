# Windows Nightly Runner Benchmark

> **For agentic workers:** Execute this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for
> tracking and remain unticked by repository convention.

**Goal:** Measure Windows nightly release wall time across GitHub, Namespace, Depot, and
Blacksmith runners on one commit, then promote only optimizations supported by repeated results.

**Baseline:** Nightly run `31984119935` took 43m22s end to end. Its cold cache miss led to 6m31s
of test compilation, 3m11s of Clippy, 9m50s compiling Tauri CLI, 8m50s compiling the regular
release, 12s regular NSIS, 2m44s recompiling standalone, 7m25s standalone NSIS, and 2m35s in the
post-job Rust cache step (including a 1,568,419,534-byte upload).

## Task 1: Add a provider-neutral benchmark harness

- [ ] Add a manually dispatched workflow that requires an exact 40-character commit SHA and
      dynamically includes only configured provider runner labels.
- [ ] Keep checkout, stable Rust + Clippy, tests, warning-denied Clippy, pinned Tauri CLI, both
      Tauri builds, runtime verification/staging, signing, updater assets, and artifact upload
      equivalent to the release build without publishing a GitHub release.
- [ ] Support isolated cache epochs and explicit cold/warm repetitions without deleting or
      contaminating production release caches.
- [ ] Record CPU, RAM, disk, command wall times, cache behavior, sccache statistics when selected,
      staged resource/input/output sizes, and makensis CPU/wall time in JSON.
- [ ] Upload raw JSON and render a compact job summary; add an aggregation job that includes
      checkout/setup/cache/post-job/total timings from the GitHub Actions jobs API.

## Task 2: Compare cache strategies independently

- [ ] Benchmark the current full-target `Swatinem/rust-cache` behavior.
- [ ] Benchmark dependency/tool caching without `target/` and a separately cached pinned Tauri
      CLI binary.
- [ ] Benchmark sccache without also restoring a full `target/`; record hit/miss and cache size.
- [ ] Measure restore and save overhead for every cache path and keep provider/strategy/epoch keys
      isolated.

## Task 3: Measure build and packaging bottlenecks

- [ ] Record the exact merged differences between `tauri.conf.json` and
      `tauri.standalone.conf.json` and verify whether Tauri can bundle twice from one preserved
      executable without changing embedded configuration, updater selection, or signatures.
- [ ] Measure standalone resource file count/bytes, makensis wall/CPU time, compression settings,
      and installer bytes; compare any compression change before proposing it for release.
- [ ] Compare serial tests + Clippy against parallel jobs only if the parallel experiment accounts
      for duplicated compilation and cache traffic.

## Task 4: Report before changing production

- [ ] Run one cold-ish seed and at least two warm repetitions per configured provider on the same
      commit; report medians/ranges and avoid conclusions from single samples.
- [ ] Document provider setup variables, baseline evidence, measured results, rejected shortcuts,
      and an optimized nightly architecture with per-change wall-clock estimates.
- [ ] Keep benchmark instrumentation and any proven production optimization in separate commits.
- [ ] Run workspace tests and warning-denied Clippy; update `handoff.md` for any production change.
