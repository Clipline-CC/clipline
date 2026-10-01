# Protected signing and Nightly 1.0.10

Base: `develop` at `2b2d12ce`. Nightly 1.0.9 built successfully but signing
received an empty environment secret in both attempts of run `36826589986`.
The original key validates against the committed updater public key. Keep that
trust and the protected, GitHub-hosted artifact-only signing job.

- [ ] Commit this plan before implementation.
- [ ] Extend the existing repository security regression to require explicit
  routing of only the updater key/password to both reusable signing calls,
  optional declarations in the callee, and an explicit empty password argument.
  Run it failing before the fix.
- [ ] Declare those optional secret inputs and map them at the Nightly/Stable
  signing calls. Environment secrets override the empty caller values after
  approval. Keep builds and benchmarks free of signing secrets; do not use
  broad `secrets: inherit` or add repository-level keys.
- [ ] Quote the password argument so an unset password remains an empty CLI
  argument. Verify signing a non-release test payload with no password.
- [ ] Advance Cargo, Cargo.lock and Tauri to 1.0.10. Preserve immutable
  `nightly-v1.0.9`; no assets from that failed release were published.
- [ ] Keep the just-reviewed Fixed Version runtime 154.0.4258.48 and October
  review dates. Run runtime preflights, workspace tests and fresh-app-cache
  warning-denied Clippy. Reopen the normal app.
- [ ] Review and merge the fix/release PR after Ubuntu and Windows CI; switch
  to develop, pull, and tag its exact release commit as `nightly-v1.0.10`.
- [ ] Review unsigned artifacts, approve protected signing, watch publication
  and public-download verification, verify seven assets and both manifest
  signatures, then record the published release in handoff.md.

Evidence: [upstream empty-environment-secret reproduction](https://github.com/actions/runner/issues/4453),
[explicit named-secret routing example](https://github.com/actions/runner/issues/1490),
and [GitHub reusable workflow documentation](https://docs.github.com/en/actions/how-tos/reuse-automations/reuse-workflows).
The remote signing run is the acceptance check for GitHub's secret resolution;
local source assertions only protect the routing and isolation contract.

Plan checkboxes remain unticked by repository convention.
