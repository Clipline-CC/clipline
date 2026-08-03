use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use clipline_library::{
    CatalogItemIdentity, CloudAccountGeneration, CloudAccountKey, CloudPageNumber,
    CloudThumbnailDescriptor, CloudThumbnailManifest, CloudThumbnailOwner, CloudWorkToken,
    ForegroundGeneration, RemoteClipId, RequestGeneration, WindowAttachmentGeneration,
    MAX_DECODED_PAGE_IMAGES,
};
use clipline_slint_spike::cloud_thumbnail::{
    cloud_thumbnail_decode_pool_shape, validate_cloud_thumbnail_dimensions,
    CloudThumbnailDecodeOutcome, CloudThumbnailDecodePort, CloudThumbnailImageController,
    CloudThumbnailImageOwner,
};

struct PixelDecoder;

impl CloudThumbnailDecodePort for PixelDecoder {
    fn decode(
        &self,
        _owner: &CloudThumbnailOwner,
        cancellation: &clipline_library::cache::CloudCancellation,
    ) -> CloudThumbnailDecodeOutcome {
        if clipline_library::cache::CancellationProbe::is_cancelled(cancellation) {
            return CloudThumbnailDecodeOutcome::Stale;
        }
        let mut pixels = slint::SharedPixelBuffer::<slint::Rgb8Pixel>::new(1, 1);
        pixels.make_mut_slice()[0] = slint::Rgb8Pixel::new(1, 2, 3);
        CloudThumbnailDecodeOutcome::Ready { pixels }
    }
}

fn manifest(request: u64, count: usize) -> CloudThumbnailManifest {
    let token = CloudWorkToken {
        window: clipline_library::WindowWorkToken {
            attachment: WindowAttachmentGeneration::new(7),
            foreground: ForegroundGeneration::new(11),
            request: RequestGeneration::new(request),
        },
        account_key: CloudAccountKey::new("account-a").unwrap(),
        account_generation: CloudAccountGeneration::new(13),
    };
    let owners = (0..count)
        .map(|index| {
            let item = CatalogItemIdentity::Cloud {
                account_key: token.account_key.clone(),
                account_generation: token.account_generation,
                remote_clip_id: RemoteClipId::new(format!("remote-{index:03}")).unwrap(),
            };
            CloudThumbnailOwner::new(
                token.clone(),
                CloudThumbnailDescriptor::new(item, 20_000 + index as u64).unwrap(),
            )
            .unwrap()
        })
        .collect();
    CloudThumbnailManifest::new(token, CloudPageNumber::new(1).unwrap(), owners).unwrap()
}

fn ready_descriptors(page: &CloudThumbnailManifest) -> Vec<CloudThumbnailDescriptor> {
    page.owners()
        .iter()
        .map(|owner| owner.descriptor.clone())
        .collect()
}

#[test]
fn decode_pool_and_image_window_are_pinned_to_thirty_two() {
    assert_eq!(cloud_thumbnail_decode_pool_shape(), (2, 32));

    let page = manifest(17, 60);
    let ready = ready_descriptors(&page);
    let mut controller = CloudThumbnailImageController::<u32>::new();
    let update = controller.sync(Some(&page), &ready, 0).unwrap();
    assert_eq!(update.queued.len(), MAX_DECODED_PAGE_IMAGES);
    assert!(update.canceled.is_empty());
    assert!(update.released.is_empty());

    for (index, work) in update.queued.iter().enumerate() {
        let accepted = controller
            .accept_ready_with(work, || u32::try_from(index + 1).unwrap())
            .unwrap();
        assert!(accepted.is_some());
    }
    assert_eq!(controller.retained_count(), MAX_DECODED_PAGE_IMAGES);

    let moved = controller.sync(Some(&page), &ready, 28).unwrap();
    assert_eq!(moved.released.len(), 28);
    assert_eq!(moved.queued.len(), 28);
    assert!(moved.canceled.is_empty());
    assert_eq!(controller.retained_count(), 4);

    for work in &moved.queued {
        controller.accept_ready_with(work, || 99).unwrap().unwrap();
    }
    assert_eq!(controller.retained_count(), MAX_DECODED_PAGE_IMAGES);
}

#[test]
fn stale_completion_never_constructs_an_image_and_detach_releases_everything() {
    let first = manifest(21, 40);
    let second = manifest(22, 40);
    let first_ready = ready_descriptors(&first);
    let second_ready = ready_descriptors(&second);
    let mut controller = CloudThumbnailImageController::<u32>::new();
    let first_update = controller.sync(Some(&first), &first_ready, 0).unwrap();
    let stale = first_update.queued[0].clone();

    let replaced = controller.sync(Some(&second), &second_ready, 0).unwrap();
    assert_eq!(replaced.canceled.len(), MAX_DECODED_PAGE_IMAGES);
    assert!(
        replaced.queued.is_empty(),
        "canceled workers still own their slots"
    );
    assert!(replaced
        .canceled
        .iter()
        .all(clipline_library::cache::CancellationProbe::is_cancelled));

    let constructed = AtomicBool::new(false);
    let accepted = controller
        .accept_ready_with(&stale, || {
            constructed.store(true, Ordering::Release);
            1
        })
        .unwrap();
    assert!(accepted.is_none());
    assert!(!constructed.load(Ordering::Acquire));

    let one_slot = controller.sync(Some(&second), &second_ready, 0).unwrap();
    assert_eq!(one_slot.queued.len(), 1);
    for work in &one_slot.queued {
        controller.accept_ready_with(work, || 2).unwrap().unwrap();
    }
    for work in first_update.queued.iter().skip(1) {
        assert!(controller.accept_abandoned(work).unwrap());
    }
    let refill = controller.sync(Some(&second), &second_ready, 0).unwrap();
    for work in &refill.queued {
        controller.accept_ready_with(work, || 2).unwrap().unwrap();
    }
    let detached = controller.sync(None, &[], 0).unwrap();
    assert_eq!(detached.released.len(), MAX_DECODED_PAGE_IMAGES);
    assert_eq!(controller.retained_count(), 0);
    assert_eq!(controller.ownership_count(), 0);
}

#[test]
fn terminal_missing_or_failed_result_does_not_loop_until_the_page_changes() {
    let page = manifest(31, 2);
    let ready = ready_descriptors(&page);
    let mut controller = CloudThumbnailImageController::<u32>::new();
    let update = controller.sync(Some(&page), &ready, 0).unwrap();
    assert!(controller.accept_missing(&update.queued[0]).unwrap());
    assert!(controller.accept_failed(&update.queued[1]).unwrap());

    let repeated = controller.sync(Some(&page), &ready, 0).unwrap();
    assert!(repeated.queued.is_empty());

    let replacement = manifest(32, 2);
    let replacement_ready = ready_descriptors(&replacement);
    let refreshed = controller
        .sync(Some(&replacement), &replacement_ready, 0)
        .unwrap();
    assert_eq!(refreshed.queued.len(), 2);
}

#[test]
fn dimensions_are_bounded_before_rgb_allocation() {
    assert_eq!(
        validate_cloud_thumbnail_dimensions(640, 360).unwrap(),
        640 * 360 * 3
    );
    assert!(validate_cloud_thumbnail_dimensions(0, 360).is_err());
    assert!(validate_cloud_thumbnail_dimensions(8_193, 1).is_err());
    assert!(validate_cloud_thumbnail_dimensions(2_048, 2_048).is_err());
}

#[test]
fn fixed_workers_complete_thirty_two_images_without_unbounded_spawn() {
    let page = manifest(41, 60);
    let ready = ready_descriptors(&page);
    let mut owner = CloudThumbnailImageOwner::start(Arc::new(PixelDecoder)).unwrap();
    owner.reconcile(Some(&page), &ready, 0).unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while owner.retained_image_count() != MAX_DECODED_PAGE_IMAGES && Instant::now() < deadline {
        owner.pump_completions(None).unwrap();
        std::thread::yield_now();
    }
    assert_eq!(owner.retained_image_count(), MAX_DECODED_PAGE_IMAGES);
    assert_eq!(owner.ownership_count(), MAX_DECODED_PAGE_IMAGES);
    owner.shutdown().unwrap();
}

#[test]
fn shell_reconciles_exact_manifest_and_releases_images_before_models_and_cloud_runtime() {
    let shell = include_str!("../src/shell.rs");
    assert!(shell.contains("cloud_thumbnail_page()"));
    assert!(shell.contains("owner.pump_completions(window.as_ref())"));
    assert!(shell.contains("owner.image_for(identity)"));
    let detach = shell.find("owner.detach_window()?").unwrap();
    let clear = shell
        .find("clear_window_models(&resources.window)")
        .unwrap();
    assert!(detach < clear);
    let thumbnail_shutdown = shell.find("cloud_thumbnails.shutdown()").unwrap();
    let cloud_shutdown = shell.find("self.catalog_cloud.shutdown()").unwrap();
    assert!(thumbnail_shutdown < cloud_shutdown);

    let cloud = include_str!("../src/cloud.rs");
    assert!(cloud.contains("decode_cached_cloud_thumbnail(&cached)"));
    assert!(cloud.contains("invalidate_thumbnail("));
    assert!(cloud.contains("CloudAssetKind::Thumbnail"));
}
