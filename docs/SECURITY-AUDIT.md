# Clipline Security Audit

| | |
|---|---|
| **Date** | 2026-06-21 |
| **Version audited** | 0.1.5 (nightly) |
| **Scope** | Full workspace — Tauri shell (`apps/clipline-app`), all six `crates/*`, the frontend (`ui/`), build/CI config |
| **Out of scope** | The separately deployed Clipline Cloud server; the desktop's first-party protocol, transport, cache, and upload clients are in this workspace. |
| **Reviewers** | Two independent passes — **Claude** (full workspace, incl. Windows-gated code) and **Codex** (cross-platform crates + tooling verification). See *Reviewer scope & reconciliation*. |

## Methodology

This document consolidates two independent reviews:

- **Claude** — direct source review of the IPC command surface, path/credential/network code,
  Tauri configuration, and the full frontend, plus targeted deep reads of the MP4 parser, the
  capture/FFmpeg/`unsafe` code, and the LoL/game-detection layer. Every Claude finding was
  verified against source.
- **Codex** — review of the platform-neutral crates with a focus on public-API hardening, plus
  build/run verification (`cargo test`, `cargo clippy`, `cargo audit`, `cargo fmt`, secret scan).

Each finding below is tagged with its **Source** (Claude / Codex / Both). Overlapping findings
were merged; the recommended fixes combine both reviewers' suggestions.

### Reviewer scope & reconciliation

The two reviews reached different top-line conclusions, and the difference is mostly one of
**scope**:

- Codex reported "no critical or high severity issues, **no Rust `unsafe`**, no checked-in
  secrets." Every file path Codex cited is in a *platform-neutral* crate
  (`clipline-lol`, `clipline-mp4`, `clipline-buffer`, `clipline-events`, the non-Windows part of
  `clipline-capture`); it cited nothing from `apps/clipline-app`. The most likely explanation is
  that the Codex pass ran on Linux, where the entire app shell and the `clipline-capture/src/windows/*`
  modules are `#[cfg(windows)]` and compile to stubs. So its "no `unsafe`" conclusion is accurate
  **only for the cross-platform crates** — the full workspace contains substantial Windows
  COM/Win32 `unsafe` (audited by Claude and found sound; see *What's done right*), plus the
  CSP/IPC/cloud/asset-scope surface that only exists in the Windows app.
- Conversely, Codex surfaced **public-API robustness gaps in the MP4 writer and the replay buffer**
  (findings 6, 7, 13) that the Claude pass did not isolate, and ran the `cargo audit` / `cargo fmt`
  verification that Claude did not. The two passes are complementary.
- **Severity of the MP4 issue (finding 1):** Claude rated the `stsz`/`stts` unbounded-allocation
  bug **High** (a ~50-byte file forces a multi-GB allocation); Codex did not isolate that specific
  parser path and rated the MP4 robustness issues it *did* find Medium/Low. Impact is a local DoS
  requiring the user to act on a malicious file — reasonable reviewers could rate it High or
  Medium. It is kept at **High** here because the trigger is trivial and the allocation is
  effectively unbounded.

## Executive summary

**No remote code execution and no command injection were found by either reviewer**, no
checked-in application secrets, and no vulnerable production dependencies (`cargo audit` clean
except one dev-only advisory). The code is, on the whole, security-conscious: solid path
validation, no shell usage anywhere, a non-keylogging hotkey hook, and a frontend that never feeds
untrusted data to `innerHTML`.

The issues worth acting on are:

1. A **trivially-triggerable memory-exhaustion DoS** in the MP4 parser (High).
2. A **TLS-verification bypass that isn't scoped to loopback** (Medium).
3. A **disabled Content Security Policy** that turns any future UI bug into a serious one (Medium).
4. A cluster of **public-API hardening gaps** — unbounded HTTP/JSON bodies and unvalidated
   MP4/buffer metadata that can panic or truncate (Medium/Low).

## Findings at a glance

| # | Severity | Source | Area | Location | Issue |
|---|----------|--------|------|----------|-------|
| 1 | 🔴 High | Claude | MP4 parser | `crates/clipline-mp4/src/trim.rs:684,673` | Unbounded allocation from file-declared sample counts → OOM DoS |
| 2 | 🟠 Medium | Both | LoL client | `crates/clipline-lol/src/client.rs:36` | `danger_accept_invalid_certs(true)` not scoped to loopback → MITM/spoofing |
| 3 | 🟠 Medium | Claude | Tauri config | `apps/clipline-app/tauri.conf.json:24,10` | `csp: null` + `withGlobalTauri` → any future XSS reaches all IPC |
| 4 | 🟠 Medium | Claude | Asset scope | `apps/clipline-app/src/app.rs:881`, `settings.rs:1043` | Runtime asset scope widens to an arbitrary dir, all file types |
| 5 | 🟠 Medium | Codex | LoL client | `crates/clipline-lol/src/client.rs:74-85` | `.json::<T>()` on an uncapped response body → memory exhaustion |
| 6 | 🟠 Med/Low | Codex | MP4 writer | `crates/clipline-mp4/src/init.rs`, `writer.rs` | Unvalidated track config (zero timescale panics; narrowing casts) |
| 7 | 🟠 Med/Low | Codex | Replay buffer | `crates/clipline-buffer/src/segment.rs:25-32` | Trusts public `SampleInfo.size`; slice can panic / narrowing casts |
| 8 | 🟡 Low | Claude | FFmpeg locate | `crates/clipline-capture/src/ffmpeg.rs:131` | Bare `ffmpeg.exe` PATH/CWD fallback → binary planting |
| 9 | 🟡 Low | Claude | Capture unsafe | `crates/clipline-capture/src/windows/nv12.rs:319` | NV12 UV-plane over-read (driver-layout dependent) |
| 10 | 🟡 Low | Claude | IPC | `apps/clipline-app/src/app.rs:779` | `extract_window_icon` takes an unvalidated arbitrary path |
| 11 | 🟡 Low | Claude | MP4 walker | `crates/clipline-mp4/src/walker.rs:54,70` | Unchecked `u64` addition (debug panic; release-contained) |
| 12 | 🟡 Low | Claude | Cloud | `apps/clipline-app/src/cloud.rs:96` | Plaintext-HTTP credentials allowed via `plain_http_confirmed` |
| 13 | 🟡 Low | Codex | Clock math | `crates/clipline-events/src/sync.rs:28-29` | Public `duration_since` fragile on a future `recording_t0` |
| 14 | 🟡 Low | Codex | Tests | `crates/clipline-mp4/tests/ffprobe_validation.rs` | Predictable temp files + `ffprobe` from `PATH`/`~/bin` |

---

## Detailed findings

### 🔴 HIGH — 1. Unbounded allocation in the MP4 parser → memory-exhaustion DoS
*Source: Claude*

**Location:** `crates/clipline-mp4/src/trim.rs:684` (`parse_stsz`) and `:673` (`parse_stts`)

```rust
// parse_stsz — sample_count is a raw u32 read from the file
let sample_count = read_u32(input, p + 8)? as usize;
if sample_size != 0 {
    return Ok(vec![sample_size; sample_count]);            // :684
}

// parse_stts — sample_count is a per-entry raw u32
out.extend(std::iter::repeat_n(delta, sample_count as usize)); // :673
```

`sample_count` is attacker-controlled and unbounded. A ~50-byte crafted `stsz`/`stts` box with
`sample_count = 0xFFFF_FFFF` forces a ~17 GB allocation with no relation to the actual file
size → OOM/abort. The same root cause appears at lower grade in the eager
`Vec::with_capacity(count)` reservations in `parse_co64:724`, `parse_stco:740`, and
`parse_stsz:688` (a `co64` count of `0xFFFFFFFF` reserves ~34 GB **before** the per-entry
bounds check runs).

**Reachable from:** `trim_keyframe_aligned_file` (export/trim), `remux_with_selected_audio_tracks`
(audio preview, cloud upload), and `movie_duration_s` (cloud upload metadata). It is **not**
reached by `list_clips`, which reads duration from the `.markers.json` sidecar — so merely
scanning the folder is safe. The trigger is the user **trimming, audio-previewing, or uploading**
a malicious `.mp4` (one dropped into `Videos/Clipline`, or a downloaded clip).

**Impact:** Denial of service (crash) only — no corruption or RCE. The codec bitstream parsers
and sample-range math elsewhere are bounds-checked and safe; this is the one exploitable class.

**Fix:** Clamp every file-declared count against the remaining byte budget before allocating — a
sample/chunk table can never have more entries than the file has bytes. A shared
`bounded_count(declared, elem_size, input_len)` helper applied across all table parsers closes
the whole class cleanly.

---

### 🟠 MEDIUM — 2. TLS verification disabled, not scoped to loopback (MITM)
*Source: Both (Claude + Codex)*

**Location:** `crates/clipline-lol/src/client.rs:34-43`

```rust
// the doc comment claims "only ever pointed at 127.0.0.1", but nothing enforces it
let http = reqwest::Client::builder()
    .danger_accept_invalid_certs(true)   // :36 — unconditional, applies to any base URL
    .timeout(Duration::from_secs(2))
    .build()?;
```

The certificate-validation bypass (needed for Riot's self-signed loopback cert) applies to
**every** `LiveClient`, including one built from the `--lol-url` override
(`app.rs:949` → `service.rs` → `markers.rs:43`). Pointed at any `https://` host, the client
accepts any forged cert and follows redirects (no `redirect::Policy` is set) — a classic MITM,
after which crafted JSON is fed to the parser. With a remote base it also becomes a limited
SSRF-style fetcher to fixed paths.

Realistic exploitability is bounded — `--lol-url` is a local CLI/test knob, not remotely
injectable — but the "loopback-only" guarantee is enforced by a comment, not code.

**Fix:** Parse the base URL and pass `danger_accept_invalid_certs(true)` **only** when the host is
`127.0.0.1` / `localhost` / `::1`; otherwise build a validating client. Split the test/mock
constructor from the production one so the insecure path can't be reached in production. Add
`.redirect(reqwest::redirect::Policy::none())` (the real local API never redirects). Consider
pinning Riot's local certificate instead of blanket invalid-cert acceptance.

---

### 🟠 MEDIUM — 3. CSP fully disabled + `withGlobalTauri` → any future XSS becomes critical
*Source: Claude*

**Location:** `apps/clipline-app/tauri.conf.json:24` (`"csp": null`) and `:10`
(`"withGlobalTauri": true`)

The entire frontend (`main.js` ~3,800 lines, `player-core.js`, `index.html`) was audited for
DOM-XSS and **none was found** — every untrusted value (clip names/paths, marker actor/champion
names, window titles, cloud titles/usernames/URLs, error strings) reaches the DOM via
`textContent` / `createElement` / `.value`; the only `innerHTML` writes are constant SVG strings
keyed by enums. **This is not currently exploitable.**

However, that safety rests entirely on developer discipline, and the cost of a single future
`innerHTML +=` regression is severe: with CSP off and the full `window.__TAURI__` bridge exposed,
injected script can invoke any backend command — including `cloud_connect` to an
**attacker-controlled host** followed by `upload_clip_to_cloud` to **exfiltrate the user's
clips**, plus `delete_clip`. This is the highest-*consequence* latent issue in the codebase.

**Fix:** Set a real CSP (e.g. `default-src 'self'; media-src 'self' asset: http://asset.localhost;
img-src 'self' data: asset: …`) instead of `null`, and drop `withGlobalTauri` in favor of
importing the specific Tauri APIs, so a future XSS cannot reach arbitrary backend commands.

---

### 🟠 MEDIUM — 4. Asset-protocol scope widens to an arbitrary directory, all file types
*Source: Claude*

**Location:** `apps/clipline-app/src/app.rs:881-883` + `apps/clipline-app/src/settings.rs:1043-1053`

`normalize_media_dir` only checks that the path is **absolute** — not where it points.
`save_settings` then calls `app.asset_protocol_scope().allow_directory(&media_dir, true)`
(recursive, **no extension filter**), broadening the static `*.mp4`-only scope from the config to
*every file type* under whatever `media_dir` is set to. Set it to `C:\` and the WebView can
`asset://`-load any file on the drive. Only data-leaking with script in the WebView (compounds
#3) or a careless media-dir choice — hence Medium.

**Fix:** Constrain the runtime scope to `*.mp4` (plus the specific preview extensions) rather than
the whole directory tree; consider rejecting obviously over-broad roots (drive roots, the user
profile root).

---

### 🟠 MEDIUM — 5. Unbounded JSON response body can exhaust memory
*Source: Codex*

**Location:** `crates/clipline-lol/src/client.rs:74-85` (`get_json`, the `.json::<T>()` call at `:83`)

```rust
self.http.get(format!("{}{}", self.base, path))
    .send().await?
    .error_for_status()?
    .json::<T>()          // no size cap on the response body
    .await
```

A malicious or misconfigured base endpoint — e.g. a local process that binds `127.0.0.1:2999`
before League starts — can return an arbitrarily large body, which `.json::<T>()` buffers fully
into memory. Combined with finding 2 (a remote base via `--lol-url`), the body source need not be
local. The 2-second timeout bounds the *transfer*, not the in-memory size after a fast localhost
response. Impact: self-DoS of the poller thread.

**Fix:** Read the body with an explicit maximum size (cap `Content-Length`, or wrap the body in a
length-limited reader / `bytes()` with a ceiling) before deserializing.

---

### 🟠 MEDIUM/LOW — 6. Unvalidated MP4 track config can panic or corrupt output
*Source: Codex*

**Location:** `crates/clipline-mp4/src/init.rs` (public `VideoTrackConfig.timescale`,
`AudioTrackConfig.sample_rate`) → consumed during finalization in `writer.rs`
(Codex cited `init.rs:11`, `writer.rs:169`; line numbers differ slightly in this checkout)

`VideoTrackConfig.timescale` and `AudioTrackConfig.sample_rate` are public and unvalidated. The
finalization path divides by the timescale, so a **zero timescale panics**. Other public fields
truncate via narrowing casts — channels to `u8`, SPS/PPS lengths to `u16`, and sample sizes to
`u32` (e.g. `size: s.data.len() as u32` at `writer.rs:161`, verified) — silently producing a
malformed MP4 for out-of-range inputs. These are first-party-only inputs today, so the practical
risk is low, but the public API offers no guard.

**Fix:** Validate configs in `HybridMp4Writer::new_multi` — reject zero timescales/sample rates,
invalid channel counts, and oversized SPS/PPS; use `try_from` for narrowing and large-size MP4
boxes where a value can legitimately exceed the field width.

---

### 🟠 MEDIUM/LOW — 7. Replay buffer trusts public sample sizes; slice can panic
*Source: Codex*

**Location:** `crates/clipline-buffer/src/segment.rs:25-32` (`slice_samples`), `SampleInfo.size`
public at `:5`

```rust
fn slice_samples<'a>(data: &'a [u8], samples: &'a [SampleInfo]) -> impl Iterator<Item = &'a [u8]> {
    let mut offset = 0usize;
    samples.iter().map(move |s| {
        let start = offset;
        offset += s.size as usize;   // unchecked accumulation
        &data[start..offset]         // panics if the sizes exceed data.len()
    })
}
```

`SampleInfo.size` is a public field. A public caller (or a buggy encoder) whose declared sizes
exceed `data.len()` makes the `&data[start..offset]` slice panic, crashing replay saving. Codex
notes related unchecked narrowing in `pipeline.rs:155`, `boxes.rs:3`, `fragment.rs:34`, and
`writer.rs:113` (line numbers per Codex's checkout). Verified here: the slice is unguarded and
`SampleInfo.size` is `pub`.

**Fix:** Validate that the sample sizes sum to `data.len()` (or use `data.get(start..offset)` and
return an `io::Error` on mismatch); prefer `try_from` / `checked_add` over `as` narrowing in the
related spots.

---

### 🟡 LOW

**8. FFmpeg binary resolved via bare `ffmpeg.exe` PATH/CWD fallback** *(Claude)* —
`crates/clipline-capture/src/ffmpeg.rs:131`. `CreateProcessW` with a bare name searches the
current working directory first; if Clipline is launched with an attacker-writable CWD, a planted
`ffmpeg.exe` runs at probe time. *Fix:* drop the bare-name fallback or require an absolute path.
(No argument/shell injection exists — all FFmpeg args are constants + numerics + validated paths
passed as single argv elements.)

**9. NV12 UV-plane over-read** *(Claude)* — `crates/clipline-capture/src/windows/nv12.rs:319-328`.
Assumes the UV plane starts exactly at `pitch*h`; a driver using a larger aligned offset would
make `from_raw_parts` read past the mapped buffer into the encoded output (OOB read / info
exposure). Driver-dependent. *Fix:* bound each row copy against the actual mapped subresource size.

**10. `extract_window_icon(exe_path)` takes an unvalidated arbitrary path** *(Claude)* —
`app.rs:779` → `game_icon.rs`. The icon extraction itself is memory-safe and size-bounded, but via
IPC it allows file-existence probing / icon disclosure for arbitrary paths. Only meaningful
alongside an XSS (#3). *Fix:* acceptable for the user-picked-exe use case, but worth noting.

**11. `walk_range` uses unchecked `u64` addition** *(Claude)* —
`crates/clipline-mp4/src/walker.rs:54,70`. `pos + size` with an attacker-controlled `largesize`
panics in debug; the release wrap is contained by later guards. *Fix:* mirror the `checked_add`
already used in `read_box_at`.

**12. Cloud connect allows plaintext-HTTP credentials** *(Claude)* — `cloud.rs:96-120` via
`plain_http_confirmed`. Username/password go over cleartext HTTP when the user confirms. Gated by
explicit confirmation, so Low; ensure the UI warning is unmistakable.

**13. Public clock math is fragile on a future `recording_t0`** *(Codex)* —
`crates/clipline-events/src/sync.rs:28-29` uses `Instant::duration_since`. Codex flagged this as a
potential panic. Note: modern std `Instant::duration_since` **saturates to zero** rather than
panicking (behavior since Rust 1.60), and `recording_t0 ≤ sampled_at` holds by construction in the
current callers — so it does not panic on supported toolchains. Still, the function is public.
*Fix:* use `saturating_duration_since` (or return a `Result`) to make the intent explicit and
future-proof.

**14. Test-only temp-file and command-lookup hazards** *(Codex)* —
`crates/clipline-mp4/tests/ffprobe_validation.rs:6,51` (and similar tests) write predictable files
under `std::env::temp_dir()` and execute `ffprobe` from `~/bin` or `PATH`. On a shared machine
this enables path-hijacking or temp-file races. Test-only, no production impact. *Fix:* use
`tempfile::NamedTempFile` and a controlled `ffprobe` path in CI.

---

## Informational / non-vulnerabilities

- **Privacy claims vs. the Cloud feature.** The README states "no account, no telemetry,
  **nothing leaves your machine without an explicit action**." The **Clipline Cloud** feature
  (login, OS-credential storage in `cloud.rs`, clip upload in `cloud_upload.rs`) is opt-in and
  user-initiated, so it is *technically* consistent — but the prominent "no account / local-only"
  messaging should be reconciled so it does not read as contradictory. *(Claude)*
- **Cloud desktop transport is now first-party.** The former external `clipline-cloud-api`
  dependency was removed during the Slint Library/Cloud migration. The bounded rustls protocol,
  redirect policy, local-only plain-HTTP validation, cache, and upload clients now live in
  `clipline-library` and are covered by repository-security and transport tests. The separately
  deployed Cloud server remains outside this workspace's audit scope.
- **Dev-only unmaintained advisory.** `cargo audit` reports `async-std` as unmaintained, pulled in
  via `httpmock` (a dev-dependency of `clipline-lol`). No production vulnerability. Consider
  replacing `httpmock` eventually. *(Codex)*
- **Workspace hygiene.** `.playwright-mcp` artifacts (browser snapshots/console logs) are tracked
  in git, and `.gitignore` only ignores `/target`. No secrets were found in them, but such
  artifacts are easy to leak context through later — consider untracking and ignoring them. Also,
  the local `.git/config` uses a credential helper that shells out to `gh auth token`; it is not
  committed source, but do not archive or share the `.git` directory. *(Codex)*

## What's done right (verified)

- `validate_clip_path` canonicalizes both sides and checks parent + `.mp4` extension — path
  traversal through the library commands is well-defended.
- Session folder names are purely date-derived (`session_label`); no game/title data reaches a
  filesystem path.
- Custom-game registration is **match-only** — the configured exe path is compared against
  running processes, never launched.
- The `WH_KEYBOARD_LL` hook filters to F1–F24 and only emits a contentless trigger on an exact
  hotkey match (it is **not** a keylogger), with sound `Arc` / `try_lock` shared state.
- No shell usage anywhere; all subprocess calls use explicit `Command` argv (no command/argument
  injection from filenames, window titles, or device names).
- The updater uses a fixed HTTPS GitHub endpoint with a committed minisign public key and
  verified signatures.
- Credentials are stored via the Windows Credential Manager.
- Win32 `unsafe` code (Windows-gated, not seen by a Linux-only pass) uses returned-length slicing
  and frees handles on all paths; the MP4 codec bitstream parsers (`bitread`, `av1c`, `hvcc`) are
  bounds-checked with no `unsafe`.
- No checked-in application secrets (secret-regex scan clean).

## Verification run (Codex)

| Check | Result |
|---|---|
| `cargo test --workspace` | passed |
| `cargo clippy --workspace --all-targets -- -D warnings` | passed |
| `cargo audit` | no vulnerable production deps; one dev-only unmaintained warning (`async-std`) |
| Secret regex scan | no checked-in application secrets |
| `cargo fmt --all -- --check` | failed on pre-existing formatting only |
| `git diff` | clean — no code changes were made during the audit |

## CI supply-chain hardening *(Both)*

`.github/workflows/ci.yml:19` uses mutable action refs (`actions/checkout@v6`,
`dtolnay/rust-toolchain@stable`, `Swatinem/rust-cache@v2`). Recommended:

- Pin actions to commit SHAs.
- Add an explicit least-privilege `permissions: contents: read`.
- Add `cargo audit` and `cargo fmt --check` to CI.

## Prioritized recommendations

1. **Fix the MP4 allocation DoS (#1)** — smallest effort, most trivially triggerable. Clamp all
   file-declared counts to the remaining byte budget before allocating.
2. **Scope the TLS bypass to loopback, split the test constructor, and disable redirects (#2)**,
   and **cap the JSON response body (#5).**
3. **Set a real CSP, drop `withGlobalTauri` (#3), and narrow the runtime asset scope to `*.mp4`
   (#4)** — together these cap the blast radius of any future UI bug, especially given the
   cloud-exfil command path.
4. **Validate public MP4/buffer config (#6, #7)** — reject zero timescales, bound sample sizes,
   replace `as` narrowing with `try_from`.
5. Tighten FFmpeg path resolution (#8) and the NV12 bounds (#9); harden CI; reconcile the privacy
   claims; and separately audit the deployed Clipline Cloud server.
