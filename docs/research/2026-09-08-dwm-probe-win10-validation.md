# DWM probe: physical Windows 10 validation, September 8

The probe's original clean-machine failure was packaging: adding Microsoft's
official x64 `vcruntime140.dll` app-locally makes the unchanged executable run.
Controlled Windows 10 capture works on **Microsoft Basic Render Driver** through
overlap and resize/minimize/restore. This does not establish AMD hardware capture,
game compatibility, exclusive-fullscreen behavior, or producer synchronization.

## Machine and executable

- Windows 10 Home 22H2, build 19045, physical Micro Computer (HK) Venus-series PC.
- Ryzen 9 7940HS / Radeon 780M hardware; the installed display driver remains
  **Microsoft Basic Display Adapter 10.0.19041.3636**, PCI VEN_1002 / DEV_15BF.
- DWM probe selected **Microsoft Basic Render Driver**. The visible WebGL renderer
  also reports `ANGLE (Microsoft, Microsoft Basic Render Driver (0x0000008C)
  Direct3D11 vs_5_0 ps_5_0, D3D11)`.
- Tested binary copied from the prior probe package, corresponding to the user's
  last verified PR commit `e6dd13a2b8e856325c1754a9f0082cc37572be03`.
  Its SHA-256 remains
  `D08041236A7C125C2CBAC0690B551D8A77A0BCB332E7FAE0FB410405068B89E8`.
- Original files: `C:\Users\Dain\Desktop\CliplineDwmTest-20260908-022112`.
  Evidence: `C:\Users\Dain\Desktop\CliplineDwmValidation-20260908-023459`.

## Reproduced loader failure and repair

Before deploying the DLL, both `--help` and `--list` returned **-1073741515 /
0xC0000135**, with no capture started. The temporary `run-tests.ps1` ignored this
exit code and eventually blamed a missing target, even though the target launched.

Downloaded Microsoft's [official x64 Visual C++ redistributable](https://aka.ms/vc14/vc_redist.x64.exe),
verified its Microsoft Authenticode signature, and extracted the x64 minimum-runtime
payload with Windows' cabinet expansion utility. No global runtime installation
was performed. The app-local DLL's Microsoft signature is valid:

| Item | Value |
| --- | --- |
| Runtime | `vcruntime140.dll` 14.51.36247.0, x64 |
| Redistributable SHA-256 | `843068991DAAA1F73AD9F6239BCE4D0F6A07A51F18C37EA2A867E9BECA71295C` |
| Runtime SHA-256 | `D1F4225DF2CD877DBF130D5668A021DCE3F94118455FF5EC952061C30AFC9CE7` |
| DLL signer | Microsoft Windows Software Compatibility Publisher / Microsoft Corporation |
| After repair | `--help` and `--list` both exit 0 |

Microsoft documents [app-local runtime deployment](https://learn.microsoft.com/en-us/cpp/windows/redistributing-visual-cpp-files).
The original Desktop folder now also contains the DLL and corrected launcher;
the original launcher was backed up in the evidence directory. The probe itself
was not rebuilt or changed. The earlier failed `crt-static` attempt was not repeated.

## Controlled target observations

| Run | Duration | Readable samples | Changed samples | Errors | Reads/s | Mean / max read ms |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Initial baseline | 22.007 s | 1,109 | 654 | 175 | 50.39 | 1.066 / 8.031 |
| Corrected runner | 22.015 s | 1,110 | 649 | 176 | 50.42 | 1.002 / 4.956 |
| Five-minute fixture | 300.023 s | 17,400 | 10,224 | 177 | 58.00 | 1.146 / 5.040 |

Every error in these two runs was `target is minimized`. The corrected runner's
minimized interval was 14.865–17.855 seconds, followed by readable samples through
21.994 seconds. Surfaces resized from 656×399 to 816×489, including non-client
padding. During the overlap interval (probe seconds 5–10), the corrected runner
logged 292 reads with 151 distinct pixel hashes.

Visual inspection of the initial `middle.bmp` shows the red/blue reference blocks
and green square, with no magenta covering-window pixels. A contemporaneous desktop
PNG shows the magenta window covering the target. `last.bmp` shows the resized
target and a different square position after restore. Desktop screenshots at probe
seconds 2 and 20 show no yellow border around the visible target. These observations
cover those sampled instants, not continuous proof of border absence or complete,
tear-free frames. Hashes/update IDs alone do not establish freshness or synchronization.

An initial observation helper reused a filename for fixture and probe stderr and
failed while saving its post-run logs. The probe's CSV, summary, BMPs, and desktop
PNGs were retained. The corrected-runner repeat uses separate files and exits zero;
no memory/CPU result is attributed to the incomplete helper log.

The five-minute run extended only the local fixture's close deadline from 28 to
330 seconds; overlap/resize/minimize/restore timings stayed the same. Its 177 errors
were minimized-window errors. Across 298 roughly one-second process measurements,
the 30.3–299.8 second steady interval used 10.95–14.03 MiB working set,
5.09–6.69 MiB private memory, and 157–160 handles. First/last steady measurements
were 10.96/10.95 MiB working set and 5.14/5.10 MiB private memory, with 160/158 handles.
The probe consumed 16.11 CPU seconds, about 5.37% of one core over the sampled
299.8 seconds. This run shows no sustained upward trend in those sampled resources;
it is a five-minute software-rendered fixture observation, not a game benchmark or
a guarantee against longer-lived leaks. The desktop/browser and script checks were
also active on this machine.

A separate 30.019-second capture of the visible `gpu.html` Edge window returned
1,758 reads, 1,757 changed samples, and zero errors (58.56 reads/s, mean/max read
cost 3.307/7.906 ms). Its first/middle/last BMPs show the square at different
positions and visible WebGL counters 27,654 / 28,613 / 29,573, with red/blue blocks
and the Microsoft Basic Render Driver string. This establishes changing software
WebGL content in these snapshots, not AMD acceleration or game validation.

## Prerequisites and untested cases

AMD's [7940HS driver page](https://www.amd.com/en/support/downloads/drivers.html/processors/ryzen/ryzen-7000-series/amd-ryzen-9-7940hs.html)
lists Adrenalin **26.8.1 WHQL Recommended** for Windows 10 x64;
the [release notes](https://www.amd.com/en/resources/support-articles/release-notes/RN-RAD-WIN-26-8-1.html)
include Windows 10 21H2 and later. The official installer was downloaded and its
AMD Authenticode signature verified. SHA-256:
`47272E13BD537C5796F1C760AF036D011B41684737BCDAF30B158D3BAB6740F3`.
It is saved locally as `amd-adrenalin-26.8.1.exe` in the evidence directory.
Windows administrator approval was requested, but the prompt was canceled;
installation did not start. No reboot or display-driver change was performed.

No League, Valorant, or Riot uninstall entries were found, nor their standard
installation directories on C:, D:, or E:. This is a prerequisite check, not a scan
of every possible custom install location. Game installation and login remain
necessary before practice/noncompetitive tests can run.

| Case | Status |
| --- | --- |
| Clean-machine probe startup | Reproduced and repaired with official app-local runtime |
| Controlled Win10 GDI fixture | Software-rendered overlap/resize/minimize recovery observed |
| WebGL renderer | Microsoft Basic Render Driver, not AMD acceleration |
| AMD accelerated capture | Blocked on interactive driver installation |
| League/Valorant windowed | Blocked on driver, game installation, and login prerequisites |
| Game borderless fullscreen | Untested; separate acceptance required |
| Game exclusive fullscreen | Untested; separate acceptance required |
| Complete-frame synchronization | Unproven; do not promote to production |

## Packaging and runner checks

`scripts/package-dwm-probe.ps1` stages an x64 probe with a Microsoft-signed release
runtime and records hashes/provenance. It validates the staged executable's loader
commands, keeping desktop-title logs outside the package. `scripts/test-dwm-probe.ps1`
checks native exits before launching the fixture, preserves timeout output, and
cleans up only its own target process. Runner regressions execute under Windows
PowerShell 5.1 and are included in Windows CI.

The real incomplete package failed with the original loader code before fixture
resolution. The corrected package passed both preflight commands. End-to-end test
executables exercised help failure, list failure, success, timeout stdout/stderr,
and paths containing spaces. Independent review found two issues (timeout log loss
and private enumeration logs inside the staged package); both were corrected.
Package staging also rejected an unsigned runtime, a signed x86 runtime, and an
existing destination, all with nonzero exits.

Local `cargo test --workspace --locked` and warning-denied workspace Clippy both
exit 101 because Microsoft's C++ linker `link.exe` is not installed. These are
environment-blocked checks, not passing local Rust validation. No Rust or production
recorder code changed, and the Clipline UI could not be rebuilt/launched here.

Keep full desktop PNGs and window lists local: they include unrelated desktop/UI
content. Review any selected fixture-only snapshots before sharing. Continue with
the AMD driver, actual renderer verification, and separate game-mode acceptance;
the prior Windows 11 / RX 6700 XT results do not fill these Windows 10 gaps.
