use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::time::Duration;

use clipline_library::{
    CloudAccountGeneration, CloudAccountKey, CloudAvatar, CloudUserProfile, CloudWorkToken,
    ForegroundGeneration, RequestGeneration, WindowAttachmentGeneration, WindowWorkToken,
};
use clipline_slint_spike::cloud_profile::{
    cloud_profile_worker_shape, decode_bounded_avatar, rail_profile_initials, CloudAvatarOutcome,
    CloudProfileController, CloudProfileImageOwner, CloudProfileOutcome, CloudProfileRequestPort,
    CloudRailSeed, CloudRailWork, MAX_CLOUD_AVATAR_DIMENSION,
};
use image::codecs::{jpeg::JpegEncoder, png::PngEncoder};
use image::{ExtendedColorType, ImageEncoder as _};

fn token(attachment: u64, request: u64) -> CloudWorkToken {
    CloudWorkToken {
        window: WindowWorkToken {
            attachment: WindowAttachmentGeneration::new(attachment),
            foreground: ForegroundGeneration::new(11),
            request: RequestGeneration::new(request),
        },
        account_key: CloudAccountKey::new("account-a").unwrap(),
        account_generation: CloudAccountGeneration::new(7),
    }
}

fn profile(display_name: Option<&str>) -> CloudUserProfile {
    CloudUserProfile {
        user_id: "user-1".into(),
        username: "cloud_user".into(),
        display_name: display_name.map(str::to_owned),
        profile_url: "https://clips.example/u/cloud_user".into(),
    }
}

fn rgb_pixels() -> slint::SharedPixelBuffer<slint::Rgba8Pixel> {
    slint::SharedPixelBuffer::clone_from_slice(&[0x11, 0x22, 0x33, 0xff], 1, 1)
}

#[test]
fn exact_controller_replaces_both_lanes_and_constructs_only_the_current_avatar() {
    let mut controller = CloudProfileController::<String>::new();
    let first = controller
        .attach(CloudRailSeed::new(token(1, 1), "First User").unwrap())
        .unwrap();
    assert_eq!(controller.projection().name, "First User");
    assert_eq!(controller.projection().initials, "FU");

    let second = controller
        .attach(CloudRailSeed::new(token(2, 1), "second.user").unwrap())
        .unwrap();
    assert!(first.profile.is_cancelled());
    assert!(first.avatar.is_cancelled());

    assert!(!controller
        .accept_profile(
            &first.profile,
            CloudProfileOutcome::Ready(profile(Some("Stale")))
        )
        .unwrap());
    let constructed = AtomicUsize::new(0);
    assert!(controller
        .accept_avatar_with(
            &first.avatar,
            CloudAvatarOutcome::Ready(rgb_pixels()),
            |_| {
                constructed.fetch_add(1, Ordering::SeqCst);
                "stale".to_owned()
            }
        )
        .unwrap()
        .is_none());
    assert_eq!(constructed.load(Ordering::SeqCst), 0);

    assert!(controller
        .accept_profile(
            &second.profile,
            CloudProfileOutcome::Ready(profile(Some("Native Cloud"))),
        )
        .unwrap());
    assert_eq!(controller.projection().name, "Native Cloud");
    assert_eq!(controller.projection().initials, "NC");
    let image = controller
        .accept_avatar_with(
            &second.avatar,
            CloudAvatarOutcome::Ready(rgb_pixels()),
            |pixels| {
                constructed.fetch_add(1, Ordering::SeqCst);
                format!("{}x{}", pixels.width(), pixels.height())
            },
        )
        .unwrap();
    assert_eq!(image.as_deref(), Some("1x1"));
    assert_eq!(constructed.load(Ordering::SeqCst), 1);
    assert!(controller.projection().has_avatar);

    assert!(controller.detach());
    assert!(!controller.projection().visible);
    assert!(!controller.projection().has_avatar);
    assert!(second.profile.is_cancelled());
    assert!(second.avatar.is_cancelled());
}

#[test]
fn initials_match_the_shipping_name_fallback_rules() {
    assert_eq!(rail_profile_initials("Native Cloud"), "NC");
    assert_eq!(rail_profile_initials("cloud_user"), "CU");
    assert_eq!(rail_profile_initials("solo"), "SO");
    assert_eq!(rail_profile_initials(""), "C");
}

#[test]
fn jpeg_and_png_decode_under_explicit_avatar_allocation_bounds() {
    let pixels = [0x10, 0x20, 0x30, 0x40, 0x50, 0x60];
    let mut jpeg = Vec::new();
    JpegEncoder::new_with_quality(&mut jpeg, 85)
        .encode(&pixels, 2, 1, ExtendedColorType::Rgb8)
        .unwrap();
    let decoded = decode_bounded_avatar(&CloudAvatar {
        content_type: "image/jpeg".into(),
        etag: Some("jpeg-1".into()),
        bytes: jpeg,
    })
    .unwrap();
    assert_eq!((decoded.width(), decoded.height()), (2, 1));

    let mut png = Vec::new();
    let png_pixels = [0x10, 0x20, 0x30, 0x00, 0x40, 0x50, 0x60, 0xff];
    PngEncoder::new(&mut png)
        .write_image(&png_pixels, 2, 1, ExtendedColorType::Rgba8)
        .unwrap();
    let mut decoded = decode_bounded_avatar(&CloudAvatar {
        content_type: "image/png".into(),
        etag: Some("png-1".into()),
        bytes: png.clone(),
    })
    .unwrap();
    assert_eq!((decoded.width(), decoded.height()), (2, 1));
    assert_eq!(decoded.make_mut_bytes()[3], 0, "PNG alpha must survive");
    assert!(decode_bounded_avatar(&CloudAvatar {
        content_type: "image/jpeg".into(),
        etag: None,
        bytes: png,
    })
    .is_err());
    let pixel_overflow_width = 1_025;
    let pixel_overflow_height = 1_024;
    let pixel_overflow =
        vec![0x55; pixel_overflow_width as usize * pixel_overflow_height as usize * 3];
    let mut pixel_overflow_encoded = Vec::new();
    JpegEncoder::new_with_quality(&mut pixel_overflow_encoded, 80)
        .encode(
            &pixel_overflow,
            pixel_overflow_width,
            pixel_overflow_height,
            ExtendedColorType::Rgb8,
        )
        .unwrap();
    assert!(decode_bounded_avatar(&CloudAvatar {
        content_type: "image/jpeg".into(),
        etag: None,
        bytes: pixel_overflow_encoded,
    })
    .is_err());
    assert!(decode_bounded_avatar(&CloudAvatar {
        content_type: "image/jpeg".into(),
        etag: None,
        bytes: vec![0xff, 0xd8, 0xff, 0xd9],
    })
    .is_err());

    let oversized = vec![0x77; (MAX_CLOUD_AVATAR_DIMENSION as usize + 1) * 3];
    let mut encoded = Vec::new();
    JpegEncoder::new_with_quality(&mut encoded, 80)
        .encode(
            &oversized,
            MAX_CLOUD_AVATAR_DIMENSION + 1,
            1,
            ExtendedColorType::Rgb8,
        )
        .unwrap();
    assert!(decode_bounded_avatar(&CloudAvatar {
        content_type: "image/jpeg".into(),
        etag: None,
        bytes: encoded,
    })
    .is_err());
    assert!(decode_bounded_avatar(&CloudAvatar {
        content_type: "image/webp".into(),
        etag: None,
        bytes: vec![1, 2, 3],
    })
    .is_err());

    assert_eq!(cloud_profile_worker_shape(), (1, 1, 4));
}

struct LatestOnlyPort {
    started: Barrier,
    release_first: Barrier,
    profile_calls: AtomicUsize,
    avatar_calls: AtomicUsize,
}

impl LatestOnlyPort {
    fn wait_if_first(&self, work: &CloudRailWork) {
        if work.token.window.attachment == WindowAttachmentGeneration::new(1) {
            self.started.wait();
            while !work.is_cancelled() {
                std::thread::sleep(Duration::from_millis(1));
            }
            self.release_first.wait();
        }
    }
}

impl CloudProfileRequestPort for LatestOnlyPort {
    fn profile(&self, work: &CloudRailWork) -> CloudProfileOutcome {
        self.profile_calls.fetch_add(1, Ordering::SeqCst);
        self.wait_if_first(work);
        if work.is_cancelled() {
            CloudProfileOutcome::Stale
        } else {
            CloudProfileOutcome::Ready(profile(Some("Latest User")))
        }
    }

    fn avatar(&self, work: &CloudRailWork) -> CloudAvatarOutcome {
        self.avatar_calls.fetch_add(1, Ordering::SeqCst);
        self.wait_if_first(work);
        if work.is_cancelled() {
            CloudAvatarOutcome::Stale
        } else {
            CloudAvatarOutcome::Ready(rgb_pixels())
        }
    }
}

#[test]
fn worker_lanes_coalesce_pending_replacements_without_unbounded_spawn() {
    let port = Arc::new(LatestOnlyPort {
        started: Barrier::new(3),
        release_first: Barrier::new(3),
        profile_calls: AtomicUsize::new(0),
        avatar_calls: AtomicUsize::new(0),
    });
    let mut owner = CloudProfileImageOwner::start(port.clone()).unwrap();
    owner
        .attach(CloudRailSeed::new(token(1, 1), "First").unwrap())
        .unwrap();
    port.started.wait();
    owner
        .attach(CloudRailSeed::new(token(2, 1), "Second").unwrap())
        .unwrap();
    owner
        .attach(CloudRailSeed::new(token(3, 1), "Third").unwrap())
        .unwrap();
    // Keep both first requests in flight until the second pending request has
    // been replaced by the third. This makes the latest-only assertion about
    // mailbox behavior deterministic rather than scheduler-dependent.
    port.release_first.wait();

    let mut avatar_received = false;
    for _ in 0..100 {
        let pump = owner.pump_completions().unwrap();
        avatar_received |= pump.avatar.is_some();
        if owner.projection().name == "Latest User" && owner.projection().has_avatar {
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(owner.projection().name, "Latest User");
    assert!(owner.projection().has_avatar);
    assert!(avatar_received);
    assert_eq!(port.profile_calls.load(Ordering::SeqCst), 2);
    assert_eq!(port.avatar_calls.load(Ordering::SeqCst), 2);
    owner.detach_window();
    owner.shutdown();
}
