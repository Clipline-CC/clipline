# Deploy the standalone WebView2 runtime correctly

Base: `fix/ffmpeg-app-control-block` (PR #285), which moves the standalone
installer to Fixed Version 157.0.4341.60. This branch depends on it.

Two deployment faults in the standalone (Fixed Version) installer:

- **Windows 10 AppContainer grant.** From Fixed Version 120, Microsoft requires
  unpackaged Win32 apps on Windows 10 to grant `S-1-15-2-2` and `S-1-15-2-1`
  inherited read/execute on the deployed runtime folder, because the renderer
  runs in an AppContainer. Windows 11 is unaffected. The installer never does
  this (research note `2026-09-07-windows-10-support.md`, Finding 3).
- **Runtime path length.** On the dev machine's AMD RX 6700 XT, the sandboxed
  GPU process crashes (`0xC0000409`) once the runtime folder reaches 168
  characters. Chromium falls back to software rendering, which has no HEVC
  decoder. Today's layout,
  `<install>\webview2-fixed\Microsoft.WebView2.FixedVersionRuntime.<ver>.x64`,
  adds 72 characters, so install directories of 96+ characters lose HEVC and
  GPU acceleration.

Tauri uses `webviewInstallMode.path` both as the build-time source folder and
as the runtime path relative to the executable
(`WEBVIEW2_BROWSER_EXECUTABLE_FOLDER = exe_dir.join(path)`). Staging the
runtime flat at `apps/clipline-app/wv2` therefore deploys it to
`<install>\wv2`: 4 characters, so install directories up to 163 characters
keep the GPU. The folder no longer carries the version, so the verifier must
check `msedgewebview2.exe`'s file version against the manifest.

Tauri's NSIS updater (`/UPDATE`) never runs the old uninstaller and only
overwrites files, so it orphans the ~700 MB legacy `webview2-fixed\` folder
and, with an unversioned folder, would also leave stale files from older
runtimes inside `wv2\`. The hooks must handle both.

- [ ] Add failing contracts: standalone config maps the runtime to `wv2`
  (resource glob and `fixedRuntime.path`) with no versioned folder; the
  verifier checks the staged executable's file version; the scripts and docs
  name `wv2`.
- [ ] Stage the CAB's single versioned root into `apps/clipline-app/wv2`,
  verify its file version, and point the config, benchmark statistics,
  `.gitignore` and `docs/release-updates.md` at it.
- [ ] Add failing hook contracts: a standalone-only hooks file defines
  `CLIPLINE_STANDALONE_WEBVIEW2` and includes the shared hooks; the
  postinstall grant runs `$SYSDIR\icacls.exe` with both SIDs, `(OI)(CI)(RX)`,
  only when `$INSTDIR\wv2\msedgewebview2.exe` exists, and checks the exit code.
- [ ] Implement the grant. Skip it on volumes without persistent ACLs (nothing
  to grant). On failure on Windows 10, offer Retry/Cancel and abort the install
  with an explanation; silent installs abort. On Windows 11, log a warning and
  continue, since Microsoft says the change does not affect it.
- [ ] Add failing cleanup contracts, then implement: postinstall removes the
  exact legacy `$INSTDIR\webview2-fixed` folder in both variants; the regular
  variant also removes an orphaned `$INSTDIR\wv2`; the standalone preinstall
  closes Clipline and clears `$INSTDIR\wv2` so the runtime is exactly this
  payload; the uninstaller removes both folders. Never recurse into a reparse
  point and never use a wildcard.
- [ ] Verify the hook logic in a standalone NSIS harness against a staged
  layout in a temp folder (fresh, update from the legacy layout, Win10 failure
  path, regular variant, uninstall). If possible, build real fresh and update
  installers under a throwaway product name and identifier so Dain's install is
  untouched, and show the `icacls` result.
- [ ] Playback smoke (HEVC, 1080p60 HEVC+Opus, H.264/Opus, AV1 to `ended`)
  from the new layout, including a deep install directory that broke under the
  old layout.
- [ ] Workspace tests, fresh-cache warning-denied Clippy, handoff, draft PR
  into `develop` noting the dependency on #285.

Plan checkboxes remain unticked by repository convention.
