# Unicode group fingerprint parity

## Goal

Keep Rust's persisted Windows path key byte-identical to JavaScript's `clipPathKey` for non-ASCII
member paths.

## TDD

- [ ] Extend the fingerprint test with uppercase/lowercase accented path components and prove the
      current ASCII-only fold fails.
- [ ] Replace the Rust ASCII fold with Unicode lowercase conversion.
- [ ] Run focused tests, workspace tests, strict Clippy, relaunch Clipline, and update PR #190.
