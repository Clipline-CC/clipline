# PR 191 compilation lifecycle fixes

- [ ] Add executable regressions for stale/orphan visibility, reorder cache refresh, and export publication after membership changes.
- [ ] Hide only a live group's selected current compilation; preserve stale and orphaned outputs as manageable Library clips.
- [ ] Evict generated artifacts from the Library cache after mutations that delete them.
- [ ] Validate the captured group fingerprint under the mutation lock before publishing an encoded compilation.
- [ ] Run workspace tests and warning-denied Clippy, update handoff/design, push PR 191, verify CI, and open the app.
