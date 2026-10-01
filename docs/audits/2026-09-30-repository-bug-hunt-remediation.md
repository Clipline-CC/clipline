# Repository bug-hunt remediation

The supplied audit reviewed `e80762006771ff3edeba9b6f2700e33ff071073b`.
These changes start at `2faf33e343d64038c140328a7e36e61305a29a52` (`develop`,
merged security PR #216). The original report was supplied in conversation;
this ledger records the resulting fixes and repository regressions.

| Finding | Result | Regression evidence |
| --- | --- | --- |
| BH-01 | The original MP4 is leased before payload preparation and stays protected through remote processing/verification. Verified configured deletion releases only its own lease under the mutation lock. | Actual selected-audio/original payloads with quota GC during transport and processing; a second upload still vetoes deletion. |
| BH-02 | Pending and final export paths are atomically reserved per job. Publication, sidecars, rollback, and complete Library scans share the existing mutation lock. | Concurrent same-source exports and deterministic pending-file interleaving preserve each range, markers, and group metadata; reservation errors do not leak pending files. Scans cannot expose a zero-byte reservation or partially published sidecars, including rollback. |
| BH-03 | Sparse audio run timestamps survive GOP sealing and disk storage. Replay/session muxing emits explicit decode-time gaps. | Actual PCM assembler, Opus encoder, Recorder, and MockEncoder: resumed audio stays at 7.00 s instead of 5.46 s. Memory/disk/session, GOP boundaries, remux, and shortened replay origins are covered. |
| BH-04 | WGC fixed regions validate current content bounds and queue `SourceChanged` ahead of frames. | A 1280×720 crop rejects 720×480 content even while the pool retains its old size; error delivery, valid crops, and transient resize waits are covered. |
| BH-05 | Recorder-generation validation and media-root publication share the runtime lock. | Old A events after replacement B, replacement racing publication, and the stopped-service gap cannot revert the library root. |
| BH-06 | League category changes invalidate resolved verdicts and fetch a fresh queue verdict for the same game. Outstanding lookups and manual bypass are preserved. | Enabling/tightening/changing filters, pending lookups, manual sessions, and obsolete detector restarts use production runtime types. |
| BH-07 | Upload/status/profile mutations check the captured host, account, and credential identity under the settings-save lock. Completion deletion uses the same ownership check. One command owns each account/clip through processing and cleanup; overlapping retries are rejected. Status reconciliation compares its captured record. Frontend results, progress events, deferred feedback, and persistent upload messages are account-bound. | An account-A completion after B rejects before saving or running cleanup. Actual progress/feedback handlers ignore queued A events after switching to B; obsolete upload status clears without erasing B's newer status. Duplicate commands (including Windows path aliases) cannot overlap; ownership releases for retry. Late status updates/removals preserve a newer successful retry. |
| BH-08 | Failed detail responses map to local `failed` with retry guidance, matching summary responses. | Detail mapping plus the real existing-upload lookup permits a retry while retaining the failed remote identity for diagnostics. |
| BH-09 | Already fixed by PR #216: every trim writes explicit identity/ownership metadata. | Titled/untitled exports without audio or markers remain owned trims after rescanning. |
| BH-10 | File and text copies both clear old formats once before publishing their replacement. | Native Win32 text→file copy in a private noninteractive window station leaves CF_HDROP and removes CF_UNICODETEXT; the user's clipboard is untouched. |
| BH-11 | Already fixed by PR #216: upload creation uses the shared bounded response parser. | Valid padded success JSON exceeds the 4 MiB cap; error JSON exceeds the 64 KiB cap. Both are rejected. |
| BH-12 | Success/retry/failure sidecar writes revalidate a regular source MP4 and identical pending job under the mutation lock. They never recreate missing parents. | Delayed real worker flow after delete, rename, replacement job, or nonregular source; trusted root aliases and redirected sessions are covered. |

Validation is recorded in `handoff.md` and the PR checks. Isolated regressions use
production types and narrow controlled interleavings; synthetic MP4 fixtures test
container/timestamp correctness rather than decoder playback. No live cloud accounts,
League matches, physical display resolution changes, or device stalls were used to
exercise those failure conditions. Device wrappers can self-skip when unavailable.
No dependency, settings schema, updater trust, release, or merge change is included.
