# Windows Nightly CI Benchmark

This document separates observed evidence from experiments that still need repeated provider runs.
The benchmark branch changes production Nightly only where measurements are already decisive: it
uses the verified prebuilt Tauri CLI and makes the ineffective per-tag Rust cache restore-only.

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

## Measured GitHub-hosted results

The following release-equivalent jobs used commit
`9fa0726082c29940934d2ac2d87d9ef4b9364358` on `windows-latest`. Total is runner job time and
includes setup, the complete signed seven-asset build, artifact upload, and post-job cache work;
it excludes the aggregate reporting job and does not publish a release.

| Strategy | Cache | Repetition | Total | Test | Clippy | CLI | Regular | Standalone | Restore/save |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|
| [full target](https://github.com/Clipline-CC/clipline/actions/runs/32069503479) + source CLI | miss | 1 | 46m21s | 8m31s | 3m10s | 10m04s | 9m24s | 9m55s | 6s / 3m18s |
| [no cache](https://github.com/Clipline-CC/clipline/actions/runs/32069542831) + prebuilt CLI | none | 1 | 31m13s | 6m40s | 2m51s | 3s | 8m42s | 11m45s | 0s / 0s |
| [dependencies only](https://github.com/Clipline-CC/clipline/actions/runs/32069548919) + prebuilt CLI | miss | 1 | 30m09s | 6m19s | 2m37s | 1s | 8m33s | 10m51s | 2s / 45s |
| [dependencies only](https://github.com/Clipline-CC/clipline/actions/runs/32072262809) + prebuilt CLI | hit | 2 | 29m27s | 6m32s | 2m57s | 2s | 9m07s | 9m05s | 18s / 1s |
| [dependencies only](https://github.com/Clipline-CC/clipline/actions/runs/32074717013) + prebuilt CLI | hit | 3 | 32m23s | 7m14s | 3m01s | 1s | 9m36s | 10m36s | 19s / 1s |
| [full target](https://github.com/Clipline-CC/clipline/actions/runs/32073514854) | hit | 2 | 17m43s | 2m12s | 21s | 1s | 3m24s | 9m40s | 50s / 0s |
| [full target](https://github.com/Clipline-CC/clipline/actions/runs/32075011877) | hit | 3 | 17m30s | 2m12s | 21s | 1s | 3m25s | 9m35s | 52s / 0s |
| [full target](https://github.com/Clipline-CC/clipline/actions/runs/32076623001) | hit | 1 | 17m59s | 2m11s | 20s | 1s | 3m09s | 10m13s | 64s / 0s |

Warm medians are **30m55s** for dependencies-only (range 29m27s–32m23s, two samples) and
**17m43s** for a full target (range 17m30s–17m59s, three samples). The latter proves sub-20 is
possible with a truly reusable target, but not that GitHub Nightly tags can obtain one: all three
hits reused the same benchmark branch ref. Dependencies-only restored 158,320,991 bytes and did
not produce a meaningful wall-time improvement over the 31m13s no-cache control. The full cache
was 1,568,375,030 bytes. Those full-hit runs used source-install mode, but the restored
`cargo-tauri.exe` made `cargo install` a one-second no-op; the verified prebuilt path provides that
benefit even when the target cache misses.

No Namespace, Depot, or Blacksmith runner variable was configured in the repository during this
investigation, so those jobs were correctly omitted. There is therefore no measured provider
ranking yet; the workflow is ready for the requested one cold-ish plus two or three warm runs as
soon as each account-specific label is supplied.

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

`benchmark_zlib=true` adds a same-job zlib bundle after preserving the normal LZMA installer,
then extracts and hashes both payloads. `benchmark_parallel_checks=true` adds independent cold
test and Clippy jobs beside a no-cache serial run; it is GitHub-only and disabled by default.

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
Download, hash, extraction, and version verification took 1–3s in three CI runs, versus 10m04s
for the benchmark's cold source install: a measured saving of about **10 minutes**. The 7.4 MB
archive is too small
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

Do not combine a giant target cache with sccache. The release-equivalent measurements now make the
tradeoff concrete: a full hit saved about 13m30s versus no cache and crossed the 20-minute target,
but required a 50–64s restore and only worked on the same ref. Dependencies-only hits spent 18–19s
restoring and remained within noise of no cache. A cold dependency-cache save cost 45s; a cold full
save cost 198s. The production benchmark-branch change therefore keeps restore enabled but sets
`save-if: false` for immutable Nightly tags.

The [cold sccache seed](https://github.com/Clipline-CC/clipline/actions/runs/32076642543)
recorded 1,421 cache misses/writes and no hits across tests, Clippy, and the regular build. Its
release workload reached 34m22s before an instrumentation bug rejected the standalone phase's
valid zero-hit statistics; that bug is fixed. A subsequent
[warm run](https://github.com/Clipline-CC/clipline/actions/runs/32079683882) completed in 26m56s
with 1,261 hits, 160 misses, and no cache errors (88.7% of cacheable requests hit). Tests took
6m50s, Clippy 1m42s, the regular build 4m55s, and standalone 9m19s; sccache observed no cacheable
standalone requests. A second warm dispatch
([run 32079685714](https://github.com/Clipline-CC/clipline/actions/runs/32079685714)) reproduced
the identical 1,261 hits / 160 misses / 0 errors and completed every build phase (test 268s,
Clippy 123s, regular 312s, standalone 637.7s, all exit 0) but failed afterwards because
`sccache --stop-server` could not connect (os error 10061) and the harness treats a failed stop as
fatal; no installer was signed, so it stays a corroborating partial sample rather than a second
complete run. Warm hit rates are deterministic for this commit; the complete warm median remains
the single 26m56s sample. At inspection time this experiment occupied 1.468 GB across 1,582 GitHub
cache objects, before its separate 158 MB dependency cache. That is worse repository-cache
pressure than the single full-target archive. The complete warm run was three to five minutes
faster than the dependencies-only/no-cache controls but 9m13s slower than the full-target median;
GitHub's tag scoping would still make every Nightly a cold sccache seed. It is therefore not a
production candidate on GitHub's backend.

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
`SetCompressor /SOLID`. `none` would approach 823 MiB and is not practical. One paired
[zlib sample](https://github.com/Clipline-CC/clipline/actions/runs/32079681984) ran inside a warm
full-target job on the same compiled executable and staged bytes:

| Metric | LZMA (same run) | zlib |
|---|---:|---:|
| `makensis` wall time | ~7m00s (416.0s CPU inside the 9m49s build phase) | 2m09s (123.5s CPU) |
| Core-equivalent utilization | 0.98 | 0.96 |
| Peak working set | 142.6 MB | 53.2 MB |
| Installer bytes | 285,825,113 | 377,787,803 (+91,962,690, +32.2%) |

The packaging saving is about 4m50s, but every Nightly user would download ~92 MB more. All 274
staged input files extracted byte-identically (payload manifest
`80359b4c7e8d9645268620e870a7995d7543c26a5dd5400f13e82846cf518fc2`), the updater signature was
produced, and the only difference was NSIS's own generated `uninstall.exe` (96,905 vs 86,123
bytes): `makensis` compiles the uninstaller with the selected compressor, so byte-identical
packages are unattainable across compressors and the harness's exact-equality check correctly
failed the job. Treat this single sample as directional only: LZMA stays unless the size tradeoff
is explicitly accepted, and adoption would still need repeated runs plus install/upgrade/uninstall
tests. Sources: [Tauri compression enum/default](https://github.com/tauri-apps/tauri/blob/499df79be65ef8c0670abc0207cd9e37b55d8491/crates/tauri-utils/src/config.rs#L800-L815),
[Tauri NSIS template](https://github.com/tauri-apps/tauri/blob/499df79be65ef8c0670abc0207cd9e37b55d8491/crates/tauri-bundler/src/bundle/windows/nsis/installer.nsi#L9-L14),
and [NSIS `SetCompressor`](https://nsis.sourceforge.io/Reference/SetCompressor).

The harness measured 0.96–0.99 core-equivalent utilization for standalone `makensis`, or
24.1–24.7% of the four-vCPU GitHub hosts. Single-thread speed, not more logical CPUs, should drive
runner selection for this phase.

### Tests, Clippy, and parallelism

Actual tests took seconds after a 6m31s compile; Clippy reused the serial workspace and still needed
3m11s. The warm main observation (163s tests, 42s Clippy) supports improving compiled-object reuse.
In one controlled cold comparison ([run 32080283247](https://github.com/Clipline-CC/clipline/actions/runs/32080283247)),
serial tests plus Clippy took 410s + 184s = 594s. Independent runners took 447.1s for tests and
262.7s for Clippy, reducing the check-only critical path by ~146s but increasing aggregate compiler
work from 594s to 710s. The same run's no-cache serial main workload completed in 1,792s (29m52s;
prebuilt CLI 1.1s, regular 547.8s, standalone 567.9s), confirming the packaging path estimate.
Splitting those two checks is therefore a poor trade. A better follow-up is one serial checks job
(tests then Clippy) beside one packaging job: release-profile compilation already occurs either
way, and publication can depend on both. A packaging-only job must still be measured before
production. Do not remove or weaken either check.

## Proposed optimized Nightly architecture

Keep the transactional publish job and every existing validation unchanged. Optimize only the
read-only build job, in this order:

1. Install the official hash-pinned Tauri CLI binary (**about 10 minutes measured saving**).
2. On immutable GitHub release tags, restore an eligible trusted cache but do not save a new 1.5 GB
   sibling-tag cache (**2m35 baseline / 3m18 benchmark cold-save cost avoided**). Provider-native
   cross-tag caches require trusted-write confirmation.
3. Keep GitHub-hosted Nightly restore-only; dependencies-only and sccache did not beat no cache.
   If a provider offers a trusted reusable cache, repeat the full-target experiment there; the
   same-ref GitHub median shows a **13m30s opportunity**, not a cross-tag result.
4. Benchmark one serial checks job in parallel with one packaging job. Do not split tests from
   Clippy; the measured split duplicated 117s of compiler work for only 146s of critical-path gain.
5. NSIS `zlib` measured **~4m50s packaging saving for +92 MB per download** in one paired sample,
   and byte-identical packages are unattainable because the generated `uninstall.exe` embeds the
   compressor. Keep LZMA unless the size tradeoff is explicitly accepted; adoption would still
   need repeated runs plus install/upgrade/uninstall tests.
6. Consider application-owned fixed-runtime detection and one executable only after the full dual
   installer proof (**2m44 measured ceiling**).
7. Keep release publication as the existing artifact-only, draft-first transaction with public-byte
   verification.

The first two measured changes reduce the 43m22 baseline to roughly **31 minutes** without relying
on a faster runner. Warm compiler reuse and the provider winner are still needed for a defensible
sub-20-minute target; NSIS zlib could remove another ~5 minutes but taxes every download by 92 MB.
Provider estimates remain blank until at least one cold-ish and two warm runs exist; manufacturing
numbers from advertised CPU counts would be misleading.
