# Clipline controlled-fork notes

This directory is based on the published `shiguredo_opus` 2026.4.0 crate
(release 2026-10-09) under the upstream Apache-2.0 license retained in
`LICENSE`. Every file except `build.rs` matches the published crate.

Clipline carries two build-script changes:

1. Embed the reviewed SHA-256 digest for each supported Windows/Ubuntu
   prebuilt archive. Upstream downloads the digest beside the artifact, which
   cannot detect replacement of both files. Each pinned digest matched both
   GitHub's upload-time asset digest and upstream's `.sha256` on 2026-10-25;
   upstream publishes no build attestations.
2. Retry the prebuilt download with `curl --retry 8 --retry-all-errors`. GitHub
   release assets sometimes return 503 or an empty reply; clippy then rebuilds
   the crate in a fresh `OUT_DIR` and fails even after tests already succeeded.

Prebuilt archives are only accepted for the pinned targets: Windows x86_64
and Ubuntu 22.04, 24.04 and 26.04 x86_64. Any other prebuilt target, including
the iOS, Android, macOS and arm64 archives in the upstream README, fails with
"no reviewed prebuilt Opus hash". Build those with the `source-build` feature,
or review and pin their archives first. `README.md` stays identical to
upstream.

The 2026.1.0 fork also selected `opus.lib` on Windows. Upstream fixed that in
2026.4.0, so the patch was dropped when the fork moved on 2026-10-25.

Review this fork by 2027-01-25. Remove it when an upstream release pins its
prebuilt hashes, or replace it with a better maintained binding after rerunning
all Opus capture, decode, mix, remux, MP4, and playback checks.
