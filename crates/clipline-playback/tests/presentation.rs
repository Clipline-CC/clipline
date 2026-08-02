use clipline_playback::{
    fit_aspect_ratio, LogicalVideoRect, PhysicalVideoRect, PresentationLifecycle,
    PresentationState, PresentationUpdate, ScaleFactor,
};

#[test]
fn logical_rectangles_convert_without_fractional_dpi_gaps() {
    for (scale, expected_width, expected_height) in [
        (1.0, 1_200, 760),
        (1.25, 1_500, 950),
        (1.5, 1_800, 1_140),
        (2.0, 2_400, 1_520),
    ] {
        let physical = LogicalVideoRect::new(0.0, 0.0, 1_200.0, 760.0)
            .unwrap()
            .to_physical(ScaleFactor::new(scale).unwrap())
            .unwrap();
        assert_eq!(
            physical,
            PhysicalVideoRect::new(0, 0, expected_width, expected_height)
        );
    }

    let scale = ScaleFactor::new(1.25).unwrap();
    let left = LogicalVideoRect::new(0.0, 0.0, 300.0, 100.0)
        .unwrap()
        .to_physical(scale)
        .unwrap();
    let right = LogicalVideoRect::new(300.0, 0.0, 900.0, 100.0)
        .unwrap()
        .to_physical(scale)
        .unwrap();
    assert_eq!(left.right(), right.x);

    let fractional = LogicalVideoRect::new(0.2, 0.2, 100.4, 50.4)
        .unwrap()
        .to_physical(ScaleFactor::new(1.0).unwrap())
        .unwrap();
    assert_eq!(fractional, PhysicalVideoRect::new(0, 0, 101, 51));
}

#[test]
fn aspect_fit_is_centered_bounded_and_handles_zero_area() {
    assert_eq!(
        fit_aspect_ratio(PhysicalVideoRect::new(10, 20, 1_000, 700), 16, 9).unwrap(),
        PhysicalVideoRect::new(10, 89, 1_000, 562)
    );
    assert_eq!(
        fit_aspect_ratio(PhysicalVideoRect::new(0, 0, 700, 1_000), 16, 9).unwrap(),
        PhysicalVideoRect::new(0, 303, 700, 393)
    );
    assert_eq!(
        fit_aspect_ratio(PhysicalVideoRect::new(5, 7, 0, 0), 16, 9).unwrap(),
        PhysicalVideoRect::new(5, 7, 0, 0)
    );
}

#[test]
fn lifecycle_coalesces_geometry_and_releases_on_hidden_states() {
    let mut lifecycle = PresentationLifecycle::new();
    let first = lifecycle
        .update(
            PhysicalVideoRect::new(20, 30, 800, 450),
            PresentationState::Visible,
        )
        .unwrap();
    assert_eq!(
        first,
        PresentationUpdate::Changed {
            revision: 1,
            release_pending_frame: false,
        }
    );
    assert_eq!(lifecycle.latest_revision(), 1);

    assert_eq!(
        lifecycle
            .update(
                PhysicalVideoRect::new(20, 30, 800, 450),
                PresentationState::Visible,
            )
            .unwrap(),
        PresentationUpdate::Unchanged { revision: 1 }
    );

    assert_eq!(
        lifecycle
            .update(
                PhysicalVideoRect::new(20, 30, 0, 0),
                PresentationState::Minimized,
            )
            .unwrap(),
        PresentationUpdate::Changed {
            revision: 2,
            release_pending_frame: true,
        }
    );
    assert!(!lifecycle.is_presentable());

    assert_eq!(
        lifecycle
            .update(
                PhysicalVideoRect::new(40, 50, 1_000, 562),
                PresentationState::Visible,
            )
            .unwrap(),
        PresentationUpdate::Changed {
            revision: 3,
            release_pending_frame: false,
        }
    );
    assert_eq!(lifecycle.latest_geometry().unwrap().width, 1_000);
}

#[test]
fn stale_geometry_and_counter_overflow_fail_atomically() {
    let mut lifecycle = PresentationLifecycle::with_revision(u64::MAX);
    let before = lifecycle.clone();
    assert!(lifecycle
        .update(
            PhysicalVideoRect::new(0, 0, 640, 360),
            PresentationState::Visible,
        )
        .is_err());
    assert_eq!(lifecycle, before);

    let mut lifecycle = PresentationLifecycle::new();
    lifecycle
        .update(
            PhysicalVideoRect::new(0, 0, 640, 360),
            PresentationState::Visible,
        )
        .unwrap();
    assert!(!lifecycle.accepts_revision(0));
    assert!(lifecycle.accepts_revision(1));
    assert!(!lifecycle.accepts_revision(2));
}
