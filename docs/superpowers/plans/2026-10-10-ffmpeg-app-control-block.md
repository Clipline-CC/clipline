# FFmpeg blocked by Windows App Control

Base: `develop` at `ee6a703a`. A Windows 11 user got a modal "ffmpeg.exe - Bad
Image" dialog for `%LOCALAPPDATA%\Clipline\ffmpeg\avfilter-11.dll` with error
status `0xC0E90002`, which is `STATUS_SYSTEM_INTEGRITY_POLICY_VIOLATION` ("An
Application Control policy has blocked this file"). Smart App Control (or a
WDAC policy) refuses the unsigned BtbN FFmpeg DLLs. The bytes are fine: the
managed verifier already hashes every file. Clipline never calls
`SetErrorMode`, so every blocked ffmpeg child raises the loader hard-error
modal, and each failure then reads as "FFmpeg missing", with an Install
button that cannot help.

Codes handled (from Windows SDK 10.0.26100.0 `shared/ntstatus.h` and
`shared/winerror.h`). These are only the ones whose message says the file was
blocked or failed signature checks. Policy-management failures (rollback,
invalid or unsigned policy, too many policies, unauthorized supplemental
policy) are a different problem and stay generic.

- Child exit NTSTATUS: `STATUS_INVALID_IMAGE_HASH` 0xC0000428,
  `STATUS_SYSTEM_INTEGRITY_POLICY_VIOLATION` 0xC0E90002, and
  `STATUS_SYSTEM_INTEGRITY_REPUTATION_*` / `_WHQL_NOT_SATISFIED`
  0xC0E90007–0xC0E9000E.
- Spawn failure Win32 (`CreateProcess` refusing `ffmpeg.exe` itself):
  `ERROR_INVALID_IMAGE_HASH` 577, `ERROR_SYSTEM_INTEGRITY_POLICY_VIOLATION`
  4551, `ERROR_SYSTEM_INTEGRITY_REPUTATION_*` 4556–4559 and 4580–4582,
  `ERROR_SYSTEM_INTEGRITY_WHQL_NOT_SATISFIED` 4583.

- [ ] Neutral `clipline-capture::app_control` (tests on both CI OSes):
  classify `ExitStatus::code()` values, handling the sign of the
  NTSTATUS-as-`i32` correctly, and spawn `io::Error`s into a typed
  `AppControlBlock`. Its message starts "Windows blocked FFmpeg" and
  never contains "ffmpeg is not available", so no install affordance
  matches it.
- [ ] `ffmpeg.rs`: `discover()` returns Found, Blocked or Missing. A pure
  `discover_from` holds the selection rule (first runnable candidate wins,
  otherwise the first block, otherwise missing). `locate()` stays
  discovery-only. `spawn_verified` carries the typed block inside its
  `io::Error`. `probe_report()` stops the whole probe on the first blocked
  child, so startup produces one state instead of a dozen failures. A
  Windows helper-subprocess test exits with 0xC0E90002 to check this.
- [ ] Encoder: a write or flush failure on a child that exited blocked
  reports the block, not "ffmpeg exited with …". Covered by a Windows helper
  test.
- [ ] App spawn sites: posters, shareable export and compilation fallback
  (which stops trying other encoders once blocked), and audio sidecars use
  `discover().require(..)` and classify exit statuses. Posters get the
  diagnostic kind `blocked_by_app_control`.
- [ ] Status and UI: the install snapshot gains `blocked` (symbol, hex code,
  message) from a fresh discovery. `ensure_ffmpeg_runtime` refuses to
  download while blocked, because redownloading can't help and must not
  loop. The settings FFmpeg row says "Blocked by Windows", hides Install /
  Repair, and shows guidance: the files aren't code-signed; check Windows
  Security → App & browser control → Smart App Control; if that's off, the
  policy appears in Event Viewer → Microsoft-Windows-CodeIntegrity/Operational
  (events 3077/3033). Don't push the user to disable SAC. The thumbnail
  warning shows the same state without an Install button, and further
  poster requests stop until a retry. UI contracts first, plus explicit
  `[hidden]` rules.
- [ ] Task 1, error mode: as the first thing in `main`, OR
  `SEM_FAILCRITICALERRORS` into the current `GetErrorMode()` through a safe
  wrapper in `apps/clipline-app/src/windows/`. Children inherit it because
  std `Command` never passes `CREATE_DEFAULT_ERROR_MODE`, so a blocked DLL
  makes ffmpeg exit with the NTSTATUS and no modal appears.
  `SEM_NOGPFAULTERRORBOX` is deliberately left out: it disables Windows
  Error Reporting for Clipline and every child, which costs crash dumps and
  doesn't help here. A Windows test runs the wrapper in an isolated child
  process and checks that a grandchild spawned through std `Command`
  reports the inherited mode.
- [ ] Run workspace tests and fresh-cache warning-denied Clippy.

Plan checkboxes remain unticked by repository convention.
