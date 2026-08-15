# Agent Notes

## Quick reference — commands

| What | Command |
|---|---|
| Build & run the app | `cargo run -p clipline-app` |
| Test everything | `cargo test --workspace` |
| Lint | `cargo clippy --workspace --all-targets -- -D warnings` |
| Test + lint in one shot | `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings` |
| Fast test (skip device tests) | `cargo test --workspace` (device tests self-skip under `CI` env or no hardware) |
| Test with fresh clippy cache | `cargo clean -p <crate> && cargo clippy -p <crate> --all-targets -- -D warnings` |
| Publish a new Nightly | Follow `docs/release-updates.md` → **Agent runbook**; push `nightly-v<version>` only after the release commit is on `develop` |
| Publish a new Stable | Follow `docs/release-updates.md` → **Stable**; push `v<version>` only after the release commit is on `main` |

## Quick reference — project structure

- **Rust workspace** with 6 library crates + 1 Tauri app (`apps/clipline-app`).
- **Source of truth:** `ddoc.md` (product/architecture design doc).
- **Current state:** `handoff.md` (what's done, sharp edges, what's next).
- **Plans:** `docs/superpowers/plans/*.md` (one per milestone, TDD step-by-step).
- **UI:** `apps/clipline-app/` — vanilla HTML/CSS/JS, no npm/bundler. `ui/player-core.js` is
  pure (DOM-free) logic tested from Rust via `boa_engine`. `tests/ui_contract.rs` guards the
  DOM contract. Keep player math in `player-core.js`, not `main.js`.
- **Settings persist:** `%APPDATA%\Clipline\settings.json`. Clips land in `Videos\Clipline\`.

## Conventions (from handoff.md — keep these)

- **Plan-driven TDD.** Each milestone gets a plan doc. Failing test first, then implement.
  Plans are committed before execution; checkboxes stay unticked (repo convention).
- **Commits:** conventional style (`feat(capture): …`), one logical change per commit.
- **Quality gates:** workspace tests green, `cargo clippy` zero warnings, push, CI green on
  both Ubuntu and Windows. Update `handoff.md` after significant work.
- **Platform discipline:** neutral logic stays neutral and testable on both CI OSes.
  Windows-only code behind `#[cfg(windows)]`. All `unsafe` confined to `windows/` modules
  behind safe wrappers. Trait changes happen neutral-side first with tests.
- **License:** MIT OR Apache-2.0 for first-party code. FFmpeg is a separate LGPL process —
  never link it, always spawn it. No GPL code.

## Workflow after implementing a change

1. Run `cargo test --workspace` — must be green.
2. Run `cargo clippy --workspace --all-targets -- -D warnings` — must be clean.
3. If an existing `clipline-app.exe` process is running, stop it before rebuilding.
4. Open the Clipline app for the user to test (`cargo run -p clipline-app`).
5. Give the user a concise list of specific things to test for the change.
6. Update `handoff.md` if the change is significant.

## When the user says “make a new Nightly release”

Treat that phrase as authorization to prepare and publish the next rolling GitHub Nightly. Follow
the agent runbook in `docs/release-updates.md` from start to finish. In particular:

- Do not build or upload release assets manually; `.github/workflows/nightly.yml` owns that work.
- Bump Cargo, Cargo.lock, and Tauri to the same new version, refresh the reviewed WebView2 metadata,
  run the quality gates, and get that release commit onto `develop` before tagging.
- Push a new immutable `nightly-v<version>` tag at the exact `develop` release commit. Never
  force-move a version tag or move the rolling `nightly` tag yourself.
- Watch the **Nightly Release** action through its public-download verification, verify the rolling
  release target and seven assets, then record the publication in `handoff.md`.

## When the user says “make a new Stable release”

Treat that phrase as authorization to prepare and publish the next non-prerelease GitHub Stable.
Follow the Stable runbook in `docs/release-updates.md` from start to finish. In particular:

- Do not build or upload release assets manually; `.github/workflows/stable.yml` owns that work.
- Fast-forward `main` to the `develop` release commit before tagging. Never tag a commit that is
  not contained in remote `main`.
- Push a new immutable `v<version>` tag at that exact commit. Never force-move a version tag.
- Watch the **Stable Release** action through its public-download verification, confirm GitHub
  latest is that non-prerelease with seven assets, then record the publication in `handoff.md`.

## Sharp edges that cost real time

- **CI clippy can fail on lints a warm local cache hides** — `cargo clean -p <crate>` before
  trusting a local clippy pass on changed crates.
- **Device tests** (WGC, MFT, WASAPI) self-skip on CI runners — they run real on the dev machine.
- **MP4 muxer wants 4-byte length-prefixed NALs**; MFTs emit Annex B — `annexb.rs` converts.
  B-frames must stay disabled (no ctts in the muxer).
- **One D3D device and one RelativeClock** must be shared across capture/encode/audio —
  constructors force it.
- **WebView2:** assetProtocol scope does not resolve `$VIDEO` — use plain globs. H.264+Opus
  plays natively; HEVC/AV1 do not. The webview silently no-ops without
  `capabilities/default.json` granting `core:default`.
- **CSS grid:** stacked views (`#review-empty` / `#review-viewer` / `#settings-page`) each need
  explicit `[hidden] { display: none }` — any `display:` rule defeats the `[hidden]` attribute.
- **AMF rejects tiny resolutions** — probe test-encodes at 640×360.
- **SVT-AV1 errors on `-maxrate`/`-bufsize`** — CBR capping is hardware-only.

## Model setup

The default model is **GLM-5.2** (via Ollama, 1M context, free) at **max** thinking
(the highest GLM-5.2 effort level, recommended by Z.ai for coding). It beats GPT-5.5 on
coding benchmarks (SWE-bench Pro 62.1 vs 58.6) and is the better coding model for this
project. Use Ctrl+P to cycle to **GPT-5.5** (for large output, images, or hardest problems)
or **GPT-5.4-mini** (for quick tasks).

### When to use the `consult` tool

**Use `consult` liberally — it's not just for emergencies.** Call it whenever you
want a second opinion or could benefit from a different model's perspective:

- Architecture decisions (trait design, module boundaries, error handling strategy)
- Subtle bugs where you're not sure of the root cause
- Code review before committing — get a second set of eyes on the diff
- Design questions (API ergonomics, naming, data structure choice)
- When your first approach didn't work and you're considering alternatives
- Any time you're uncertain about the right call

The subagent (GPT-5.5:xhigh) gets a fresh context window with just your question
and any files you pass. Don't be proud — escalate early and often. A 30-second
consult is cheaper than a wrong 10-minute implementation.

## Git config (repo-local)

- `user.email` = `dain98@gmail.com`, `user.name` = `Dain`
- Remote is HTTPS with gh as credential helper — don't switch to SSH.
