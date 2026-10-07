# Finalize interrupted full-session recordings

Base: `develop` at `c5ec0657`. A full session is written as a Hybrid MP4: a
fragmented `.mp4.recording` (ftyp, 16-byte `free` placeholder, init `moov` with
`mvex`, then `moof`/`mdat` pairs) that `HybridMp4Writer::finalize` turns into
ftyp / mdat / moov in place. If the process dies first (killed dev build,
crash, power loss), startup recovery only renames `.recording` to `.mp4`. The
library then shows a fragmented file with an empty index. WebView2 must walk
every fragment through the asset protocol to open it (838 range requests and
858 MB read for a 432 MB session instead of 2–3 requests). Five real sessions
from 2026-09-30/10-01 are broken this way.

Goal: recovery finalizes interrupted recordings exactly like a clean stop, and
repairs already-renamed ones that older builds left behind.

- [ ] Add failing `clipline-mp4` tests: a writer killed after N fragments
  (`into_inner`) recovers to bytes identical to `finalize()` for the same
  fragments; a torn last fragment (truncated moof, mdat, or payload) is cut
  off and the rest recovers; a file killed mid-`finalize` (full moov appended,
  header not flipped) recovers; zero fragments reports empty; finalized MP4s,
  foreign MP4s and unknown trun/tfhd flags are rejected untouched; explicit
  decode-time gaps and absorbed sub-frame video gaps survive recovery.
- [ ] Implement `HybridMp4Writer::resume_interrupted` (crate-internal) that
  validates the exact Hybrid header, rebuilds each `TrackConfig` from the init
  moov with the trim parser, replays every complete fragment through the same
  `TrackState` bookkeeping as `write_planned_fragment` (shared helper, not a
  copy), and positions at the end of the last complete fragment. Expose
  `finalize_interrupted_recording(&mut File)` returning
  `NotInterrupted | Empty | Finalized { fragments, discarded_tail_bytes }`.
  Order the in-place write as truncate tail, append moov, `sync_data`, flip
  header, `sync_data`, so a crash during recovery is itself recoverable.
- [ ] Add failing `clipline-storage` tests, then wire recovery: finalize each
  owned `.recording` before renaming it (no fragments counts as empty and is
  deleted as today), and finalize owned `.mp4` files that still carry the
  Hybrid header. Never touch unowned files. A file that fails to parse is
  left as is and reported, without aborting recovery of the others.
- [ ] Surface the counts through the existing startup warning in
  `service/session.rs`, keeping the internal-restart skip.
- [ ] Verify on copies of the five real sessions: packet counts match ffprobe
  of the original, the layout becomes ftyp/mdat/moov, and Chrome opens each
  with a handful of range requests. Then let the app repair the real files.
- [ ] Run workspace tests and fresh-cache warning-denied Clippy on Rust 1.98
  and 1.99, update the handoff, reopen Clipline, and create a PR into develop
  with green OS checks.

Plan checkboxes remain unticked by repository convention.
