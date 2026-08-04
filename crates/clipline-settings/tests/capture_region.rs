use clipline_settings::capture_region::{
    resolve_display, Align, DisplayGeometry, DpiScale, LogicalPoint, LogicalSize, PhysicalPoint,
    PhysicalSize, RegionAction, RegionGeometry, RegionGeometryError,
};
use clipline_settings::CaptureRegionSettings;

fn display<'a>(
    id: &'a str,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    is_primary: bool,
) -> DisplayGeometry<'a> {
    DisplayGeometry::new(id, x, y, width, height, is_primary).expect("valid display")
}

#[test]
fn negative_monitor_coordinates_are_preserved_and_clamped() {
    let monitor = display("DISPLAY2", -2560, -240, 2560, 1440, false);
    let region = RegionGeometry::new(-3000, -900, 801, 451).expect("valid region");

    assert_eq!(
        region.clamp_to(monitor).expect("clamped region"),
        RegionGeometry::new(-2560, -240, 800, 450).unwrap()
    );
}

#[test]
fn full_display_selection_keeps_the_exact_physical_display() {
    let monitor = display("DISPLAY2", 1920, -120, 2560, 1440, false);

    assert_eq!(
        RegionGeometry::new(0, 0, 2, 2)
            .unwrap()
            .apply(monitor, RegionAction::FullDisplay)
            .unwrap(),
        RegionGeometry::new(1920, -120, 2560, 1440).unwrap()
    );
}

#[test]
fn a_removed_display_falls_back_to_primary_and_then_first() {
    let secondary = display("secondary", -1280, 0, 1280, 720, false);
    let primary = display("primary", 0, 0, 1920, 1080, true);
    let settings = CaptureRegionSettings {
        display_id: Some("removed".into()),
        x: -5000,
        y: 5000,
        width: 800,
        height: 450,
    };

    let displays = [secondary, primary];
    let resolved = resolve_display(&settings, &displays).expect("primary fallback");
    assert_eq!(resolved.display.id(), "primary");
    assert_eq!(
        resolved.region,
        RegionGeometry::new(0, 630, 800, 450).unwrap()
    );

    let no_primary = [secondary];
    assert_eq!(
        resolve_display(&settings, &no_primary)
            .expect("first fallback")
            .display
            .id(),
        "secondary"
    );
    assert_eq!(
        resolve_display(&settings, &[]).unwrap_err(),
        RegionGeometryError::NoDisplays
    );
}

#[test]
fn removed_display_coordinates_are_clamped_before_far_edge_conversion() {
    let primary = display("primary", 0, 0, 1920, 1080, true);
    let settings = CaptureRegionSettings {
        display_id: Some("removed".into()),
        x: i32::MAX,
        y: i32::MIN,
        width: 16_384,
        height: 16_384,
    };
    let displays = [primary];

    assert_eq!(
        resolve_display(&settings, &displays).unwrap().region,
        RegionGeometry::new(0, 0, 1920, 1080).unwrap()
    );
}

#[test]
fn matching_display_is_preferred_over_the_primary() {
    let selected = display("selected", -1920, 0, 1920, 1080, false);
    let primary = display("primary", 0, 0, 1920, 1080, true);
    let settings = CaptureRegionSettings {
        display_id: Some("selected".into()),
        x: -1800,
        y: 100,
        width: 800,
        height: 450,
    };

    let displays = [primary, selected];
    let resolved = resolve_display(&settings, &displays).unwrap();
    assert_eq!(resolved.display.id(), "selected");
    assert_eq!(resolved.region.x, -1800);
}

#[test]
fn move_resize_and_menu_alignment_match_the_retained_js_contract() {
    let monitor = display("DISPLAY2", 1920, -120, 2560, 1440, false);
    let initial = RegionGeometry::new(2000, 0, 800, 450).unwrap();

    assert_eq!(
        initial
            .apply(
                monitor,
                RegionAction::MoveBy {
                    dx: 5000,
                    dy: -5000
                }
            )
            .unwrap(),
        RegionGeometry::new(3680, -120, 800, 450).unwrap()
    );
    assert_eq!(
        RegionGeometry::new(1000, -900, 800, 450)
            .unwrap()
            .apply(monitor, RegionAction::MoveBy { dx: 100, dy: 100 })
            .unwrap(),
        RegionGeometry::new(2020, -20, 800, 450).unwrap()
    );
    assert_eq!(
        initial
            .apply(
                monitor,
                RegionAction::ResizeBy {
                    width_delta: 1,
                    height_delta: 1,
                },
            )
            .unwrap(),
        initial
    );
    assert_eq!(
        initial
            .apply(
                monitor,
                RegionAction::ResizeBy {
                    width_delta: -50_000,
                    height_delta: -50_000,
                },
            )
            .unwrap(),
        RegionGeometry::new(2000, 0, 2, 2).unwrap()
    );

    let cases = [
        (Align::Left, RegionGeometry::new(1920, 0, 800, 450).unwrap()),
        (
            Align::Right,
            RegionGeometry::new(3680, 0, 800, 450).unwrap(),
        ),
        (
            Align::Top,
            RegionGeometry::new(2000, -120, 800, 450).unwrap(),
        ),
        (
            Align::Bottom,
            RegionGeometry::new(2000, 870, 800, 450).unwrap(),
        ),
        (
            Align::Center,
            RegionGeometry::new(2800, 375, 800, 450).unwrap(),
        ),
    ];
    for (align, expected) in cases {
        assert_eq!(
            initial.apply(monitor, RegionAction::Align(align)).unwrap(),
            expected
        );
    }
}

#[test]
fn oversized_regions_shrink_before_position_is_clamped() {
    let monitor = display("DISPLAY2", 1920, -120, 2560, 1440, false);
    let region = RegionGeometry::new(1000, -900, 4000, 2000).unwrap();

    assert_eq!(
        region.clamp_to(monitor).unwrap(),
        RegionGeometry::new(1920, -120, 2560, 1440).unwrap()
    );
}

#[test]
fn geometry_rejects_empty_ids_invalid_sides_and_coordinate_overflow() {
    assert_eq!(
        DisplayGeometry::new("", 0, 0, 1920, 1080, true).unwrap_err(),
        RegionGeometryError::EmptyDisplayId
    );
    assert_eq!(
        DisplayGeometry::new("display", 0, 0, 1, 1080, true).unwrap_err(),
        RegionGeometryError::SideTooSmall
    );
    assert_eq!(
        DisplayGeometry::new("display", i32::MAX, 0, 2, 1080, true).unwrap_err(),
        RegionGeometryError::CoordinateOverflow
    );
    assert_eq!(
        RegionGeometry::new(0, 0, 1, 1080).unwrap_err(),
        RegionGeometryError::SideTooSmall
    );
}

#[test]
fn dpi_scale_requires_a_finite_positive_value() {
    for invalid in [0.0, -1.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        assert_eq!(
            DpiScale::new(invalid).unwrap_err(),
            RegionGeometryError::InvalidScale
        );
    }
    assert_eq!(DpiScale::new(1.25).unwrap().get(), 1.25);
}

#[test]
fn dpi_conversion_round_trips_negative_points_and_positive_sizes() {
    let dpi = DpiScale::new(1.25).unwrap();
    let physical_point = dpi
        .logical_to_physical_point(LogicalPoint::new(-1536.0, -96.0).unwrap())
        .unwrap();
    let physical_size = dpi
        .logical_to_physical_size(LogicalSize::new(640.0, 360.0).unwrap())
        .unwrap();

    assert_eq!(physical_point, PhysicalPoint { x: -1920, y: -120 });
    assert_eq!(
        physical_size,
        PhysicalSize {
            width: 800,
            height: 450
        }
    );
    assert_eq!(
        dpi.physical_to_logical_point(physical_point).unwrap(),
        LogicalPoint::new(-1536.0, -96.0).unwrap()
    );
    assert_eq!(
        dpi.physical_to_logical_size(physical_size).unwrap(),
        LogicalSize::new(640.0, 360.0).unwrap()
    );
}

#[test]
fn dpi_conversion_is_checked_and_never_saturates_silently() {
    let dpi = DpiScale::new(2.0).unwrap();
    assert_eq!(
        LogicalPoint::new(f64::NAN, 0.0).unwrap_err(),
        RegionGeometryError::NonFiniteCoordinate
    );
    assert_eq!(
        LogicalSize::new(-1.0, 100.0).unwrap_err(),
        RegionGeometryError::SideTooSmall
    );
    assert_eq!(
        dpi.logical_to_physical_point(LogicalPoint {
            x: f64::from(i32::MAX),
            y: 0.0
        })
        .unwrap_err(),
        RegionGeometryError::CoordinateOverflow
    );
    assert_eq!(
        dpi.logical_to_physical_size(LogicalSize {
            width: f64::from(u32::MAX),
            height: 2.0
        })
        .unwrap_err(),
        RegionGeometryError::SideTooLarge
    );
}

#[test]
fn applying_to_settings_allocates_only_for_a_changed_display_id() {
    let display = display("DISPLAY2", -1920, 0, 1920, 1080, false);
    let resolved = RegionGeometry::new(-1800, 100, 800, 450)
        .unwrap()
        .resolve(display);
    let mut settings = CaptureRegionSettings::default();

    resolved.apply_to_settings(&mut settings).unwrap();
    assert_eq!(settings.display_id.as_deref(), Some("DISPLAY2"));
    assert_eq!((settings.x, settings.y), (-1800, 100));
    assert_eq!((settings.width, settings.height), (800, 450));

    let capacity = settings.display_id.as_ref().unwrap().capacity();
    resolved.apply_to_settings(&mut settings).unwrap();
    assert_eq!(settings.display_id.as_ref().unwrap().capacity(), capacity);
}
