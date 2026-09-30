# Security scan remediation — 2026-09-30

Changes are on `fix/security-scan-2026-09-30`. No release was published and the
updater public key is unchanged.

| Report finding | Remediation |
| --- | --- |
| Production key on Depot/unreviewed benchmarks | Removed inherited secrets; generate and smoke-test a throwaway key on each benchmark runner. Removed the old production-key test requirement. |
| Signing while dependencies compile | Both installers build with updater artifacts disabled. A separate GitHub-hosted job downloads five unsigned assets, validates their identity, and signs with a pinned prebuilt CLI. No checkout, dependency compilation, or cache in that job. |
| Write access can publish signed updates | Protected `release-signing` environment with maintainer approval and version-tag policies; moved the existing private key there and deleted the repository secret. Branches reject deletion/force-push; develop requires PR/Ubuntu/Windows checks. Only the maintainer can create version tags; version tags cannot move or be deleted. Enabled secret scanning and push protection. |
| Replay larger than quota deletes clips for no benefit | Both quota-reclamation paths return before deletion when the requested write exceeds the whole quota. Equality still permits reclamation. |
| Renderer chooses arbitrary replay-cache directory | Independent native-picker authorization for media and replay-cache paths, consumed only on successful settings commit. Validation covers saved disk paths even in memory mode; resetting to the default remains available. |
| UNC paths contact a host before containment checks | Reject UNC/device paths before normalization or clip/group/enrichment resolution. Access checks configured or canonical root containment before resolving the requested file, retaining trusted local aliases such as Windows short paths. |
| Imported audio selection creates ownership | Marker sidecars no longer prove ownership. Imported audio selections remain readable but do not make the original eligible for quota/uninstall deletion. Trim exports always write explicit ownership metadata. Linked ownership proofs are refused. |
| Aggregate MP4 allocation and track amplification | Cap movie-wide samples at 4 million and tracks at 64 before expanding tables; bound repeated HEVC parameter arrays and reject empty parameter NALs. |
| Quadratic edit-list scanning and unlimited box walks | Advance a monotonic sample cursor; cap file-header traversal at 4096 and per-container/sample-entry walks at 65,536. Native finalized recordings contain only three top-level boxes regardless of duration. |
| Unlimited marker/metadata reads | Shared bounded JSON reader and matching serializer cap sidecars/journals at 8 MiB. Mutations refuse malformed/oversized existing data; export/enrichment/rename paths use the same checks. |
| Malformed group journal blocks whole library | Preserve syntax-invalid/oversized journals separately without applying them; scanning continues. Valid recovery still validates all paths before any rollback and retains its journal if a write fails. Rejected oversized reorder preserves existing compilations. Compilation cleanup occurs before committing an order change; a lease/cleanup failure rolls the order back. |
| Prototype names crash grouped gallery | Session buckets use a null-prototype object; regression covers `constructor`, `__proto__`, and other inherited names. |
| Old GitHub updater redirect | Regular/standalone Stable/Nightly endpoints use `Clipline-CC/clipline`. |
| Unrestricted direct upload URLs | Require HTTPS for external object origins; resolve, reject private/reserved destinations, and pin public DNS answers. Disable proxies/redirects for that transport. Explicitly consented self-hosted cloud origin remains supported. Unsafe object targets fall back to the authenticated cloud proxy. |
| Unlimited create-upload response | Reuse the existing bounded control-response reader. |
| Presigned URLs/secrets in diagnostics | Strip URLs from direct PUT errors and rejected-origin diagnostics. Remove secret-bearing app Debug derives; pin the Cloud SDK's redacted Debug patch, [Cloud PR #67](https://github.com/Clipline-CC/clipline-cloud/pull/67). Wire formats remain unchanged. |
| FFmpeg PATH/elevated unverified runtime | Windows refuses bare PATH discovery and checks executable/DLL/provenance hashes before every subprocess launch, including cached poster paths. Discovery shares that launch guard rather than hashing twice for its version probe. Elevated launches require pinned bytes. Ordinary users retain deliberate LGPL replacement through an absolute `CLIPLINE_FFMPEG` override. |
| FFmpeg downloads allow HTTP | The download client is HTTPS-only, with the existing archive/payload hashes retained. |
| Recursive installer cleanup | NSIS removes only empty runtime directories after the guarded cleanup helper; unrelated files are retained. |
| Main-webview navigation | Allow only the packaged app root/index origin. External navigation is refused on initial and recreated webviews. |
| Recycled process IDs and memory-read access | Memory sampling uses query rights, never VM-read rights; checks parent/child creation order, snapshot timing, and identity on the opened handle. Only snapshot descendants are queried. |
| Session writes through junctions | Refuse linked/reparse session directories and metadata; reserve replay/recording files exclusively. Failed saves remove only files they created. |
| Null MFT output pointers | Both MFT output paths reject null pointers before slice construction and unlock the buffer. |
| Benchmark input/action-pin gaps | Validate runner/provider/cache/CLI inputs and pass values through environment variables. Pin checking recognizes every `uses:` line, including named steps. |

## Pending operational work

- **Rotate the exposed updater key through a bridge release.** The protected environment currently
  holds the original key so installed clients can still update. Ship the new public key in an
  old-key-signed bridge for both channels, retain a migration route for clients that miss it, then
  replace the protected private key. See [release runbook](release-updates.md#rotation-after-the-benchmark-exposure).
- **Organization 2FA:** leave enforcement pending until `zipknicks` enables it, per the user's decision.
- Merge this remediation before releasing: old release workflows no longer have a repository
  signing secret. Each future signing job requires the maintainer's environment approval.

## Verification and limits

Regression checks cover media preservation, imported ownership, parser budgets, linked session
writes, picker isolation, network paths, navigation, upload bounds/redaction, process reuse, and
prototype names. `scripts/tests/test-release-signing.ps1` runs both actual workflow script bodies
with throwaway keys; local independent minisign verification checked all four manifest signatures.
Actionlint and PowerShell parsing validate the workflows. All 1620 workspace tests pass
(two intentional ignores), along with fresh-cache warning-denied Clippy. The live app rejects
unpicked replay-cache paths, UNC paths and external navigation. Platform CI and dependency
security checks are tracked on [PR #216](https://github.com/Clipline-CC/clipline/pull/216).

Marker-only legacy clips with arbitrary filenames are conservatively retained as unmanaged;
explicit title/file edits adopt them. Malformed journals are preserved, but their automatic rollback
cannot be reconstructed. Parser limits can reject exceptionally large imported movies while leaving
their files intact. File verification and process spawning are separate operations, so concurrent
local replacement remains a race; these checks do not replace filesystem permissions. External
object uploads bypass proxies and may fall back to the cloud proxy on restricted networks.
