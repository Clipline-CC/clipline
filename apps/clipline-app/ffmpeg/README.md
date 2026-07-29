Release builds stage the pinned LGPLv3 FFmpeg runtime in this directory before
running `cargo tauri build`.

The actual binaries are ignored so they do not land in git. Use only
`scripts/stage-ffmpeg-resource.ps1`; it verifies the immutable archive and
copies the reviewed allowlist here with `PROVENANCE.json`, preserving:

- `ffmpeg.exe`
- FFmpeg DLL dependencies
- `LICENSE.txt` from the distribution and Clipline's third-party notice/source offer

`cargo tauri build` runs `scripts/verify-ffmpeg-resource.ps1` offline before
bundling and fails if this directory is incomplete or differs from the
manifest and provenance receipt.

FFmpeg remains a separate, independently replaceable process. Users may swap
these files for a compatible modified LGPL build.
