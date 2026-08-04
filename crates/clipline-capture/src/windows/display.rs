//! Win32 monitor enumeration for display-region capture.

use windows::core::BOOL;
use windows::Win32::Foundation::{LPARAM, RECT};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW,
};

use crate::traits::CaptureError;

const MONITORINFOF_PRIMARY: u32 = 0x00000001;
pub const MAX_PROBE_DISPLAYS: usize = 64;
pub const MAX_DISPLAY_TEXT_BYTES: usize = 16 * 1024;
pub const MAX_DISPLAY_CATALOG_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct DisplayInfo {
    /// Win32 device id, e.g. `\\.\DISPLAY1`.
    pub id: String,
    /// Human-friendly fallback label for the UI.
    pub name: String,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub is_primary: bool,
}

#[derive(Debug, Clone)]
pub struct DisplayHandle {
    pub handle: HMONITOR,
    pub info: DisplayInfo,
}

pub fn enumerate_displays() -> Result<Vec<DisplayInfo>, CaptureError> {
    enumerate_displays_with_checkpoint(|| Ok(()))
}

pub fn enumerate_displays_with_checkpoint(
    checkpoint: impl FnOnce() -> Result<(), String>,
) -> Result<Vec<DisplayInfo>, CaptureError> {
    // GDI monitor enumeration has no separate activation object. Treat the
    // point immediately before the OS call as the post-activation checkpoint.
    checkpoint().map_err(CaptureError::Init)?;
    Ok(enumerate_display_handles()?
        .into_iter()
        .map(|display| display.info)
        .collect())
}

pub fn validate_display_catalog(displays: &[DisplayInfo]) -> Result<(), CaptureError> {
    if displays.len() > MAX_PROBE_DISPLAYS {
        return Err(CaptureError::Init(format!(
            "display count {} exceeds {MAX_PROBE_DISPLAYS}",
            displays.len()
        )));
    }
    let mut aggregate = 0usize;
    for display in displays {
        for (label, value) in [("display id", &display.id), ("display name", &display.name)] {
            if value.len() > MAX_DISPLAY_TEXT_BYTES {
                return Err(CaptureError::Init(format!(
                    "{label} is {} bytes; maximum is {MAX_DISPLAY_TEXT_BYTES}",
                    value.len()
                )));
            }
            aggregate = aggregate.checked_add(value.len()).ok_or_else(|| {
                CaptureError::Init("display catalog byte count overflowed".into())
            })?;
        }
    }
    if aggregate > MAX_DISPLAY_CATALOG_BYTES {
        return Err(CaptureError::Init(format!(
            "display catalog is {aggregate} bytes; maximum is {MAX_DISPLAY_CATALOG_BYTES}"
        )));
    }
    Ok(())
}

pub fn display_handle_by_id(id: Option<&str>) -> Result<DisplayHandle, CaptureError> {
    let displays = enumerate_display_handles()?;
    select_display_handle(&displays, id)
}

pub fn display_handle_by_id_or_primary(
    id: Option<&str>,
) -> Result<(DisplayHandle, bool), CaptureError> {
    let displays = enumerate_display_handles()?;
    select_display_handle_or_primary(&displays, id)
}

fn select_display_handle(
    displays: &[DisplayHandle],
    id: Option<&str>,
) -> Result<DisplayHandle, CaptureError> {
    if let Some(id) = id.filter(|id| !id.trim().is_empty()) {
        if let Some(display) = displays.iter().find(|display| display.info.id == id) {
            return Ok(display.clone());
        }
        return Err(CaptureError::Init(format!("display {id:?} was not found")));
    }
    displays
        .iter()
        .find(|display| display.info.is_primary)
        .or_else(|| displays.first())
        .cloned()
        .ok_or_else(|| CaptureError::Init("no displays found".into()))
}

fn select_display_handle_or_primary(
    displays: &[DisplayHandle],
    id: Option<&str>,
) -> Result<(DisplayHandle, bool), CaptureError> {
    match select_display_handle(displays, id) {
        Ok(display) => Ok((display, false)),
        Err(e) if id.is_some_and(|id| !id.trim().is_empty()) => {
            let fallback = select_display_handle(displays, None).map_err(|fallback| {
                CaptureError::Init(format!(
                    "{e}; primary display fallback was not available: {fallback}"
                ))
            })?;
            Ok((fallback, true))
        }
        Err(e) => Err(e),
    }
}

fn enumerate_display_handles() -> Result<Vec<DisplayHandle>, CaptureError> {
    let mut displays = Vec::<DisplayHandle>::new();
    displays
        .try_reserve_exact(MAX_PROBE_DISPLAYS)
        .map_err(|_| CaptureError::Init("reserve bounded display catalog".into()))?;
    let mut enumeration = DisplayEnumeration {
        displays,
        error: None,
        utf8_bytes: 0,
    };
    // SAFETY: the callback only runs during this call; lparam points at
    // `displays`, which outlives the enumeration.
    let ok = unsafe {
        EnumDisplayMonitors(
            None,
            None,
            Some(enum_monitor_proc),
            LPARAM(&mut enumeration as *mut DisplayEnumeration as isize),
        )
    };
    if let Some(error) = enumeration.error {
        return Err(CaptureError::Init(error));
    }
    if !ok.as_bool() {
        return Err(CaptureError::Init("EnumDisplayMonitors failed".into()));
    }
    Ok(enumeration.displays)
}

struct DisplayEnumeration {
    displays: Vec<DisplayHandle>,
    error: Option<String>,
    utf8_bytes: usize,
}

unsafe extern "system" fn enum_monitor_proc(
    monitor: HMONITOR,
    _hdc: HDC,
    _rect: *mut RECT,
    lparam: LPARAM,
) -> BOOL {
    // SAFETY: lparam is the accumulator passed by enumerate_display_handles
    // on this same thread, alive for the whole enumeration.
    let enumeration = unsafe { &mut *(lparam.0 as *mut DisplayEnumeration) };
    if enumeration.displays.len() == MAX_PROBE_DISPLAYS {
        enumeration.error = Some(format!("display count exceeds {MAX_PROBE_DISPLAYS}"));
        return BOOL(0);
    }
    let mut info = MONITORINFOEXW::default();
    info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
    // SAFETY: monitor comes from EnumDisplayMonitors; info points to a
    // properly-sized MONITORINFOEXW whose first field is MONITORINFO.
    let ok = unsafe { GetMonitorInfoW(monitor, &mut info as *mut _ as *mut MONITORINFO) };
    if !ok.as_bool() {
        return BOOL(1);
    }
    let id = utf16_z(&info.szDevice);
    let rect = info.monitorInfo.rcMonitor;
    let width = (rect.right - rect.left).max(0) as u32;
    let height = (rect.bottom - rect.top).max(0) as u32;
    if width == 0 || height == 0 {
        return BOOL(1);
    }
    let name = id
        .strip_prefix(r"\\.\")
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| id.clone());
    if id.len() > MAX_DISPLAY_TEXT_BYTES || name.len() > MAX_DISPLAY_TEXT_BYTES {
        enumeration.error = Some(format!(
            "display label exceeds {MAX_DISPLAY_TEXT_BYTES} UTF-8 bytes"
        ));
        return BOOL(0);
    }
    let Some(utf8_bytes) = enumeration
        .utf8_bytes
        .checked_add(id.len())
        .and_then(|bytes| bytes.checked_add(name.len()))
    else {
        enumeration.error = Some("display catalog byte count overflowed".into());
        return BOOL(0);
    };
    if utf8_bytes > MAX_DISPLAY_CATALOG_BYTES {
        enumeration.error = Some(format!(
            "display catalog exceeds {MAX_DISPLAY_CATALOG_BYTES} UTF-8 bytes"
        ));
        return BOOL(0);
    }
    enumeration.utf8_bytes = utf8_bytes;
    enumeration.displays.push(DisplayHandle {
        handle: monitor,
        info: DisplayInfo {
            id,
            name,
            x: rect.left,
            y: rect.top,
            width,
            height,
            is_primary: info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY == MONITORINFOF_PRIMARY,
        },
    });
    BOOL(1)
}

fn utf16_z(buf: &[u16]) -> String {
    let len = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16_z_stops_at_nul() {
        assert_eq!(utf16_z(&[b'D' as u16, b'1' as u16, 0, b'X' as u16]), "D1");
    }

    #[test]
    fn display_enumeration_is_best_effort() {
        if std::env::var_os("CI").is_some() {
            eprintln!("SKIP: display enumeration needs an interactive desktop");
            return;
        }
        let displays = match enumerate_displays() {
            Ok(displays) => displays,
            Err(e) => {
                eprintln!("SKIP: no displays: {e}");
                return;
            }
        };
        assert!(!displays.is_empty());
        assert!(displays.iter().all(|d| d.width > 0 && d.height > 0));
    }

    #[test]
    fn display_catalog_rejects_count_and_huge_os_labels() {
        let display = DisplayInfo {
            id: r"\\.\DISPLAY1".into(),
            name: "DISPLAY1".into(),
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
            is_primary: true,
        };
        assert!(validate_display_catalog(&vec![display.clone(); MAX_PROBE_DISPLAYS]).is_ok());
        assert!(validate_display_catalog(&vec![display.clone(); MAX_PROBE_DISPLAYS + 1]).is_err());
        let mut huge = display;
        huge.name = "é".repeat(MAX_DISPLAY_TEXT_BYTES);
        assert!(validate_display_catalog(&[huge]).is_err());
    }

    #[test]
    fn select_display_uses_primary_flag_when_no_id_is_saved() {
        let displays = vec![
            DisplayHandle {
                handle: HMONITOR(std::ptr::dangling_mut()),
                info: DisplayInfo {
                    id: r"\\.\DISPLAY-GHOST".into(),
                    name: "DISPLAY-GHOST".into(),
                    x: 0,
                    y: 0,
                    width: 1920,
                    height: 1080,
                    is_primary: false,
                },
            },
            DisplayHandle {
                handle: HMONITOR(std::ptr::dangling_mut()),
                info: DisplayInfo {
                    id: r"\\.\DISPLAY2".into(),
                    name: "DISPLAY2".into(),
                    x: 1920,
                    y: 0,
                    width: 2560,
                    height: 1440,
                    is_primary: true,
                },
            },
        ];

        let selected = select_display_handle(&displays, None).unwrap();

        assert_eq!(selected.info.id, r"\\.\DISPLAY2");
    }

    #[test]
    fn select_display_or_primary_recovers_from_missing_saved_id() {
        let displays = vec![
            DisplayHandle {
                handle: HMONITOR(std::ptr::dangling_mut()),
                info: DisplayInfo {
                    id: r"\\.\DISPLAY1".into(),
                    name: "DISPLAY1".into(),
                    x: 0,
                    y: 0,
                    width: 1920,
                    height: 1080,
                    is_primary: true,
                },
            },
            DisplayHandle {
                handle: HMONITOR(std::ptr::dangling_mut()),
                info: DisplayInfo {
                    id: r"\\.\DISPLAY2".into(),
                    name: "DISPLAY2".into(),
                    x: 1920,
                    y: 0,
                    width: 2560,
                    height: 1440,
                    is_primary: false,
                },
            },
        ];

        let (selected, recovered) =
            select_display_handle_or_primary(&displays, Some(r"\\.\DISPLAY-GHOST")).unwrap();

        assert!(recovered);
        assert_eq!(selected.info.id, r"\\.\DISPLAY1");
    }
}
