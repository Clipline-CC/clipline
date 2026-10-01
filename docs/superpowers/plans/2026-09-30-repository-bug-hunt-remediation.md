# Repository bug-hunt remediation

Audit baseline: `e80762006771ff3edeba9b6f2700e33ff071073b`.
Implementation base: `2faf33e343d64038c140328a7e36e61305a29a52` (`develop`, PR #216).
The user supplied the complete twelve-finding report in conversation after its linked
file could not be found. Revalidate each finding against this newer base; preserve
existing fixes and add coverage where needed. No release or merge is part of this task.

## Reproduction and scope

| ID | Class | Input/interleaving and required behavior |
| --- | --- | --- |
| BH-01 | Race/lifetime | Selected-audio payload is leased while its original is quota-selected; protect the original through preparation, transport, and readiness, releasing deliberately for verified configured deletion. |
| BH-02 | Race/publication | Two same-source/range or titled exports overlap; private reserved pending files and non-overwriting publication must preserve each request's video/metadata. |
| BH-03 | Data/timing | PCM gap exceeds five-second fill cap within a pending GOP; resumed packet retains its true timestamp in memory/disk/session output, including a GOP boundary. |
| BH-04 | Data/capture state | WGC fixed crop exceeds current content after resolution change; report `SourceChanged`, preserving repetition only for transient timeouts. |
| BH-05 | Race/runtime ownership | Old service resolves folder A after replacement published B; atomically reject old-generation root publication. |
| BH-06 | Data/policy state | League category filter changes for the same game after gate resolution; recompute the verdict while retaining manual-recording bypass. |
| BH-07 | Race/account ownership | Account A request completes after switching to B; reject stale records/profile/status mutation and completion-triggered deletion. |
| BH-08 | Data/status mapping | Detail reconciliation observes remote `failed`; preserve failure guidance and permit a new upload instead of deduplicating a failed ID. |
| BH-09 | Data/identity | Titled/untitled ungrouped trim without markers/audio is rescanned; durable trim metadata and ownership survive. Current base already writes metadata unconditionally. |
| BH-10 | Data/clipboard state | Copy text then a file; replacement clipboard formats contain the file and discard obsolete text. Test without touching the user's clipboard. |
| BH-11 | Resource bounds | Create-upload returns oversized success/error JSON; shared 4 MiB/64 KiB caps apply. Current base already uses the bounded parser. |
| BH-12 | Race/stale worker | Delete/rename a clip while osu! score fetch awaits; every sidecar write checks the same live source/pending job under the mutation lock and never recreates the old session. |

## Execution

- [ ] Verify/reproduce against the implementation base using actual application types and narrow deterministic regressions, not audit harness stubs.
- [ ] Protect original upload lifetimes and account-bind asynchronous cloud mutations; correct terminal failure mapping and verify response caps.
- [ ] Reserve each export's pending path and publish safely; verify durable trim identity and replace clipboard formats.
- [ ] Preserve discontinuous audio decode times through GOP sealing and all segment consumers.
- [ ] Surface invalid WGC content regions as errors through the existing capture queue.
- [ ] Serialize recorder-generation root publication and recompute relevant League policy decisions.
- [ ] Reject stale osu! sidecar success/retry/failure writes after deletion or rename.
- [ ] Review all callers and focused regressions, then run workspace tests and fresh-cache warning-denied Clippy.
- [ ] Update `handoff.md`, push, and open a new PR into `develop`; verify Ubuntu/Windows CI and review results.
- [ ] Reopen the patched app and provide focused manual recording/export/cloud acceptance checks.

Reuse existing locks, source identities, sidecar serializers, temporary-name and request
ownership helpers. Keep changes at the shared boundaries; add no new dependency or
general transaction framework. Checkboxes intentionally remain unticked per repository convention.
