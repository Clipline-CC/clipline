# Clipline controlled-fork notes

This directory is based on `shiguredo_opus` 2026.1.0, tag commit
`59d3d218e7c0bdfa9656cb9c58dc1befbca3c1f7`, under the upstream Apache-2.0
license retained in `LICENSE`.

Clipline carries three build-script changes:

1. Select `opus.lib` for the upstream Windows prebuilt archive and `libopus.a`
   for Ubuntu archives. The 2026.1.0 crate expected `libopus.a` on every host,
   although its Windows release contains `lib/opus.lib`.
2. Embed the reviewed SHA-256 digest for each supported Windows/Ubuntu
   prebuilt archive. Upstream downloads the digest beside the artifact, which
   cannot detect replacement of both files.
3. Retry the prebuilt download with `curl --retry 8 --retry-all-errors`. GitHub
   release assets sometimes return 503 or an empty reply; clippy then rebuilds
   the crate in a fresh `OUT_DIR` and fails even after tests already succeeded.

Review this fork by 2026-10-18. Remove it when an upstream release provides
both fixes, or replace it with a better maintained binding after rerunning all
Opus capture, decode, mix, remux, MP4, and playback checks.
