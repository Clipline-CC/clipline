//! Encoder capability model and runtime ranking (ddoc §4 encoder matrix).
//!
//! Two probes feed this: the Media Foundation probe (`windows::mft_probe`,
//! the proven zero-copy H.264 path) and the FFmpeg probe (`ffmpeg`,
//! NVENC/AMF/QSV plus software SVT-AV1, across AV1/HEVC/H.264). Both report
//! [`EncoderCapability`]; [`rank_encoders`] merges them into the ordered
//! list the recorder walks until one opens.

/// Which encode API provides a capability. The MFT path is preferred when
/// both offer the same backend+codec (zero-copy GPU; no DLL dependency).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum EncoderApi {
    /// Windows Media Foundation transform (zero-copy GPU H.264).
    Mft,
    /// Dynamically-loaded LGPL FFmpeg (libavcodec).
    Ffmpeg,
}

/// Hardware/software encoder engines in deterministic priority order
/// (ddoc §4: NVENC → AMF → QuickSync → software). `SvtAv1` is the LGPL
/// software AV1 tier (BSD-licensed, ships in LGPL FFmpeg builds — no GPL
/// x264/x265). `MfSoftware` (Microsoft's H.264 MFT) is the last resort.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum EncoderBackend {
    Nvenc,
    Amf,
    QuickSync,
    /// Software AV1 (SVT-AV1) via FFmpeg.
    SvtAv1,
    /// Microsoft software H.264 MFT — last resort.
    MfSoftware,
}

impl EncoderBackend {
    /// True for engines that live on the GPU, whose availability depends on
    /// silicon and driver rather than on our own code shipping. Only these
    /// need to prove themselves with a test encode; the software tiers
    /// (`SvtAv1`, `MfSoftware`) are self-contained.
    pub fn is_hardware(self) -> bool {
        match self {
            EncoderBackend::Nvenc | EncoderBackend::Amf | EncoderBackend::QuickSync => true,
            EncoderBackend::SvtAv1 | EncoderBackend::MfSoftware => false,
        }
    }
}

/// Drop hardware capabilities that cannot actually encode on this machine.
///
/// Being enumerable is not proof of being usable: Intel's H.264 MFT on
/// Alder Lake-N registers *and opens*, then fails the first frame with
/// `E_UNEXPECTED`. Advertising it makes the recorder commit to an encoder that
/// dies mid-session, so each hardware backend is confirmed by an actual encode
/// before it is offered — the same rule the FFmpeg probe already applies.
///
/// Software tiers are never consulted: `MfSoftware` is the inbox last resort
/// and does not depend on a GPU driver, so validating it could leave a machine
/// with no MFT tier at all.
pub fn retain_encodable_hardware(
    caps: Vec<EncoderCapability>,
    mut can_encode: impl FnMut(EncoderBackend) -> bool,
) -> Vec<EncoderCapability> {
    let mut verdicts: Vec<(EncoderBackend, bool)> = Vec::new();
    caps.into_iter()
        .filter(|cap| {
            if !cap.backend.is_hardware() {
                return true;
            }
            // Probing spawns a real encode, so ask at most once per backend
            // even when several capabilities share it.
            if let Some((_, ok)) = verdicts.iter().find(|(b, _)| *b == cap.backend) {
                return *ok;
            }
            let ok = can_encode(cap.backend);
            verdicts.push((cap.backend, ok));
            ok
        })
        .collect()
}

/// Default codec preference order: H.264 first for broad playback
/// compatibility, then HEVC/AV1 for explicit local-efficiency use cases.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Codec {
    H264,
    Hevc,
    Av1,
}

/// What one (api, backend) pair reported during startup probing.
#[derive(Debug, Clone)]
pub struct EncoderCapability {
    pub api: EncoderApi,
    pub backend: EncoderBackend,
    pub codecs: Vec<Codec>,
}

/// One concrete encoder the recorder can try to open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncoderCandidate {
    pub api: EncoderApi,
    pub backend: EncoderBackend,
    pub codec: Codec,
}

/// What the user asked for in Settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncoderPreference {
    /// Merit-order selection restricted to player-decodable codecs.
    Auto,
    /// A specific engine + codec; the recorder still falls back through the
    /// Auto order if it fails to open, but this is tried first even when the
    /// codec is not in-app-decodable (the user opted in).
    Explicit {
        backend: EncoderBackend,
        codec: Codec,
    },
}

/// Merit key: codec compatibility, then backend priority, then API (MFT before
/// FFmpeg for the same backend+codec). All three enums encode their order
/// via declaration order + derived `Ord`.
fn merit(c: &EncoderCandidate) -> (Codec, EncoderBackend, EncoderApi) {
    (c.codec, c.backend, c.api)
}

/// Rank every available encoder into the order the recorder should try,
/// best first. `decodable` is the set of codecs the in-app player can play;
/// `Auto` is restricted to it so we never silently record a clip the user
/// can't review (HEVC/AV1 without the OS codec extension). An `Explicit`
/// choice is tried first regardless, then the Auto order provides fallback.
pub fn rank_encoders(
    available: &[EncoderCapability],
    decodable: &[Codec],
    preference: EncoderPreference,
) -> Vec<EncoderCandidate> {
    let mut all: Vec<EncoderCandidate> = available
        .iter()
        .flat_map(|cap| {
            let (api, backend) = (cap.api, cap.backend);
            cap.codecs.iter().map(move |&codec| EncoderCandidate {
                api,
                backend,
                codec,
            })
        })
        .collect();
    all.sort_by_key(merit);
    all.dedup();

    let decodable_order: Vec<EncoderCandidate> = all
        .iter()
        .copied()
        .filter(|c| decodable.contains(&c.codec))
        .collect();

    match preference {
        EncoderPreference::Auto => decodable_order,
        EncoderPreference::Explicit { backend, codec } => {
            // Prefer MFT for the explicit combo (merit-sorted `all` lists it
            // first), then fall through the decodable Auto order.
            let head = all
                .iter()
                .copied()
                .find(|c| c.backend == backend && c.codec == codec);
            let mut out = Vec::with_capacity(decodable_order.len() + 1);
            out.extend(head);
            out.extend(decodable_order.into_iter().filter(|c| Some(*c) != head));
            out
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL_CODECS: &[Codec] = &[Codec::H264, Codec::Hevc, Codec::Av1];

    fn cap(api: EncoderApi, backend: EncoderBackend, codecs: &[Codec]) -> EncoderCapability {
        EncoderCapability {
            api,
            backend,
            codecs: codecs.to_vec(),
        }
    }

    fn cand(api: EncoderApi, backend: EncoderBackend, codec: Codec) -> EncoderCandidate {
        EncoderCandidate {
            api,
            backend,
            codec,
        }
    }

    #[test]
    fn only_gpu_backends_count_as_hardware() {
        assert!(EncoderBackend::Nvenc.is_hardware());
        assert!(EncoderBackend::Amf.is_hardware());
        assert!(EncoderBackend::QuickSync.is_hardware());
        assert!(!EncoderBackend::SvtAv1.is_hardware());
        assert!(!EncoderBackend::MfSoftware.is_hardware());
    }

    #[test]
    fn unencodable_hardware_is_dropped() {
        let caps = vec![
            cap(EncoderApi::Mft, EncoderBackend::QuickSync, &[Codec::H264]),
            cap(EncoderApi::Mft, EncoderBackend::MfSoftware, &[Codec::H264]),
        ];
        let kept = retain_encodable_hardware(caps, |_| false);
        assert_eq!(kept.len(), 1, "only the software tier survives");
        assert_eq!(kept[0].backend, EncoderBackend::MfSoftware);
    }

    #[test]
    fn encodable_hardware_is_kept_in_order() {
        let caps = vec![
            cap(EncoderApi::Mft, EncoderBackend::QuickSync, &[Codec::H264]),
            cap(EncoderApi::Mft, EncoderBackend::MfSoftware, &[Codec::H264]),
        ];
        let kept = retain_encodable_hardware(caps, |_| true);
        assert_eq!(kept.len(), 2);
        assert_eq!(kept[0].backend, EncoderBackend::QuickSync);
        assert_eq!(kept[1].backend, EncoderBackend::MfSoftware);
    }

    /// A test encode is expensive, and software tiers must never trigger one.
    #[test]
    fn software_tiers_are_never_probed() {
        let caps = vec![
            cap(EncoderApi::Mft, EncoderBackend::MfSoftware, &[Codec::H264]),
            cap(EncoderApi::Ffmpeg, EncoderBackend::SvtAv1, &[Codec::Av1]),
        ];
        let mut asked = Vec::new();
        let kept = retain_encodable_hardware(caps, |backend| {
            asked.push(backend);
            false
        });
        assert!(asked.is_empty(), "validator consulted for {asked:?}");
        assert_eq!(kept.len(), 2, "software tiers pass through untouched");
    }

    /// Several capabilities can share one backend; each probe is a real
    /// encode, so the verdict is reused rather than re-measured.
    #[test]
    fn each_hardware_backend_is_probed_once() {
        let caps = vec![
            cap(EncoderApi::Mft, EncoderBackend::Nvenc, &[Codec::H264]),
            cap(EncoderApi::Ffmpeg, EncoderBackend::Nvenc, &[Codec::Hevc]),
            cap(EncoderApi::Ffmpeg, EncoderBackend::Nvenc, &[Codec::Av1]),
        ];
        let mut calls = 0;
        let kept = retain_encodable_hardware(caps, |_| {
            calls += 1;
            true
        });
        assert_eq!(calls, 1, "one probe for three Nvenc capabilities");
        assert_eq!(kept.len(), 3);
    }

    #[test]
    fn empty_capabilities_stay_empty() {
        let kept = retain_encodable_hardware(Vec::new(), |_| panic!("must not probe"));
        assert!(kept.is_empty());
    }

    #[test]
    fn auto_prefers_h264_then_backend() {
        let caps = vec![
            cap(EncoderApi::Ffmpeg, EncoderBackend::Amf, &[Codec::Av1]),
            cap(
                EncoderApi::Ffmpeg,
                EncoderBackend::Nvenc,
                &[Codec::H264, Codec::Hevc],
            ),
        ];
        let ranked = rank_encoders(&caps, ALL_CODECS, EncoderPreference::Auto);
        // H.264 is the Automatic baseline; backend priority applies within
        // the same codec.
        assert_eq!(
            ranked[0],
            cand(EncoderApi::Ffmpeg, EncoderBackend::Nvenc, Codec::H264)
        );
        assert_eq!(
            ranked[1],
            cand(EncoderApi::Ffmpeg, EncoderBackend::Nvenc, Codec::Hevc)
        );
        assert_eq!(
            ranked[2],
            cand(EncoderApi::Ffmpeg, EncoderBackend::Amf, Codec::Av1)
        );
    }

    #[test]
    fn mft_outranks_ffmpeg_for_same_backend_and_codec() {
        let caps = vec![
            cap(EncoderApi::Ffmpeg, EncoderBackend::Nvenc, &[Codec::H264]),
            cap(EncoderApi::Mft, EncoderBackend::Nvenc, &[Codec::H264]),
        ];
        let ranked = rank_encoders(&caps, ALL_CODECS, EncoderPreference::Auto);
        assert_eq!(ranked[0].api, EncoderApi::Mft, "zero-copy MFT preferred");
        assert_eq!(ranked[1].api, EncoderApi::Ffmpeg);
    }

    #[test]
    fn auto_skips_codecs_the_player_cannot_decode() {
        // Hardware offers AV1/HEVC/H.264 but the player only decodes H.264:
        // Auto must still pick this backend, just on H.264.
        let caps = vec![cap(
            EncoderApi::Ffmpeg,
            EncoderBackend::Nvenc,
            &[Codec::Av1, Codec::Hevc, Codec::H264],
        )];
        let ranked = rank_encoders(&caps, &[Codec::H264], EncoderPreference::Auto);
        assert_eq!(
            ranked,
            vec![cand(EncoderApi::Ffmpeg, EncoderBackend::Nvenc, Codec::H264)]
        );
    }

    #[test]
    fn explicit_choice_is_tried_first_even_when_not_decodable() {
        let caps = vec![
            cap(EncoderApi::Mft, EncoderBackend::Amf, &[Codec::H264]),
            cap(
                EncoderApi::Ffmpeg,
                EncoderBackend::Amf,
                &[Codec::Av1, Codec::H264],
            ),
        ];
        // User forces AMF AV1; player can't decode AV1.
        let ranked = rank_encoders(
            &caps,
            &[Codec::H264],
            EncoderPreference::Explicit {
                backend: EncoderBackend::Amf,
                codec: Codec::Av1,
            },
        );
        assert_eq!(
            ranked[0],
            cand(EncoderApi::Ffmpeg, EncoderBackend::Amf, Codec::Av1)
        );
        // Then the decodable Auto order provides fallback (MFT H.264 first).
        assert_eq!(
            ranked[1],
            cand(EncoderApi::Mft, EncoderBackend::Amf, Codec::H264)
        );
        assert!(
            !ranked[1..].contains(&ranked[0]),
            "explicit head not duplicated"
        );
    }

    #[test]
    fn explicit_choice_absent_falls_back_to_auto() {
        let caps = vec![cap(
            EncoderApi::Mft,
            EncoderBackend::MfSoftware,
            &[Codec::H264],
        )];
        let ranked = rank_encoders(
            &caps,
            ALL_CODECS,
            EncoderPreference::Explicit {
                backend: EncoderBackend::Nvenc,
                codec: Codec::Av1,
            },
        );
        assert_eq!(
            ranked,
            vec![cand(
                EncoderApi::Mft,
                EncoderBackend::MfSoftware,
                Codec::H264
            )]
        );
    }

    #[test]
    fn microsoft_h264_fallback_ranks_above_software_av1() {
        let caps = vec![
            cap(EncoderApi::Mft, EncoderBackend::MfSoftware, &[Codec::H264]),
            cap(EncoderApi::Ffmpeg, EncoderBackend::SvtAv1, &[Codec::Av1]),
        ];
        let ranked = rank_encoders(&caps, ALL_CODECS, EncoderPreference::Auto);
        assert_eq!(ranked[0].backend, EncoderBackend::MfSoftware);
        assert_eq!(ranked[1].backend, EncoderBackend::SvtAv1);
    }

    #[test]
    fn no_encoders_means_empty() {
        assert!(rank_encoders(&[], ALL_CODECS, EncoderPreference::Auto).is_empty());
        let empty = vec![cap(EncoderApi::Mft, EncoderBackend::Nvenc, &[])];
        assert!(rank_encoders(&empty, ALL_CODECS, EncoderPreference::Auto).is_empty());
    }
}
