# Windows Nightly CI Benchmark

This document separates observed evidence from experiments that still need repeated provider runs.
The production `.github/workflows/nightly.yml` is unchanged while those experiments run.

## Baseline: Nightly 1.0.2

[Run 31984119935](https://github.com/Clipline-CC/clipline/actions/runs/31984119935) built commit
`4e7f2f483f98be7cc777825af4519e2b21f92baa` on GitHub `windows-latest` in 43m22s from run creation
through public-byte verification. The build job occupied a runner for 42m44s; the transactional
publish and public verification job added 32s.

| Phase | Wall time | Share of end-to-end |
|---|---:|---:|
| Queue / runner startup | 3s | 0.1% |
| Checkout + Rust setup + cache lookup | 18s | 0.7% |
| Tag/version/ancestry checks | 6s | 0.2% |
| `cargo test --workspace` | 6m50s (compile reported 6m31s) | 15.8% |
| warning-denied Clippy | 3m13s | 7.4% |
| compile/install Tauri CLI 2.11.2 | 9m53s | 22.8% |
| regular Tauri compile + NSIS + updater signature | 9m12s | 21.2% |
| stage and verify fixed runtimes | 9s | 0.3% |
| standalone compile + verify + NSIS + signature | 10m16s | 23.7% |
| prepare and upload seven verified assets | 4s | 0.2% |
| Rust cache cleanup/compress/upload | 2m35s | 6.0% |
| transactional publish + downloaded-byte verification | 32s | 1.2% |

The regular release compile was 8m50s and its `makensis` phase 13.09s. The standalone release
compile was 2m44s, pre-bundle verification 3.19s, `makensis` 445.31s, and updater signature 0.47s.

## Running the benchmark

The dispatcher is `.github/workflows/windows-nightly-benchmark.yml`. The reusable job runs the
same commit, commands, signing inputs, runtime provenance checks, two package configurations,
updater signing, seven-asset preparation, and artifact upload on every configured provider. It
does not publish or replace the rolling Nightly release.

Set runner labels as repository configuration variables. Labels are account configuration, not
secrets; leave an unavailable provider unset and its job is omitted.

| Variable | Value |
|---|---|
| `CI_BENCH_GITHUB_WINDOWS_RUNNER` | Optional; defaults to `windows-latest` |
| `CI_BENCH_NAMESPACE_WINDOWS_RUNNER` | The Namespace Windows profile/runner label from the account |
| `CI_BENCH_DEPOT_WINDOWS_RUNNER` | The Depot Windows runner label from the account |
| `CI_BENCH_BLACKSMITH_WINDOWS_RUNNER` | The Blacksmith Windows runner label from the account |

Do not copy example proprietary labels into the repository without confirming the enabled account
and runner size. Namespace, Depot, and Blacksmith require their GitHub organization/app connection
before their labels can accept jobs. See the official setup references:

- [Namespace GitHub Actions setup](https://namespace.so/docs/solutions/github-actions) and
  [runner configuration](https://namespace.so/docs/reference/github-actions/runner-configuration)
- [Depot GitHub Actions setup](https://depot.dev/docs/github-actions/quickstart) and
  [runner types](https://depot.dev/docs/github-actions/runner-types)
- [Blacksmith quickstart](https://docs.blacksmith.sh/introduction/quickstart) and
  [runner types](https://docs.blacksmith.sh/blacksmith-runners/overview)

For one comparable series:

1. Select one exact 40-character commit and use it for every dispatch.
2. Use a new `cache_epoch`, `expected_cache=cold`, and repetition `1` for the cold-ish seed.
3. Reuse that commit, ref, provider set, strategy, CLI mode, and epoch for repetitions `2` through
   `4`, marked `expected_cache=warm`.
4. Compare medians and ranges of the warm runs. A single result is descriptive, not a provider
   conclusion.
5. Change only `cache_strategy` or `tauri_cli` for cache/tool experiments. Change only provider
   labels for provider comparisons.

Branch pushes run the GitHub baseline automatically. `workflow_dispatch` is the repeatable path
once the workflow exists on the default branch. Every job uploads its signed-but-unpublished
release transaction and a small JSON timing artifact. The aggregate job queries Actions step
timestamps after post-job cache saving has finished, writes the comparison table to the job
summary, and uploads `windows-nightly-benchmark.json` for 90 days.

The JSON includes runner CPU model/logical CPUs, RAM, logical/physical disk data, action and command
wall times, cache hit/miss, cache-directory sizes, staged resource counts/bytes, installer bytes,
sccache statistics, and `makensis` wall/CPU/peak-memory samples. `run_to_job_start_seconds` is the
best available provisioning proxy and includes GitHub orchestration plus provider queue time.

### OS comparability caveat

There is currently no documented four-provider common Windows image: GitHub `windows-latest` and
Blacksmith use Windows Server 2025, Namespace documents Windows Server 2022, and Depot offers both.
Report the requested practical four-way result, then run controlled cohorts where accounts allow:

- 2025: GitHub `windows-2025`, Depot 2025, Blacksmith 2025.
- 2022: GitHub `windows-2022`, Depot 2022, Namespace 2022.

The captured image/build metadata prevents an OS difference from being mistaken for a provider
difference. GitHub's label/image behavior is documented in
[Choosing the runner for a job](https://docs.github.com/en/actions/how-tos/write-workflows/choose-where-workflows-run/choose-the-runner-for-a-job).

## Findings independent of provider

### Tauri CLI: use the official pinned binary

`Swatinem/rust-cache` can preserve `$CARGO_HOME/bin`, but the Nightly run had a complete cache miss.
Cargo also compiles `cargo install` packages in a temporary target directory by default, so the
workspace `target/` cache does not preserve the CLI's intermediate objects. More importantly,
GitHub cache access is separated by branch/tag: one Nightly version tag cannot restore a cache
created by a sibling Nightly tag. See [GitHub cache access restrictions](https://docs.github.com/en/actions/reference/workflows-and-actions/dependency-caching#restrictions-for-accessing-a-cache)
and [Cargo install behavior](https://doc.rust-lang.org/cargo/commands/cargo-install.html).

Tauri publishes an official `cargo-tauri-x86_64-pc-windows-msvc.zip` for 2.11.2. The harness pins
its URL, exact 7,414,116-byte size, release-provided SHA-256
`b6844470bcbf1da6e5dbf01990ae317d4d7969171628bb8badbdbff2e3d06d23`, and reported version.
Download, hash, extraction, and version verification took 1.85s locally, versus 9m50s of source
compilation in the baseline: a measured saving of about **9m48s**. The 7.4 MB archive is too small
to justify another cache. Sources: [Tauri CLI 2.11.2 release](https://github.com/tauri-apps/tauri/releases/tag/tauri-cli-v2.11.2)
and [release asset digest API](https://api.github.com/repos/tauri-apps/tauri/releases/tags/tauri-cli-v2.11.2).

### Rust caching: the current tag save is mostly wasted

Seven inspected recent release runs reported `No cache found`. At inspection time the repository
had 11 caches totaling 10,659,778,861 bytes; release-tag caches had already been evicted. GitHub's
default repository limit is 10 GB and its documentation explicitly calls repeated eviction above
the limit cache thrashing.

The baseline post-step spent about 24.8s cleaning, 106.8s creating the zstd archive, and 3.8s
uploading 1,568,419,534 bytes. Compression/scan, not network upload, dominated. Standalone resources
are copied beneath `target/release`, so the full target cache can also include roughly 796 MB of
WebView2 and FFmpeg payload that rustc will never reuse.

Benchmark these mutually exclusive strategies:

- `rust-cache`: current dependency, Cargo binary, and dependency target behavior.
- `deps-only`: registry/git only; no `target/` and no Cargo tool binaries.
- `sccache`: deps-only plus pinned sccache 0.17.0; no full target cache. JSON hit/miss/write/error
  statistics are retained.
- `none`: diagnostic control.

Do not combine a giant target cache with sccache unless one diagnostic pair unexpectedly wins. One
recent main-branch Windows CI run restored a partial 1.079 GB target cache in 33s, then ran tests in
163s and Clippy in 42s with a 43s post-save. Compared with the cold Nightly, that is directional
evidence for roughly **5m20s** of net warm-build benefit, not yet a release conclusion:
[warm CI run](https://github.com/Clipline-CC/clipline/actions/runs/32050855096/job/95449606017).

Provider-native caches need separate interpretation. Depot transparently redirects Actions caches
and does not use GitHub's branch isolation; confirm trusted-write/cache-poisoning controls before a
release consumes cross-branch compiled objects. Namespace documents remote sccache and NVMe cache
volumes, but its cache-volume mount examples do not document Windows. Blacksmith accelerates
Actions caches while keeping GitHub-compatible branch/tag isolation, and its sccache path remains
on GitHub's backend. Sources: [Depot cache](https://depot.dev/docs/cache/integrations/github-actions),
[Namespace caching](https://namespace.so/docs/solutions/github-actions/caching), and
[Blacksmith caching](https://docs.blacksmith.sh/blacksmith-caching/dependencies-actions).

### Two Tauri compilations are currently required

The standalone overlay adds the offline FFmpeg verifier, FFmpeg resources, fixed WebView2 resource
tree, and `fixedRuntime` WebView mode/path; all other package/update/NSIS settings inherit from the
base config. The mode is compiled into Tauri's application configuration. Current 1.0.2 packages
prove that the executable bytes differ:

| Variant | Executable bytes | SHA-256 |
|---|---:|---|
| Regular | 27,631,616 | `ADB3C7036B70C9828E45EF286D6F1B3AEA5201162BFADFF843640E7FEECC1B78` |
| Standalone | 27,581,440 | `A0338E7CDAFB762CD2F43D431D97B19CD521A3DE2FBCDCF8B623C50822DB13C4` |

The standalone binary contains the exact fixed-runtime path and the regular binary does not.
Bundling the regular executable twice would package the runtime but fail to select it at startup.
That shortcut is rejected. Tauri sources show [`build` compiles then bundles](https://github.com/tauri-apps/tauri/blob/499df79be65ef8c0670abc0207cd9e37b55d8491/crates/tauri-cli/src/build.rs#L99-L135),
[`bundle` uses an already-built executable](https://github.com/tauri-apps/tauri/blob/499df79be65ef8c0670abc0207cd9e37b55d8491/crates/tauri-cli/src/bundle.rs#L51-L71),
and [the runtime applies the compiled fixed-WebView path](https://github.com/tauri-apps/tauri/blob/499df79be65ef8c0670abc0207cd9e37b55d8491/crates/tauri/src/app.rs#L2309-L2326).

A future one-compile design could make Clipline detect the exact pinned runtime beside its
executable before Tauri initializes, then bundle that same executable twice. Its direct measured
ceiling is the second 2m44s compile. It requires installer extraction/hash comparison, both updater
signature verifications, regular Evergreen and standalone fixed-runtime launch tests, playback,
and uninstall tests before production.

### Standalone NSIS is compressing 823 MiB with solid LZMA

The published standalone installer is 285,868,799 bytes (272.63 MiB). Its observable extracted
input is 862,801,336 bytes (822.83 MiB) across only 274 files:

| Input | Files | Bytes | MiB |
|---|---:|---:|---:|
| Fixed WebView2 | 257 | 693,039,172 | 660.93 |
| FFmpeg | 11 | 142,090,230 | 135.51 |
| Clipline executable | 1 | 27,581,440 | 26.30 |
| Helpers/plugins | 5 | 90,494 | 0.09 |

Seven-Zip reports solid LZMA (`LZMA:23`). Its ~1.85 MiB/s effective throughput closely matches the
regular package's throughput; the 7m25 phase is payload volume under default solid LZMA, not file
count or signing. Pruning reviewed WebView2/FFmpeg files is not safe.

Tauri directly supports `lzma` (default), `zlib`, `bzip2`, and `none` and emits
`SetCompressor /SOLID`. Benchmark `zlib` first on the same compiled executable and staged bytes;
retain LZMA unless zlib saves material time with an acceptable installer-size increase and exact
extracted hashes/signatures. `none` would approach 823 MiB and is not practical. Sources:
[Tauri compression enum/default](https://github.com/tauri-apps/tauri/blob/499df79be65ef8c0670abc0207cd9e37b55d8491/crates/tauri-utils/src/config.rs#L800-L815),
[Tauri NSIS template](https://github.com/tauri-apps/tauri/blob/499df79be65ef8c0670abc0207cd9e37b55d8491/crates/tauri-bundler/src/bundle/windows/nsis/installer.nsi#L9-L14),
and [NSIS `SetCompressor`](https://nsis.sourceforge.io/Reference/SetCompressor).

The harness samples `makensis` CPU. Near-one core-equivalent utilization means single-thread speed,
not more logical CPUs, should drive runner selection for this phase.

### Tests, Clippy, and parallelism

Actual tests took seconds after a 6m31s compile; Clippy reused the serial workspace and still needed
3m11s. The warm main observation (163s tests, 42s Clippy) supports improving compiled-object reuse.
Splitting tests and Clippy onto independent runners would also make the packaging job compile the
application a third time unless target artifacts/caches are transferred. No existing measurement
shows that duplicated compilation plus another cache transfer wins, so the proposed release path
keeps tests and Clippy serial on the packaging workspace. Add a parallel experiment only after the
cache/provider winner is known; do not remove or weaken either check.

## Proposed optimized Nightly architecture

Keep the transactional publish job and every existing validation unchanged. Optimize only the
read-only build job, in this order:

1. Install the official hash-pinned Tauri CLI binary (**~9m48 measured saving**).
2. On immutable GitHub release tags, restore an eligible trusted cache but do not save a new 1.5 GB
   sibling-tag cache (**~2m35 measured saving on the baseline miss**). Provider-native cross-tag
   caches require trusted-write confirmation.
3. Select the winning deps-only/sccache/full-target strategy after one cold-ish and three warm runs
   (**~5m20 directional warm opportunity**, release measurement pending).
4. Keep tests, Clippy, regular compile/package, runtime staging, and standalone compile/package
   serial in one workspace so each phase can reuse local artifacts.
5. Adopt supported NSIS `zlib` only if repeated results preserve hashes/signatures/install behavior
   and its size tradeoff is acceptable. The measured upper bound is the 7m25 LZMA phase; a practical
   **4–6 minute saving is a benchmark target, not a result**.
6. Consider application-owned fixed-runtime detection and one executable only after the full dual
   installer proof (**2m44 measured ceiling**).
7. Keep release publication as the existing artifact-only, draft-first transaction with public-byte
   verification.

The first two measured changes reduce the 43m22 baseline to roughly **31 minutes** without relying
on a faster runner. Warm compiler reuse, supported faster NSIS compression, and the provider winner
are all needed for a defensible sub-20-minute target. Provider estimates remain blank until at least
one cold-ish and two warm runs exist; manufacturing numbers from advertised CPU counts would be
misleading.
