# MFT Hardware Encoder Validation Plan

**Goal:** Stop advertising a hardware H.264 MFT that cannot actually encode. Media Foundation
registration is not proof of function: on Intel Alder Lake-N (N97, UHD Graphics, driver
32.0.101.7082) the vendor H.264 MFT enumerates *and opens successfully*, then fails on the first
`ProcessInput` with `E_UNEXPECTED` (`0x8000FFFF`). The recorder had already committed to it, so
recording dies with the toast `recording: encoder failed: Catastrophic failure (0x8000FFFF)` even
though four working encoders exist on the machine.

## Observed failure

- `mft_probe::enumerate()` reports `api=Mft backend=QuickSync codec=H264` from registration alone.
- `merit()` (`probe.rs`) ranks MFT above FFmpeg for the same backend+codec, so Automatic picks it.
- `open_candidate` (`service.rs:1872`) passes `encoder_backend: Some(QuickSync)`; `MftH264Encoder::new`
  returns `Ok`, and `encoder_selected` is logged.
- `Encoder::encode` accepts frame 0 (zero packets), then returns
  `Backend("Catastrophic failure (0x8000FFFF)")` on frame 1.
- `select_encoder` only falls back on *open* failure (`service.rs:1780-1817`), so a first-frame
  failure aborts the session instead of downgrading.
- Reproduced by the existing device test `windows::mft::tests::encodes_synthetic_frames_to_keyframed_avcc`,
  which fails at `mft.rs:1350` on this machine in 0.41s.

The same silicon encodes H.264 fine through oneVPL/libmfx (`h264_qsv` passes a real encode), so this
is specific to the Media Foundation hardware encoder, not to Quick Sync as such.

## Product decisions

- **Validate hardware MFTs with a real one-frame encode**, exactly the discipline the FFmpeg probe
  already applies and documents (`ffmpeg.rs`: *"`ffmpeg -encoders` lists every compiled encoder
  regardless of hardware, so each hardware encoder is confirmed with a one-frame test encode"*). The
  MFT half was the only probe trusting registration.
- **Probe at 640x360**, matching the FFmpeg probe's size and for the same reason (AMF rejects very
  small resolutions; a tiny probe would wrongly drop a working encoder).
- **One frame is not enough; probe 8 and drain.** Measured on the affected machine: frame 0 is
  accepted and returns `Ok` with zero packets, and frame 1 fails. An async MFT banks the first
  `ProcessInput` against its NeedInput credit without doing encode work, so a single-frame probe
  passes on a broken encoder. The probe runs 8 frames, calls `finish()`, and additionally requires at
  least one packet — a silent encoder is no more usable than an erroring one. The full 30-frame
  hardware test runs in well under a second, so 8 frames is negligible at startup.
- **Software tiers are exempt from validation.** `MfSoftware` is Microsoft's inbox encoder and does
  not depend on GPU drivers; it is the last-resort fallback, so validating it could leave the machine
  with no MFT tier at all. This mirrors `requires_test_encode` exempting `SvtAv1` on the FFmpeg side.
- **Failure to create a D3D device means "cannot encode"**, not an error. Headless CI has no hardware
  MFT to advertise anyway, and probing must never fail startup.
- Do **not** add runtime mid-session encoder fallback in this change. It is a real gap (a failure
  after open still aborts the recording) but it touches the live recorder with a running buffer and
  clock; it is tracked separately below.
- Do **not** change `merit()` ordering. MFT-before-FFmpeg stays correct once the probe only
  advertises MFTs that work.

## Minimal architecture

Neutral logic in `probe.rs` (compiled and tested on both CI OSes), Windows-only glue behind it:

- `probe.rs` — `EncoderBackend::is_hardware()` (pure) and
  `retain_encodable_hardware(caps, can_encode)` (pure): drops any capability whose backend
  `is_hardware()` and whose `can_encode(backend)` is false, preserving order and leaving software
  tiers untouched. Fully unit-testable with a stub closure, no hardware.
- `windows/mft.rs` — `hardware_backend_can_encode(backend)`: hardware D3D11 device via
  `d3d11::create_device()`, `MftH264Encoder::new` with `encoder_backend: Some(backend)` at 640x360,
  one `create_bgra_texture` frame through `Encoder::encode`. `true` only on `Ok`. All COM released on
  drop.
- `windows/mft_probe.rs` — split the current body into `enumerate_registered()` (registration only,
  unchanged semantics) and `enumerate()` = `enumerate_registered()` filtered through
  `retain_encodable_hardware` with the real validator. `enumerate_with_validator()` exposes the seam
  so tests inject a stub instead of touching hardware.

Call site `service.rs::mft_capabilities_cached()` keeps calling `mft_probe::enumerate()` and needs no
change; it is already cached in a `OnceLock`, so validation runs once per process.

## Steps

- [ ] **Step 1: Failing test — advertised hardware must encode.** In `mft_probe.rs`, assert that
      every hardware capability returned by `enumerate()` can encode one frame. Fails on this machine
      today (QuickSync advertised, cannot encode); passes trivially where no hardware MFT exists.
- [ ] **Step 2: Neutral pure-logic tests.** In `probe.rs`, test `is_hardware()` for all five backends
      and `retain_encodable_hardware` for: hardware dropped when the validator says no, hardware kept
      when yes, software never consulted, order preserved, empty input.
- [ ] **Step 3: Implement `retain_encodable_hardware` + `is_hardware`** in `probe.rs`.
- [ ] **Step 4: Implement `hardware_backend_can_encode`** in `windows/mft.rs`.
- [ ] **Step 5: Wire `mft_probe::enumerate()`** through the validator, keeping
      `enumerate_registered()` for diagnostics and the injectable seam for tests.
- [ ] **Step 6: Re-gate the existing hardware device tests.** Two tests build an encoder with
      `encoder_backend: None`, which takes the first *registered* hardware MFT and so still fails on
      this machine after the fix — `mft.rs::encodes_synthetic_frames_to_keyframed_avcc` and
      `wgc.rs::real_engines_on_one_clock_produce_a_synced_timeline`. Both were already failing here
      before this change (verified by stashing it), for the same reason the app was. Gate each on
      `enumerate()` advertising a hardware backend and encode with that backend explicitly, so they
      self-skip on hardware without a working hardware encoder — the same shape as
      `advertised_software_mft_encodes_warp_frames`. Their `SKIP` on a machine with a working
      hardware MFT is unchanged.
- [ ] **Step 7: Quality gates.** `cargo test --workspace` green, `cargo clippy --workspace
      --all-targets -- -D warnings` clean, then verify in the app that Automatic records
      successfully on this machine (expected selection: `api=Ffmpeg backend=QuickSync codec=H264`).

## Verification on this machine

Expected after the change: `enumerate()` no longer reports `Mft`/`QuickSync`; `MfSoftware` still
reported; `encoder_selected` logs an FFmpeg-tier encoder; a replay saves without the catastrophic
failure toast. Expected on hardware with a working MFT: no behavior change beyond one extra
one-frame encode during first probe.

## Follow-up (not in this change)

- **Mid-session encoder fallback.** `select_encoder` only downgrades on open failure. An encoder that
  dies after open still kills the recording. Worth handling now that we know registered MFTs can fail
  late.
- **`available_encoder_options` dedupes by `(backend, codec)` and ignores `api`** (`service.rs:381`),
  so the working FFmpeg QSV encoder collapses into the same dropdown entry as the MFT one and cannot
  be chosen independently from Settings.
