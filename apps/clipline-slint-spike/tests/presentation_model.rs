use clipline_slint_spike::model::{
    format_clock, representative_library, Marker, MarkerCategory, PresentationModelError,
    ReviewState, SpikeView, VISIBLE_LIBRARY_ROWS,
};

#[test]
fn representative_library_has_exactly_twenty_four_bounded_rows() {
    let rows = representative_library();
    assert_eq!(rows.len(), VISIBLE_LIBRARY_ROWS);
    assert_eq!(VISIBLE_LIBRARY_ROWS, 24);
    assert!(rows.iter().all(|row| !row.title.is_empty()));
    assert!(rows.iter().all(|row| row.title.len() <= 64));
    assert!(rows.iter().all(|row| row.subtitle.len() <= 96));
    assert!(rows
        .iter()
        .all(|row| row.duration_ticks <= 48_000 * 60 * 60));
    assert_eq!(rows.first().unwrap().poster_seed, 0);
    assert_eq!(rows.last().unwrap().poster_seed, 23);
}

#[test]
fn markers_sort_stably_and_reject_out_of_range_positions() {
    let mut review = ReviewState::new(48_000 * 10);
    review
        .set_markers(vec![
            Marker::new(96_000, "Second A", MarkerCategory::Event).unwrap(),
            Marker::new(48_000, "First", MarkerCategory::Bookmark).unwrap(),
            Marker::new(96_000, "Second B", MarkerCategory::Kill).unwrap(),
        ])
        .unwrap();

    let labels: Vec<_> = review
        .markers()
        .iter()
        .map(|marker| marker.label.as_str())
        .collect();
    assert_eq!(labels, ["First", "Second A", "Second B"]);

    let error = review
        .set_markers(vec![Marker::new(
            48_000 * 11,
            "Too late",
            MarkerCategory::Event,
        )
        .unwrap()])
        .unwrap_err();
    assert_eq!(
        error,
        PresentationModelError::MarkerBeyondDuration {
            position_ticks: 48_000 * 11,
            duration_ticks: 48_000 * 10,
        }
    );
}

#[test]
fn review_state_clamps_transport_and_tracks_without_duplicates() {
    let mut review = ReviewState::new(480_000);
    review.set_position(600_000);
    review.set_playing(true);
    review.set_volume(1.5).unwrap();
    review.set_track_selected(7, true).unwrap();
    review.set_track_selected(7, true).unwrap();
    review.set_track_selected(9, true).unwrap();

    assert_eq!(review.position_ticks(), 480_000);
    assert!(review.is_playing());
    assert_eq!(review.volume(), 1.0);
    assert_eq!(review.selected_tracks(), &[7, 9]);
    assert_eq!(review.transport_label(), "Pause");

    review.set_track_selected(7, false).unwrap();
    review.set_playing(false);
    assert_eq!(review.selected_tracks(), &[9]);
    assert_eq!(review.transport_label(), "Play");
    assert_eq!(review.rate_label(), "1x");
}

#[test]
fn view_switching_and_clock_format_are_deterministic() {
    let mut view = SpikeView::Library;
    view.show_review();
    assert_eq!(view, SpikeView::Review);
    view.show_library();
    assert_eq!(view, SpikeView::Library);

    assert_eq!(format_clock(0), "00:00.000");
    assert_eq!(format_clock(48_000 * 65 + 24_000), "01:05.500");
    assert_eq!(format_clock(48_000 * 3_665 + 47_999), "1:01:05.999");
}
