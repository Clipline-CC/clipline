# Security scan remediation

**Goal:** Close the supplied security scan findings at their shared boundaries, preserve user media and updater continuity, and record any infrastructure limitations explicitly.

## Release trust

- [ ] Add failing repository contracts for secret-free benchmark builds, artifact-only protected signing, complete action-pin detection, and canonical update URLs.
- [ ] Replace inherited benchmark secrets with a throwaway signing key; validate benchmark inputs before script use.
- [ ] Build installers without updater signing; sign verified artifacts on a GitHub-hosted job without source checkout or compilation.
- [ ] Protect release branches/tags and the signing environment, enable available secret scanning, and document staged key rotation and organization 2FA requirements.

## Storage and renderer authority

- [ ] Reproduce oversized replay deletion, imported audio-selection ownership, replay-cache authorization, prototype-named gallery buckets, and network-path resolution.
- [ ] Refuse impossible quota reclamation; preserve the explicit media adoption boundary.
- [ ] Require native picker authorization for replay-cache changes and reject network paths before filesystem access.
- [ ] Refuse junctions on session writes and restrict main-webview navigation.

## Bounded media and transport

- [ ] Add regressions for aggregate MP4 allocation, track/box/edit-list budgets, oversized sidecars, and corrupt group journals.
- [ ] Bound parser work and allocation; isolate malformed library metadata without exposing an unresolved journal transaction.
- [ ] Validate object-upload destinations, bound create responses, redact credential-bearing diagnostics and Debug output.
- [ ] Harden FFmpeg discovery/downloads, uninstall deletion, process identity sampling, and MFT buffer pointer validation.

## Verification and delivery

- [ ] Run targeted regression checks, workspace tests, fresh-cache warning-denied Clippy, and applicable workflow/script checks.
- [ ] Obtain a second opinion on security boundaries and review the final diff.
- [ ] Update handoff with findings, verification, infrastructure changes, and any staged rotation work.
- [ ] Push the reviewable branch, confirm Ubuntu/Windows CI, and open Clipline with a concise manual acceptance checklist.

Plan checkboxes remain unticked per repository convention. No release is published as part of remediation; key replacement must preserve existing clients through an old-key-signed bridge release.
