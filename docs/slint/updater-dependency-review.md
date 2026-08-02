# Native updater dependency review

Reviewed: 2026-08-02

`clipline-updater` is framework-neutral and licensed `MIT OR Apache-2.0` with the rest of the
workspace. Its security-sensitive direct dependencies are deliberately narrow:

- `minisign-verify = 0.2.5` is pinned exactly, MIT licensed, has no transitive dependencies or
  build script, and is the same verification implementation previously resolved through
  `tauri-plugin-updater`. Clipline uses its streaming prehashed verification API and preserves the
  existing release signature format and public key.
- `reqwest = 0.12` is the workspace rustls-only client. The updater disables automatic redirects,
  accepts only Clipline GitHub release paths and GitHub's release-asset CDN, caps redirects at five,
  and applies 20-second connection and read-idle deadlines without imposing a whole-installer
  deadline. It does not enable native TLS.
- `windows = 0.62` matches the capture/native-shell interop line. It is target-gated and all unsafe
  process handoff code is confined to `clipline-updater/src/windows.rs` behind safe typed ownership.
- `sha2`, `base64`, `semver`, `url`, `serde`, `serde_json`, `tokio`, and `thiserror` are already
  reviewed workspace/application primitives. They cover streaming telemetry, the legacy outer
  base64 signature envelope, strict release parsing, bounded asynchronous I/O, and value-free
  errors.

Removing `tauri-plugin-updater` eliminates its updater-owned dependency surface. Tauri core 2.11.2
still resolves reqwest 0.13 for unrelated rollback-shell services; that temporary duplicate-line
exception remains documented in `docs/dependency-policy.json` until Tauri is retired or aligned.
