# Groups second review follow-up

## Scope

The review was pinned before the first follow-up commit. Keep those resolved items intact and fix
the remaining current-head defects with existing policy/helpers.

## TDD tasks

- [ ] Add a UI contract requiring document-level `dragover` and `drop` cancellation, then prevent
      external file drops from navigating the WebView while preserving row-owned drag handlers.
- [ ] Add a Unicode group-name test and use the same locale-independent lowercase key in Rust and
      JavaScript.
- [ ] Add a corrupt-sidecar reorder test and refuse to overwrite unreadable group metadata.
- [ ] Make compilation duration inspection fall back to summed member duration instead of orphaning
      the already-published MP4.
- [ ] Preserve legacy titled-export kind behavior; write the trim metadata sidecar only for grouped
      exports.
- [ ] Derive visible group cards from member filter/search predicates so grouped content remains
      reachable under kind, marker, game, and search filters.
- [ ] Single-flight group picker submission and compilation creation so Enter/copy/upload races
      cannot create duplicate outputs.

## Verification

- [ ] Run focused regressions, Node syntax, workspace tests, and warning-denied Clippy.
- [ ] Relaunch Clipline, push PR #190, and watch Ubuntu/Windows CI plus Greptile.
