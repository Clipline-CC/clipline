# Group-owned Compilation Lifecycle

**Goal:** Never render a generated group compilation as a separate Library item, while ensuring
group mutations cannot strand generated media on disk.

- [ ] Add failing tests that reorder and ungroup invalidate every generated compilation for the
      affected group, including sidecars.
- [ ] Keep all `source_group` compilations out of the top-level Library.
- [ ] Reuse the existing guarded clip-file deletion path under the group mutation lock.
- [ ] Invalidate generated compilations before persisting reorder or ungroup mutations; reject the
      mutation if an active upload or filesystem error prevents cleanup.
- [ ] Run focused Groups tests, workspace tests, and warning-denied Clippy; update docs, push the PR,
      resolve any review fallout, and reopen Clipline.
