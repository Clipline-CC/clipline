//! Isolated DWM feasibility probe. Run with --help; never used by the app.

use std::path::PathBuf;

#[cfg(windows)]
#[path = "windows/dwm_probe.rs"]
mod platform;

const HELP: &str = "DWM window capture experiment (Windows x64; no WGC fallback)
  dwm_probe --list
  dwm_probe --window \"unique title\" [--seconds 10] [--fps 30] [--out NEW_DIRECTORY]
  dwm_probe --hwnd 123456 [--seconds 10] [--fps 30] [--out NEW_DIRECTORY]
Limits: 1-600 seconds, 1-120 sampling FPS. Keep the selected window animating.
Add --require-motion to fail if all readable samples are identical.
Saves first/middle/last BMP images, samples.csv, and summary.txt locally.
Sampling/pixel changes do not establish game FPS or synchronization correctness.";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.is_empty() || args == ["--help"] {
        println!("{HELP}");
        return Ok(());
    }
    #[cfg(windows)]
    {
        if args == ["--list"] {
            platform::list();
            return Ok(());
        }
        platform::run(Options::parse(args)?)
    }
    #[cfg(not(windows))]
    Err("dwm_probe requires an interactive Windows desktop".into())
}

pub struct Options {
    pub window: Option<String>,
    pub hwnd: Option<isize>,
    pub seconds: u64,
    pub fps: u32,
    pub output: Option<PathBuf>,
    pub require_motion: bool,
}

impl Options {
    pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, String> {
        let mut result = Self {
            window: None,
            hwnd: None,
            seconds: 10,
            fps: 30,
            output: None,
            require_motion: false,
        };
        let mut args = args.into_iter();
        let mut seen = std::collections::HashSet::new();
        while let Some(flag) = args.next() {
            if !seen.insert(flag.clone()) {
                return Err(format!("duplicate option: {flag}"));
            }
            if flag == "--require-motion" {
                result.require_motion = true;
                continue;
            }
            let value = args
                .next()
                .ok_or_else(|| format!("missing value for {flag}"))?;
            match flag.as_str() {
                "--window" if !value.trim().is_empty() => result.window = Some(value),
                "--hwnd" => {
                    let raw = if let Some(hex) = value.strip_prefix("0x") {
                        isize::from_str_radix(hex, 16)
                    } else {
                        value.parse()
                    };
                    result.hwnd = Some(raw.map_err(|_| "invalid HWND")?);
                    if result.hwnd.is_some_and(|raw| raw <= 0) {
                        return Err("HWND must be positive".into());
                    }
                }
                "--seconds" => result.seconds = value.parse().map_err(|_| "invalid seconds")?,
                "--fps" => result.fps = value.parse().map_err(|_| "invalid FPS")?,
                "--out" if !value.trim().is_empty() => result.output = Some(value.into()),
                _ => return Err(format!("unknown option or empty value: {flag}")),
            }
        }
        if result.window.is_some() == result.hwnd.is_some() {
            return Err("specify exactly one of --window and --hwnd".into());
        }
        if !(1..=600).contains(&result.seconds) || !(1..=120).contains(&result.fps) {
            return Err("seconds must be 1-600 and FPS must be 1-120".into());
        }
        Ok(result)
    }
}

/// An animated-target acceptance check, not a claim about synchronized game FPS.
pub fn capture_verdict(reads: u64, changes: u64, require_motion: bool) -> Result<(), &'static str> {
    if reads == 0 {
        Err("no readable DWM frames; see samples.csv")
    } else if require_motion && changes == 0 {
        Err("motion required but all captured samples were identical; inspect the saved BMPs")
    } else {
        Ok(())
    }
}

/// BMP stores top-down BGRA rows without GPU padding. No image dependency needed.
pub fn bmp_bytes(
    width: u32,
    height: u32,
    pitch: usize,
    pixels: &[u8],
) -> Result<Vec<u8>, &'static str> {
    if width == 0 || height == 0 || width > i32::MAX as u32 || height > i32::MAX as u32 {
        return Err("invalid image dimensions");
    }
    let row = (width as usize).checked_mul(4).ok_or("row overflow")?;
    let size = row.checked_mul(height as usize).ok_or("image overflow")?;
    let required = pitch
        .checked_mul(height as usize - 1)
        .and_then(|offset| offset.checked_add(row))
        .ok_or("pitch overflow")?;
    if pitch < row || pixels.len() < required || size > 256 * 1024 * 1024 {
        return Err("invalid or oversized image buffer");
    }
    let mut bmp = vec![0; 54 + size];
    bmp[..2].copy_from_slice(b"BM");
    bmp[2..6].copy_from_slice(&((54 + size) as u32).to_le_bytes());
    bmp[10..14].copy_from_slice(&54u32.to_le_bytes());
    bmp[14..18].copy_from_slice(&40u32.to_le_bytes());
    bmp[18..22].copy_from_slice(&(width as i32).to_le_bytes());
    bmp[22..26].copy_from_slice(&(-(height as i32)).to_le_bytes());
    bmp[26..28].copy_from_slice(&1u16.to_le_bytes());
    bmp[28..30].copy_from_slice(&32u16.to_le_bytes());
    bmp[34..38].copy_from_slice(&(size as u32).to_le_bytes());
    for (y, destination) in bmp[54..].chunks_exact_mut(row).enumerate() {
        destination.copy_from_slice(&pixels[y * pitch..y * pitch + row]);
    }
    Ok(bmp)
}

#[cfg(test)]
mod tests {
    #[test]
    fn motion_required_runs_reject_readable_but_static_surfaces() {
        assert!(super::capture_verdict(300, 0, true).is_err());
        assert!(super::capture_verdict(300, 1, true).is_ok());
        assert!(super::capture_verdict(300, 0, false).is_ok());
        assert!(super::capture_verdict(0, 0, false).is_err());
        let options =
            super::Options::parse(["--hwnd", "42", "--require-motion"].map(str::to_owned)).unwrap();
        assert!(options.require_motion);
        assert!(super::Options::parse(
            ["--hwnd", "42", "--require-motion", "--require-motion"].map(str::to_owned),
        )
        .is_err());
    }

    #[test]
    fn rejects_unbounded_or_ambiguous_capture_requests() {
        for args in [
            vec!["--window", ""],
            vec!["--hwnd", "0"],
            vec!["--window", "game", "--fps", "0"],
            vec!["--window", "game", "--seconds", "601"],
            vec!["--window", "game", "--hwnd", "42"],
            vec!["--window", "game", "--typo"],
        ] {
            assert!(super::Options::parse(args.into_iter().map(str::to_owned)).is_err());
        }
        let options = super::Options::parse(
            ["--hwnd", "42", "--fps", "60", "--seconds", "2"].map(str::to_owned),
        )
        .unwrap();
        assert_eq!(
            (options.hwnd, options.fps, options.seconds),
            (Some(42), 60, 2)
        );
    }

    #[test]
    fn bmp_preserves_top_down_colors_and_ignores_gpu_row_padding() {
        let pixels = [0, 0, 255, 255, 99, 99, 99, 99, 0, 255, 0, 255];
        let bmp = super::bmp_bytes(1, 2, 8, &pixels).unwrap();
        assert_eq!(&bmp[..2], b"BM");
        assert_eq!(i32::from_le_bytes(bmp[22..26].try_into().unwrap()), -2);
        assert_eq!(&bmp[54..], &[0, 0, 255, 255, 0, 255, 0, 255]);
        assert!(super::bmp_bytes(2, 2, 4, &pixels).is_err());
        assert!(super::bmp_bytes(1, 2, 8, &pixels[..11]).is_err());
        assert!(super::bmp_bytes(0, 2, 8, &pixels).is_err());
    }
}
