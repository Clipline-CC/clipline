# Clear the October 2026 RustSec advisories

Base: `chore/dependency-policy-review` at `c3da154a` (PR #267). The advisory
database updated on 2026-10-24 flags 15 vulnerabilities in `Cargo.lock`. The
weekly audit on `main` passed on 2026-10-19 and will fail from its next run.

- **TLS stack:** `rustls` 0.23.45 (4 advisories), `rustls-webpki` 0.103.15,
  and `webpki-roots` / `webpki-root-certs` 1.0.7 (distrusted emSign
  intermediates). All HTTPS goes through both `reqwest` release lines: cloud,
  League client and the updater. Fixed in semver-compatible releases.
- **`quinn-proto` 0.11.15:** 5 advisories, fixed in 0.11.17. Lockfile only.
- **`zip` 4.6.1:** 3 advisories, all in `ZipArchive::extract()` and stream
  extraction, and all Unix behaviour (SUID bits, permissions through
  symlinks, symlink targets). Fixed only in 9.0. Clipline never calls
  `extract()`: the FFmpeg install reads allowlisted entries by name and checks
  each size and SHA-256. `tauri-plugin-updater` (2.14.0 included) pulls
  `zip` ^4 on Windows for its optional `zip` feature. That feature only
  extracts zip-wrapped updates, which Clipline does not publish
  (`createUpdaterArtifacts: true` ships the NSIS setup.exe). The update is
  signature-checked before install either way.

`.cargo/audit.toml` must keep `ignore = []`, so the fix removes `zip` 4
instead of ignoring it.

- [ ] Add a failing repository-security contract: `Cargo.lock` selects no
  `zip` release below 9, and no `rustls`, `rustls-webpki`, `webpki-roots`,
  `webpki-root-certs` or `quinn-proto` version named by these advisories.
- [ ] Depend on `tauri-plugin-updater` without default features, keeping
  `rustls-tls`. Its zip and tar.gz update paths and their dependencies drop out.
- [ ] Move Clipline's direct `zip` to 9 with `deflate` and adapt the support
  bundle writer and the FFmpeg verifier if the API changed.
- [ ] `cargo update` the TLS and QUIC crates to their patched releases.
- [ ] Run workspace tests (support bundle round-trip, FFmpeg verification and
  device tests included) and fresh-cache warning-denied Clippy. Run
  `cargo audit` and confirm no vulnerabilities remain. Check the updater still
  reads a signed manifest and the NSIS path builds. Update the handoff, and
  open a PR with green Ubuntu, Windows and RustSec checks.

Plan checkboxes remain unticked by repository convention.
