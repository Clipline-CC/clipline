# Durable group reorder recovery

## Goal

Never expose mixed group order when both a sidecar update and its immediate rollback fail.

## TDD

- [ ] Add a recovery test with a persisted pre-order journal, a partially updated sidecar, and a
      blocked rollback; the first scan must fail and leave the journal, then a retry must restore
      every previous sidecar before scanning.
- [ ] Write the journal atomically before changing any member order.
- [ ] Keep the journal on rollback failure; make every local Library scan complete recovery first.
- [ ] Remove the journal only after all writes commit or all prior values are restored.
- [ ] Run workspace tests and strict Clippy, relaunch Clipline, update PR #190, and resolve the thread.
