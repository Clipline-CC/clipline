# Windows 10 DWM probe packaging and validation

Continue PR #200 from `e6dd13a2` on physical Windows 10 Home 19045,
Ryzen 9 7940HS / Radeon 780M. Keep the experiment outside production recording.

- [ ] Preserve the copied probe hash, machine/driver baseline, and native exit codes.
  Reproduce `--help` / `--list` failing before capture with `0xC0000135`.
- [ ] Deploy the Microsoft-signed x64 runtime from Microsoft's redistributable
  app-locally and repeat both loader checks without changing the executable.
  Do not repeat the failed `crt-static` workaround with dynamic-CRT Opus.
- [ ] Add a package staging script with explicit runtime provenance and a fixture
  runner that checks native exit codes before reporting missing target windows.
  Reproduce the missing-runtime failure before implementing the runner; test the
  corrected package and a deliberately incomplete copy on this clean machine.
- [ ] Run the controlled overlap/resize/minimize fixture on Basic Display Adapter,
  then establish an official AMD driver and repeat where installation permits.
  Preserve logs, BMPs, desktop observations, and sampled memory/CPU/handle counts.
- [ ] Verify the WebGL renderer before labeling any result AMD accelerated. Check
  League/Valorant installation and login prerequisites; test accessible practice
  sessions in windowed, borderless, and exclusive modes separately.
- [ ] Run applicable script checks and workspace tests/Clippy where build tools
  permit, review the diff, update the existing PR and docs with measured results
  and explicit blockers. No WGC fallback, injection, protected-window bypass,
  production integration, or claims of synchronization from hashes/update IDs.

Evidence remains local until reviewed for sharing. Driver installation may require
interactive Windows administrator approval; do not treat elapsed time as consent
or automatically restart this test machine.
