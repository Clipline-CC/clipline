# FFmpeg Thumbnail Reliability

> **For agentic workers:** Execute this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for
> tracking and remain unticked by repository convention.

**Goal:** Make local gallery posters work in pulled source builds, prevent an incomplete FFmpeg
runtime from reaching an installer, and turn genuine runtime failures into an actionable UI state.

## Task 1: Pin the reproduced discovery failure

- [ ] Add a failing `clipline-capture` unit test proving a debug/source build can discover the
      installed runtime under `%LOCALAPPDATA%\Clipline\ffmpeg`.
- [ ] Keep the explicit override and packaged Tauri resource ahead of installed/user fallbacks,
      keep `PATH` last, and avoid duplicate candidates when directories coincide.
- [ ] Implement the Local AppData candidate without removing the existing Roaming AppData
      compatibility path.

## Task 2: Keep the FFmpeg child out of protected recording folders

- [ ] Add tests for bounded poster bytes captured from an FFmpeg stdout pipe and for atomic
      Rust-owned publication beside the clip.
- [ ] Change poster extraction to emit one JPEG through `image2pipe`; drain stdout and stderr
      concurrently, enforce bounds, and let Clipline write the sibling temporary file.
- [ ] Preserve timeout kill/reap behavior and cleanup of incomplete poster files on every error.

## Task 3: Fail closed when packaging the runtime

- [ ] Add a staged-resource verifier that checks the exact manifest allowlist, file sizes and
      hashes, provenance, executable version, and LGPL configuration.
- [ ] Wire the verifier into Tauri's release build command so a README-only `ffmpeg/` directory
      cannot produce an installer.
- [ ] Extend repository contract tests and release documentation to pin the mandatory preflight.

## Task 4: Replace silent gradients with an actionable failure

- [ ] Add frontend coverage for one missing-component warning per foreground session and a retry
      action that clears only failed local poster entries.
- [ ] Surface the exact unavailable-runtime error while leaving corrupt single-clip failures on
      their normal gradient fallback.
- [ ] Emit a bounded diagnostic event for unavailable or failed poster extraction so support
      reports distinguish discovery, execution, and media failures.

## Task 5: Verify and publish

- [ ] Run formatting, targeted Rust/Boa/UI contract tests, workspace tests, and warning-denied
      workspace Clippy.
- [ ] Temporarily move the Roaming AppData FFmpeg override aside and use computer control to prove
      the debug app discovers the installed Local AppData runtime and regenerates a missing poster.
- [ ] Exercise release-preflight rejection with an incomplete fixture and success with the staged
      reviewed runtime.
- [ ] Update `handoff.md`, commit each logical change, push `pr-memory-gaps`, and confirm PR #107
      checks are green.
