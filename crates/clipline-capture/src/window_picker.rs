//! Platform-neutral rules for the custom-game window picker: which windows
//! to offer, and how a captured frame becomes a small preview.

/// What the window manager reports about one top-level window.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WindowTraits {
    pub visible: bool,
    /// Hidden by DWM (another virtual desktop, a suspended UWP host, …).
    pub cloaked: bool,
    pub has_title: bool,
    pub has_area: bool,
    /// `WS_EX_TOOLWINDOW`: palettes and floating toolbars.
    pub tool_window: bool,
    /// `WS_EX_APPWINDOW`: asks for a taskbar button despite the above.
    pub app_window: bool,
    /// Has an owner window (dialogs, popups).
    pub owned: bool,
    /// The owner is visible. Games sometimes own their gameplay window from a
    /// hidden bootstrap window; those stay pickable.
    pub owner_visible: bool,
}

/// Alt-Tab's rule for which windows are apps worth offering.
pub fn is_pickable_window(traits: &WindowTraits) -> bool {
    traits.visible
        && !traits.cloaked
        && traits.has_title
        && traits.has_area
        && (traits.app_window || !(traits.tool_window || (traits.owned && traits.owner_visible)))
}

/// One WGC session, seen only through the steps that decide whether it may
/// start without the yellow border.
pub trait BorderlessCapture {
    /// The OS can hide the border at all (Windows 11 and the session API).
    fn borderless_supported(&mut self) -> bool;
    /// Borderless capture access came back `Allowed`.
    fn request_borderless_access(&mut self) -> bool;
    /// `IsBorderRequired = false` was accepted.
    fn hide_border(&mut self) -> bool;
    fn start(&mut self) -> Result<(), String>;
}

/// Start `session` only if every border check passes, in order. `Ok(false)`
/// means it was never started, so no border could have been shown.
pub fn start_without_border(session: &mut impl BorderlessCapture) -> Result<bool, String> {
    if !(session.borderless_supported()
        && session.request_borderless_access()
        && session.hide_border())
    {
        return Ok(false);
    }
    session.start()?;
    Ok(true)
}

/// The part of a capture surface that holds the window: its positive content
/// size, or `None` when the content is empty or outgrew the surface.
pub fn preview_region(
    content_width: i32,
    content_height: i32,
    surface_width: u32,
    surface_height: u32,
) -> Option<(u32, u32)> {
    let width = u32::try_from(content_width).ok().filter(|&w| w > 0 && w <= surface_width)?;
    let height = u32::try_from(content_height).ok().filter(|&h| h > 0 && h <= surface_height)?;
    Some((width, height))
}

/// A preview image, tightly packed RGBA.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Thumbnail {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// Largest size that fits `max_width`×`max_height`, keeps the aspect ratio,
/// never upscales and is at least 1×1.
pub fn fit_within(width: u32, height: u32, max_width: u32, max_height: u32) -> (u32, u32) {
    if width <= max_width && height <= max_height {
        return (width.max(1), height.max(1));
    }
    let scaled = |value: u32, numerator: u32, denominator: u32| {
        let rounded = (u64::from(value) * u64::from(numerator) + u64::from(denominator) / 2)
            / u64::from(denominator);
        (rounded as u32).max(1)
    };
    // Compare aspect ratios without division: the wider side hits its bound.
    if u64::from(width) * u64::from(max_height) >= u64::from(height) * u64::from(max_width) {
        (max_width.max(1), scaled(height, max_width, width))
    } else {
        (scaled(width, max_height, height), max_height.max(1))
    }
}

/// Area-average downscale of a BGRA frame with `row_pitch` bytes per row into
/// an opaque RGBA thumbnail that fits the bounding box. `None` when the input
/// is empty or too short for its dimensions.
pub fn downscale_bgra_to_rgba(
    bgra: &[u8],
    width: u32,
    height: u32,
    row_pitch: usize,
    max_width: u32,
    max_height: u32,
) -> Option<Thumbnail> {
    let row_bytes = width as usize * 4;
    if width == 0 || height == 0 || row_pitch < row_bytes {
        return None;
    }
    let needed = row_pitch.checked_mul(height as usize - 1)?.checked_add(row_bytes)?;
    if bgra.len() < needed {
        return None;
    }
    let (out_width, out_height) = fit_within(width, height, max_width, max_height);
    let mut rgba = Vec::with_capacity(out_width as usize * out_height as usize * 4);
    // Source span covered by output index `i` of `out` along an axis of `len`.
    let span = |i: u32, out: u32, len: u32| {
        let start = (u64::from(i) * u64::from(len) / u64::from(out)) as usize;
        let end = (u64::from(i + 1) * u64::from(len) / u64::from(out)) as usize;
        start..end.max(start + 1)
    };
    for out_y in 0..out_height {
        let rows = span(out_y, out_height, height);
        for out_x in 0..out_width {
            let columns = span(out_x, out_width, width);
            let mut sums = [0_u32; 3];
            let count = (rows.len() * columns.len()) as u32;
            for y in rows.clone() {
                let row = &bgra[y * row_pitch..];
                for x in columns.clone() {
                    let pixel = &row[x * 4..x * 4 + 3];
                    for (sum, &channel) in sums.iter_mut().zip(pixel) {
                        *sum += u32::from(channel);
                    }
                }
            }
            let [blue, green, red] = sums.map(|sum| ((sum + count / 2) / count) as u8);
            rgba.extend_from_slice(&[red, green, blue, u8::MAX]);
        }
    }
    Some(Thumbnail {
        width: out_width,
        height: out_height,
        rgba,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app() -> WindowTraits {
        WindowTraits {
            visible: true,
            has_title: true,
            has_area: true,
            ..WindowTraits::default()
        }
    }

    #[test]
    fn offers_ordinary_app_windows() {
        assert!(is_pickable_window(&app()));
    }

    #[test]
    fn hides_windows_alt_tab_hides() {
        for (label, traits) in [
            ("invisible", WindowTraits { visible: false, ..app() }),
            ("cloaked", WindowTraits { cloaked: true, ..app() }),
            ("untitled", WindowTraits { has_title: false, ..app() }),
            ("zero size", WindowTraits { has_area: false, ..app() }),
            ("tool window", WindowTraits { tool_window: true, ..app() }),
            ("owned popup", WindowTraits { owned: true, owner_visible: true, ..app() }),
        ] {
            assert!(!is_pickable_window(&traits), "{label} should be hidden");
        }
    }

    #[test]
    fn offers_windows_owned_by_a_hidden_window() {
        let gameplay = WindowTraits { owned: true, owner_visible: false, ..app() };
        assert!(is_pickable_window(&gameplay));
        let hidden_tool = WindowTraits { tool_window: true, ..gameplay };
        assert!(!is_pickable_window(&hidden_tool), "tool windows still need WS_EX_APPWINDOW");
    }

    #[test]
    fn app_window_style_overrides_tool_and_owner() {
        for traits in [
            WindowTraits { tool_window: true, app_window: true, ..app() },
            WindowTraits { owned: true, owner_visible: true, app_window: true, ..app() },
        ] {
            assert!(is_pickable_window(&traits), "{traits:?}");
        }
        let cloaked = WindowTraits { cloaked: true, app_window: true, ..app() };
        assert!(!is_pickable_window(&cloaked), "WS_EX_APPWINDOW never revives a cloaked window");
    }

    #[test]
    fn fit_within_keeps_aspect_and_never_upscales() {
        assert_eq!(fit_within(1920, 1080, 320, 180), (320, 180));
        assert_eq!(fit_within(1080, 1920, 320, 180), (101, 180));
        assert_eq!(fit_within(3440, 1440, 320, 180), (320, 134));
        assert_eq!(fit_within(200, 100, 320, 180), (200, 100));
        assert_eq!(fit_within(10_000, 1, 320, 180), (320, 1));
    }

    /// BGRA rows with `pad` junk bytes after each row.
    fn frame(width: u32, height: u32, pad: usize, pixel: impl Fn(u32, u32) -> [u8; 4]) -> (Vec<u8>, usize) {
        let pitch = width as usize * 4 + pad;
        let mut bytes = vec![0xEE; pitch * height as usize];
        for y in 0..height {
            for x in 0..width {
                let at = y as usize * pitch + x as usize * 4;
                bytes[at..at + 4].copy_from_slice(&pixel(x, y));
            }
        }
        (bytes, pitch)
    }

    #[test]
    fn downscale_swizzles_averages_and_honours_row_pitch() {
        // 4×2 frame: left half blue, right half red (BGRA), padded rows.
        let (bgra, pitch) = frame(4, 2, 12, |x, _| if x < 2 { [255, 0, 0, 0] } else { [0, 0, 255, 0] });
        let thumb = downscale_bgra_to_rgba(&bgra, 4, 2, pitch, 2, 1).unwrap();
        assert_eq!((thumb.width, thumb.height), (2, 1));
        assert_eq!(thumb.rgba, vec![0, 0, 255, 255, 255, 0, 0, 255], "blue then red, opaque");

        let (stripes, pitch) = frame(2, 2, 0, |_, y| if y == 0 { [0, 0, 0, 255] } else { [200, 100, 50, 255] });
        let averaged = downscale_bgra_to_rgba(&stripes, 2, 2, pitch, 1, 1).unwrap();
        assert_eq!(averaged.rgba, vec![25, 50, 100, 255]);
    }

    #[test]
    fn downscale_keeps_small_frames_at_their_size() {
        let (bgra, pitch) = frame(3, 2, 4, |x, y| [x as u8, y as u8, 7, 9]);
        let thumb = downscale_bgra_to_rgba(&bgra, 3, 2, pitch, 320, 180).unwrap();
        assert_eq!((thumb.width, thumb.height), (3, 2));
        assert_eq!(&thumb.rgba[..8], &[7, 0, 0, 255, 7, 0, 1, 255]);
    }

    #[test]
    fn preview_region_crops_to_content_and_rejects_growth() {
        assert_eq!(preview_region(800, 600, 1920, 1080), Some((800, 600)), "shrunk window");
        assert_eq!(preview_region(1920, 1080, 1920, 1080), Some((1920, 1080)));
        assert_eq!(preview_region(2560, 1080, 1920, 1080), None, "grew past the surface");
        assert_eq!(preview_region(1920, 1440, 1920, 1080), None, "grew past the surface");
        for (w, h) in [(0, 600), (800, 0), (-1, 600), (800, -5)] {
            assert_eq!(preview_region(w, h, 1920, 1080), None, "{w}x{h}");
        }
    }

    #[test]
    fn downscale_ignores_pixels_outside_the_content_region() {
        // A 4×4 surface whose window shrank to 2×2: everything else is stale.
        let (bgra, pitch) = frame(4, 4, 0, |x, y| if x < 2 && y < 2 { [10, 20, 30, 0] } else { [255, 255, 255, 255] });
        let (width, height) = preview_region(2, 2, 4, 4).unwrap();
        let thumb = downscale_bgra_to_rgba(&bgra, width, height, pitch, 1, 1).unwrap();
        assert_eq!(thumb.rgba, vec![30, 20, 10, 255]);
    }

    #[derive(Default)]
    struct FakeSession {
        supported: bool,
        allowed: bool,
        hides_border: bool,
        calls: Vec<&'static str>,
    }

    impl BorderlessCapture for FakeSession {
        fn borderless_supported(&mut self) -> bool {
            self.calls.push("supported");
            self.supported
        }
        fn request_borderless_access(&mut self) -> bool {
            self.calls.push("request");
            self.allowed
        }
        fn hide_border(&mut self) -> bool {
            self.calls.push("hide");
            self.hides_border
        }
        fn start(&mut self) -> Result<(), String> {
            self.calls.push("start");
            Ok(())
        }
    }

    #[test]
    fn capture_starts_only_after_every_border_check_passes() {
        let mut ok = FakeSession { supported: true, allowed: true, hides_border: true, ..Default::default() };
        assert_eq!(start_without_border(&mut ok), Ok(true));
        assert_eq!(ok.calls, ["supported", "request", "hide", "start"]);

        for (supported, allowed, hides_border) in [
            (false, true, true),
            (true, false, true),
            (true, true, false),
            (false, false, false),
        ] {
            let mut denied = FakeSession { supported, allowed, hides_border, ..Default::default() };
            assert_eq!(start_without_border(&mut denied), Ok(false));
            assert!(
                !denied.calls.contains(&"start"),
                "started with supported={supported} allowed={allowed} hides_border={hides_border}"
            );
        }
    }

    #[test]
    fn downscale_rejects_empty_or_short_input() {
        assert_eq!(downscale_bgra_to_rgba(&[], 0, 0, 0, 320, 180), None);
        assert_eq!(downscale_bgra_to_rgba(&[0; 15], 2, 2, 8, 320, 180), None);
        assert_eq!(downscale_bgra_to_rgba(&[0; 16], 2, 2, 4, 320, 180), None, "pitch below a row");
    }
}
