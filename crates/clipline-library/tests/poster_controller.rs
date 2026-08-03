use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use clipline_library::{
    ClipPathIdentity, ForegroundGeneration, PosterCompletion, PosterController, PosterFailureKind,
    PosterPageItem, PosterWorkKind, RequestGeneration, WindowAttachmentGeneration, WindowWorkToken,
    MAX_CATALOG_PAGE_ROWS, MAX_DECODED_PAGE_IMAGES, MAX_POSTER_RESULT_ENTRIES,
};

fn window(attachment: u64, foreground: u64, page: u64) -> WindowWorkToken {
    WindowWorkToken {
        attachment: WindowAttachmentGeneration::new(attachment),
        foreground: ForegroundGeneration::new(foreground),
        request: RequestGeneration::new(page),
    }
}

fn item(index: usize) -> PosterPageItem {
    PosterPageItem::new(
        PathBuf::from(format!(r"C:\clips\clip-{index}.mp4")),
        index as f64 / 10.0,
    )
    .unwrap()
}

fn poster(index: usize) -> PathBuf {
    PathBuf::from(format!(r"C:\clips\clip-{index}.poster.jpg"))
}

#[test]
fn viewport_queues_at_most_32_and_releases_every_handle_that_leaves_the_window() {
    let mut controller = PosterController::<u64>::new();
    let page: Vec<_> = (0..MAX_CATALOG_PAGE_ROWS).map(item).collect();
    controller.replace_page(window(1, 2, 3), page).unwrap();

    let first = controller.set_viewport(0, 20, 20).unwrap();
    assert_eq!(first.queued.len(), MAX_DECODED_PAGE_IMAGES);
    assert!(first.canceled.is_empty());
    assert!(first.released.is_empty());

    let extract = first.queued[0].clone();
    assert!(matches!(extract.kind, PosterWorkKind::Extract));
    let decode = controller.accept_extracted(&extract, PosterCompletion::Ready(poster(0)));
    assert_eq!(decode.queued.len(), 1);
    assert!(matches!(
        decode.queued[0].kind,
        PosterWorkKind::Decode { .. }
    ));
    assert!(controller
        .accept_decoded(&decode.queued[0], 99)
        .released
        .is_empty());
    assert_eq!(controller.retained_image_count(), 1);

    let moved = controller.set_viewport(40, 12, 4).unwrap();
    assert!(moved.queued.len() <= MAX_DECODED_PAGE_IMAGES);
    assert!(moved.canceled.len() <= MAX_DECODED_PAGE_IMAGES);
    assert_eq!(moved.released, vec![99]);
    assert_eq!(controller.retained_image_count(), 0);
}

#[test]
fn every_attachment_foreground_page_and_poster_generation_fences_stale_results() {
    let mut controller = PosterController::<u64>::new();
    controller
        .replace_page(window(1, 1, 1), vec![item(0)])
        .unwrap();
    let old = controller.set_viewport(0, 1, 0).unwrap().queued.remove(0);

    controller
        .replace_page(window(2, 3, 4), vec![item(0)])
        .unwrap();
    assert!(controller.set_viewport(0, 1, 0).unwrap().queued.is_empty());
    let mut resumed = controller.accept_extracted(&old, PosterCompletion::Ready(poster(0)));
    let current = resumed.queued.remove(0);
    assert_eq!(controller.queued_work_count(), 1);

    let decode = controller.accept_extracted(&current, PosterCompletion::Ready(poster(0)));
    let stale_decode = decode.queued[0].clone();
    controller.set_viewport(0, 1, 0).unwrap();
    let released = controller.accept_decoded(&stale_decode, 7);
    assert_eq!(released.released, vec![7]);
    assert_eq!(controller.retained_image_count(), 0);
}

#[test]
fn each_token_dimension_independently_rejects_a_stale_completion() {
    for mutation in 0..5 {
        let mut controller = PosterController::<u64>::new();
        controller
            .replace_page(window(1, 2, 3), vec![item(0)])
            .unwrap();
        let current = controller.set_viewport(0, 1, 0).unwrap().queued.remove(0);
        let mut stale = current.clone();
        match mutation {
            0 => stale.token.window.attachment = WindowAttachmentGeneration::new(9),
            1 => stale.token.window.foreground = ForegroundGeneration::new(9),
            2 => stale.token.window.request = RequestGeneration::new(9),
            3 => stale.token.poster = clipline_library::PosterGeneration::new(9),
            4 => {
                stale.token.path = item(9).identity;
            }
            _ => unreachable!(),
        }
        let ignored = controller.accept_extracted(&stale, PosterCompletion::Ready(poster(0)));
        assert!(ignored.queued.is_empty());
        assert_eq!(controller.ownership_count(), 1);
        assert!(controller.accepts_request(&current));
    }
}

#[test]
fn cache_is_lru_bounded_to_120_including_negative_results() {
    let mut controller = PosterController::<u64>::new();
    for index in 0..=MAX_POSTER_RESULT_ENTRIES {
        controller
            .replace_page(window(1, 1, index as u64), vec![item(index)])
            .unwrap();
        let request = controller.set_viewport(0, 1, 0).unwrap().queued.remove(0);
        let completion = if index.is_multiple_of(2) {
            PosterCompletion::Missing
        } else {
            PosterCompletion::Failed(PosterFailureKind::Corrupt)
        };
        let _ = controller.accept_extracted(&request, completion);
    }

    assert_eq!(controller.cache_len(), MAX_POSTER_RESULT_ENTRIES);
    assert!(!controller.cache_contains(&item(0).identity));
    assert!(controller.cache_contains(&item(MAX_POSTER_RESULT_ENTRIES).identity));
}

#[test]
fn hide_and_path_invalidation_cancel_work_release_images_and_reject_old_completions() {
    let mut controller = PosterController::<u64>::new();
    controller
        .replace_page(window(1, 1, 1), vec![item(0), item(1)])
        .unwrap();
    let queued = controller.set_viewport(0, 2, 0).unwrap().queued;
    let decode = controller.accept_extracted(&queued[0], PosterCompletion::Ready(poster(0)));
    let _ = controller.accept_decoded(&decode.queued[0], 11);

    let invalidated = controller.invalidate_path(&item(0).identity).unwrap();
    assert_eq!(invalidated.released, vec![11]);
    assert!(!controller.cache_contains(&item(0).identity));

    let hidden = controller.hide().unwrap();
    assert_eq!(hidden.canceled.len(), 1);
    assert_eq!(controller.retained_image_count(), 0);
    assert_eq!(controller.queued_work_count(), 0);

    let stale = controller.accept_extracted(&queued[1], PosterCompletion::Ready(poster(1)));
    assert!(stale.queued.is_empty());
}

#[test]
fn duplicate_or_oversized_pages_fail_atomically() {
    let mut controller = PosterController::<u64>::new();
    let duplicate = vec![item(0), item(0)];
    assert!(controller.replace_page(window(1, 1, 1), duplicate).is_err());
    assert_eq!(controller.page_len(), 0);

    let oversized: Vec<_> = (0..=MAX_CATALOG_PAGE_ROWS).map(item).collect();
    assert!(controller.replace_page(window(1, 1, 1), oversized).is_err());
    assert_eq!(controller.page_len(), 0);

    let identity = ClipPathIdentity::from_text(r"C:\clips\clip-0.mp4").unwrap();
    assert!(!controller.cache_contains(&identity));
}

#[test]
fn old_page_work_keeps_its_leases_until_cancellation_is_acknowledged() {
    let mut controller = PosterController::<u64>::new();
    controller
        .replace_page(
            window(1, 1, 1),
            (0..MAX_CATALOG_PAGE_ROWS).map(item).collect(),
        )
        .unwrap();
    let old = controller.set_viewport(0, 32, 0).unwrap().queued;
    assert_eq!(old.len(), MAX_DECODED_PAGE_IMAGES);
    assert_eq!(controller.ownership_count(), MAX_DECODED_PAGE_IMAGES);

    controller
        .replace_page(
            window(1, 1, 2),
            (100..100 + MAX_CATALOG_PAGE_ROWS).map(item).collect(),
        )
        .unwrap();
    let blocked = controller.set_viewport(0, 32, 0).unwrap();
    assert!(blocked.queued.is_empty());
    assert_eq!(controller.ownership_count(), MAX_DECODED_PAGE_IMAGES);

    let first = controller.acknowledge_canceled(&old[0]).unwrap();
    assert_eq!(first.queued.len(), 1);
    assert_eq!(controller.ownership_count(), MAX_DECODED_PAGE_IMAGES);
    let second = controller.acknowledge_canceled(&old[1]).unwrap();
    assert_eq!(second.queued.len(), 1);
    assert_eq!(controller.ownership_count(), MAX_DECODED_PAGE_IMAGES);
}

#[test]
fn acknowledging_current_work_does_not_release_its_lease() {
    let mut controller = PosterController::<u64>::new();
    controller
        .replace_page(window(1, 1, 1), (0..32).map(item).collect())
        .unwrap();
    let request = controller.set_viewport(0, 32, 0).unwrap().queued[0].clone();
    let ownership_before = controller.ownership_count();
    let queued_before = controller.queued_work_count();

    let update = controller.acknowledge_canceled(&request).unwrap();

    assert!(update.queued.is_empty());
    assert!(update.canceled.is_empty());
    assert!(update.released.is_empty());
    assert_eq!(controller.ownership_count(), ownership_before);
    assert_eq!(controller.queued_work_count(), queued_before);
    assert!(controller.accepts_request(&request));
}

#[test]
fn a_stale_window_detach_cannot_clear_replacement_work() {
    let mut controller = PosterController::<u64>::new();
    controller
        .replace_page(window(1, 1, 1), vec![item(0)])
        .unwrap();
    let old = controller.set_viewport(0, 1, 0).unwrap().queued[0].clone();

    controller
        .replace_page(window(2, 2, 2), vec![item(1)])
        .unwrap();
    let replacement = controller.set_viewport(0, 1, 0).unwrap().queued[0].clone();
    let ownership_before = controller.ownership_count();

    let update = controller
        .detach_window_if_matches(old.token.window)
        .unwrap();

    assert!(update.queued.is_empty());
    assert!(update.canceled.is_empty());
    assert!(update.released.is_empty());
    assert_eq!(controller.ownership_count(), ownership_before);
    assert!(controller.accepts_request(&replacement));

    controller.acknowledge_canceled(&old).unwrap();
    assert!(controller.accepts_request(&replacement));
}

#[test]
fn mixed_extract_decode_and_retained_phases_never_exceed_thirty_two() {
    let mut controller = PosterController::<u64>::new();
    controller
        .replace_page(window(1, 1, 1), (0..32).map(item).collect())
        .unwrap();
    let extracts = controller.set_viewport(0, 32, 0).unwrap().queued;
    let mut decodes = Vec::new();
    for (index, extract) in extracts.iter().enumerate() {
        let update = controller.accept_extracted(extract, PosterCompletion::Ready(poster(index)));
        assert_eq!(update.queued.len(), 1);
        decodes.push(update.queued[0].clone());
        assert_eq!(controller.ownership_count(), MAX_DECODED_PAGE_IMAGES);
    }
    for (index, decode) in decodes.iter().take(16).enumerate() {
        let update = controller.accept_decoded(decode, index as u64);
        assert!(update.released.is_empty());
        assert_eq!(controller.ownership_count(), MAX_DECODED_PAGE_IMAGES);
    }
    assert_eq!(controller.retained_image_count(), 16);
    assert_eq!(controller.queued_work_count(), 16);
}

#[test]
fn final_stale_check_runs_before_constructing_the_ui_handle() {
    let mut controller = PosterController::<u64>::new();
    controller
        .replace_page(window(1, 1, 1), vec![item(0)])
        .unwrap();
    let extract = controller.set_viewport(0, 1, 0).unwrap().queued.remove(0);
    let decode = controller.accept_extracted(&extract, PosterCompletion::Ready(poster(0)));
    let decode = decode.queued[0].clone();
    controller.hide().unwrap();

    let constructed = AtomicBool::new(false);
    let result = controller
        .accept_decoded_with(&decode, || {
            constructed.store(true, Ordering::SeqCst);
            9
        })
        .unwrap();
    assert!(!constructed.load(Ordering::SeqCst));
    assert!(result.released.is_empty());
    assert_eq!(controller.ownership_count(), 0);
}

#[test]
fn negative_cache_retries_after_thirty_seconds_without_sleeping() {
    let now = Instant::now();
    let mut controller = PosterController::<u64>::new();
    controller
        .replace_page(window(1, 1, 1), vec![item(0)])
        .unwrap();
    let request = controller
        .set_viewport_at(0, 1, 0, now)
        .unwrap()
        .queued
        .remove(0);
    controller
        .accept_extracted_at(&request, PosterCompletion::Missing, now)
        .unwrap();

    assert!(controller
        .set_viewport_at(0, 1, 0, now + Duration::from_secs(29))
        .unwrap()
        .queued
        .is_empty());
    let retry = controller
        .set_viewport_at(0, 1, 0, now + Duration::from_secs(30))
        .unwrap();
    assert_eq!(retry.queued.len(), 1);
    assert!(matches!(retry.queued[0].kind, PosterWorkKind::Extract));
}

#[test]
fn hidden_or_invalidated_rows_cannot_be_requeued() {
    let mut controller = PosterController::<u64>::new();
    controller
        .replace_page(window(1, 1, 1), vec![item(0), item(1)])
        .unwrap();
    let queued = controller.set_viewport(0, 2, 0).unwrap().queued;
    let invalidated = controller.invalidate_path(&item(0).identity).unwrap();
    assert_eq!(invalidated.canceled.len(), 1);
    assert_eq!(controller.page_len(), 1);

    controller.hide().unwrap();
    let hidden = controller.set_viewport(0, 1, 0).unwrap();
    assert!(hidden.queued.is_empty());
    let stale = controller.accept_extracted(&queued[0], PosterCompletion::Ready(poster(0)));
    assert!(stale.queued.is_empty());
}

#[test]
fn an_unexpected_artifact_path_is_never_sent_to_the_decoder() {
    let mut controller = PosterController::<u64>::new();
    controller
        .replace_page(window(1, 1, 1), vec![item(0)])
        .unwrap();
    let request = controller.set_viewport(0, 1, 0).unwrap().queued.remove(0);
    let update = controller.accept_extracted(
        &request,
        PosterCompletion::Ready(PathBuf::from(r"C:\foreign\poster.jpg")),
    );
    assert!(update.queued.is_empty());
    assert_eq!(controller.queued_work_count(), 0);
}
