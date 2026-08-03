use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use clipline_library::{
    ForegroundGeneration, PosterCompletion, PosterController, PosterGeneration, PosterPageItem,
    PosterWorkKind, PosterWorkRequest, PosterWorkToken, RequestGeneration,
    WindowAttachmentGeneration, WindowWorkToken, MAX_POSTER_DECODED_PIXELS, MAX_POSTER_DIMENSION,
    MAX_POSTER_ENCODED_BYTES,
};
use clipline_slint_spike::poster::{
    clone_ui_poster, decode_poster_file, poster_decode_pool_shape, poster_extraction_pool_shape,
    publish_decoded_poster, validate_poster_dimensions, with_ui_poster_controller, DecodedPoster,
    PosterAdapterError,
};
use image::codecs::jpeg::JpegEncoder;
use image::ExtendedColorType;

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new(case: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "clipline-slint-poster-{case}-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn window() -> WindowWorkToken {
    WindowWorkToken {
        attachment: WindowAttachmentGeneration::new(1),
        foreground: ForegroundGeneration::new(2),
        request: RequestGeneration::new(3),
    }
}

fn decode_request(clip: &Path, poster: &Path) -> PosterWorkRequest {
    let item = PosterPageItem::new(clip.to_path_buf(), 0.0).unwrap();
    PosterWorkRequest {
        token: PosterWorkToken {
            window: window(),
            poster: PosterGeneration::new(4),
            path: item.identity.clone(),
        },
        item,
        kind: PosterWorkKind::Decode {
            encoded_path: poster.to_path_buf(),
        },
    }
}

fn write_jpeg(path: &Path, width: u32, height: u32) {
    let pixels = vec![91_u8; (u64::from(width) * u64::from(height) * 3) as usize];
    let file = std::fs::File::create(path).unwrap();
    JpegEncoder::new_with_quality(file, 80)
        .encode(&pixels, width, height, ExtendedColorType::Rgb8)
        .unwrap();
}

#[test]
fn valid_jpeg_decodes_to_an_exact_sendable_rgb8_buffer_and_closes_the_file() {
    fn assert_send<T: Send>() {}
    assert_send::<DecodedPoster>();

    let directory = TestDirectory::new("valid");
    let clip = directory.0.join("clip.mp4");
    let poster = directory.0.join("clip.poster.jpg");
    std::fs::write(&clip, b"clip").unwrap();
    write_jpeg(&poster, 8, 6);

    let decoded = decode_poster_file(decode_request(&clip, &poster)).unwrap();
    assert_eq!(decoded.width(), 8);
    assert_eq!(decoded.height(), 6);
    std::fs::rename(&poster, directory.0.join("moved.poster.jpg")).unwrap();
}

#[test]
fn corrupt_and_oversized_files_fail_before_publication() {
    let directory = TestDirectory::new("invalid");
    let clip = directory.0.join("clip.mp4");
    let poster = directory.0.join("clip.poster.jpg");
    std::fs::write(&clip, b"clip").unwrap();
    std::fs::write(&poster, b"not a jpeg").unwrap();
    assert_eq!(
        decode_poster_file(decode_request(&clip, &poster)).unwrap_err(),
        PosterAdapterError::CorruptJpeg
    );
    assert!(!poster.exists(), "corrupt owned cache must be invalidated");

    std::fs::write(&poster, vec![0_u8; MAX_POSTER_ENCODED_BYTES + 1]).unwrap();
    assert_eq!(
        decode_poster_file(decode_request(&clip, &poster)).unwrap_err(),
        PosterAdapterError::EncodedTooLarge
    );
    assert!(
        !poster.exists(),
        "oversized owned cache must be invalidated"
    );
}

#[test]
fn a_forged_foreign_encoded_path_is_rejected_without_opening_or_deleting_it() {
    let directory = TestDirectory::new("foreign");
    let clip = directory.0.join("clip.mp4");
    let foreign = directory.0.join("foreign.jpg");
    std::fs::write(&clip, b"clip").unwrap();
    std::fs::write(&foreign, b"not a jpeg").unwrap();

    assert_eq!(
        decode_poster_file(decode_request(&clip, &foreign)).unwrap_err(),
        PosterAdapterError::InvalidRequest
    );
    assert_eq!(std::fs::read(&foreign).unwrap(), b"not a jpeg");
}

#[test]
fn dimension_pixel_and_rgb_bounds_are_checked_before_decode_allocation() {
    assert_eq!(
        validate_poster_dimensions(0, 1),
        Err(PosterAdapterError::InvalidDimensions)
    );
    assert_eq!(
        validate_poster_dimensions(MAX_POSTER_DIMENSION + 1, 1),
        Err(PosterAdapterError::InvalidDimensions)
    );
    let too_many_pixels = (MAX_POSTER_DIMENSION, 129);
    assert!(
        u64::from(too_many_pixels.0) * u64::from(too_many_pixels.1) > MAX_POSTER_DECODED_PIXELS
    );
    assert_eq!(
        validate_poster_dimensions(too_many_pixels.0, too_many_pixels.1),
        Err(PosterAdapterError::PixelLimit)
    );
}

#[test]
fn image_is_constructed_only_after_the_controller_accepts_the_exact_token() {
    let directory = TestDirectory::new("publish");
    let clip = directory.0.join("clip.mp4");
    let poster = directory.0.join("clip.poster.jpg");
    std::fs::write(&clip, b"clip").unwrap();
    write_jpeg(&poster, 4, 4);

    let item = PosterPageItem::new(clip.clone(), 0.0).unwrap();
    let mut controller = PosterController::<slint::Image>::new();
    controller.replace_page(window(), vec![item]).unwrap();
    let extract = controller.set_viewport(0, 1, 0).unwrap().queued.remove(0);
    let decode = controller.accept_extracted(&extract, PosterCompletion::Ready(poster.clone()));
    let decoded = decode_poster_file(decode.queued[0].clone()).unwrap();
    publish_decoded_poster(&mut controller, decoded).unwrap();
    assert_eq!(controller.retained_image_count(), 1);
}

#[test]
fn ui_poster_clone_accessor_returns_only_the_exact_retained_identity() {
    let directory = TestDirectory::new("clone-accessor");
    let clip = directory.0.join("clip.mp4");
    let other_clip = directory.0.join("other.mp4");
    let poster = directory.0.join("clip.poster.jpg");
    std::fs::write(&clip, b"clip").unwrap();
    write_jpeg(&poster, 4, 4);

    let item = PosterPageItem::new(clip.clone(), 0.0).unwrap();
    let identity = item.identity.clone();
    let other_identity = PosterPageItem::new(other_clip, 0.0).unwrap().identity;
    let decode = with_ui_poster_controller(|controller| {
        controller.replace_page(window(), vec![item]).unwrap();
        let extract = controller.set_viewport(0, 1, 0).unwrap().queued.remove(0);
        controller
            .accept_extracted(&extract, PosterCompletion::Ready(poster))
            .queued
            .remove(0)
    });
    let decoded = decode_poster_file(decode).unwrap();
    with_ui_poster_controller(|controller| publish_decoded_poster(controller, decoded).unwrap());

    assert!(clone_ui_poster(&identity).is_some());
    assert!(clone_ui_poster(&other_identity).is_none());
    with_ui_poster_controller(|controller| {
        let _ = controller.detach_window().unwrap();
    });
    assert!(clone_ui_poster(&identity).is_none());
}

#[test]
fn adapter_source_forbids_path_loading_and_text_encoded_images() {
    let source = include_str!("../src/poster.rs");
    assert!(!source.contains("Image::load_from_path"));
    assert!(!source.contains("base64"));
    assert!(!source.contains("data:"));
    let check = source.find("accept_decoded_with").unwrap();
    let construct = source.find("Image::from_rgb8").unwrap();
    assert!(check < construct);
    assert!(source.contains("slint::Weak<CliplineSpike>"));
    assert!(source.contains("slint::invoke_from_event_loop"));
    assert!(source.contains("window.upgrade().is_some()"));
}

#[test]
fn decode_dispatch_uses_a_fixed_bounded_pool_instead_of_a_thread_per_poster() {
    let (workers, queue_capacity) = poster_decode_pool_shape();
    assert_eq!(workers, 2);
    assert_eq!(queue_capacity, clipline_library::MAX_DECODED_PAGE_IMAGES);

    let source = include_str!("../src/poster.rs");
    let enqueue = source.find("pub fn spawn_poster_decode").unwrap();
    let enqueue_end = source[enqueue..].find("\nfn poster_decode_pool").unwrap() + enqueue;
    let enqueue_body = &source[enqueue..enqueue_end];
    assert!(enqueue_body.contains("try_send"));
    assert!(!enqueue_body.contains("Builder::new()\n        .name(\"clipline-poster-decode\""));

    let (extract_workers, extract_capacity) = poster_extraction_pool_shape();
    assert_eq!(extract_workers, 2);
    assert_eq!(extract_capacity, clipline_library::MAX_DECODED_PAGE_IMAGES);
    assert!(source.contains("PosterService::standard()"));
    assert!(source.contains("clipline-poster-extract-{index}"));
}

#[test]
fn shell_routes_exact_poster_results_and_releases_images_before_model_clear() {
    let shell = include_str!("../src/shell.rs");
    assert!(shell.contains("replace_ui_poster_page("));
    assert!(shell.contains("resources.poster_viewport_start = start"));
    assert!(shell.contains("poster_page.items,\n                poster_viewport_start,"));
    assert!(shell.contains("CatalogItemIdentity::Local { path } => clone_ui_poster(path)"));
    let detach = shell.find("detach_ui_poster_window(poster_token)").unwrap();
    let clear = shell
        .find("clear_window_models(&resources.window)")
        .unwrap();
    assert!(detach < clear);

    let source = include_str!("../src/poster.rs");
    assert!(source.contains("ExpectedResultOwner::Poster(request.token.clone())"));
    assert!(source.contains("try_send_recoverable"));
    assert!(source.contains("accepts_request(&request)"));
}
