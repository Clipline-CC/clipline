//! Win32 monitor enumeration for display-region capture.

use windows::core::BOOL;
use windows::Win32::Foundation::{LPARAM, RECT};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW,
};

use crate::traits::CaptureError;

const MONITORINFOF_PRIMARY: u32 = 0x00000001;

#[derive(Debug, Clone, PartialEq, Eq)]
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
    Ok(enumerate_display_handles()?
        .into_iter()
        .map(|display| display.info)
        .collect())
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
    enumerate_monitor_snapshot()?.finish(false)
}

/// Hybrid source guards require the complete topology, never a best-effort
/// subset that could turn two monitors into an apparent single-monitor setup.
pub(super) fn enumerate_complete_display_handles() -> Result<Vec<DisplayHandle>, CaptureError> {
    enumerate_monitor_snapshot()?.finish(true)
}

#[derive(Default)]
struct MonitorSnapshot {
    displays: Vec<DisplayHandle>,
    incomplete: bool,
}

impl MonitorSnapshot {
    fn finish(self, require_complete: bool) -> Result<Vec<DisplayHandle>, CaptureError> {
        if require_complete && self.incomplete {
            return Err(CaptureError::Init("incomplete display enumeration".into()));
        }
        Ok(self.displays)
    }
}

fn enumerate_monitor_snapshot() -> Result<MonitorSnapshot, CaptureError> {
    let mut snapshot = MonitorSnapshot::default();
    // SAFETY: the callback only runs during this call; lparam points at
    // `snapshot`, which outlives the enumeration.
    let ok = unsafe {
        EnumDisplayMonitors(
            None,
            None,
            Some(enum_monitor_proc),
            LPARAM(&mut snapshot as *mut MonitorSnapshot as isize),
        )
    };
    if !ok.as_bool() {
        return Err(CaptureError::Init("EnumDisplayMonitors failed".into()));
    }
    Ok(snapshot)
}

unsafe extern "system" fn enum_monitor_proc(
    monitor: HMONITOR,
    _hdc: HDC,
    _rect: *mut RECT,
    lparam: LPARAM,
) -> BOOL {
    // SAFETY: lparam is the snapshot pointer passed by enumerate_monitor_snapshot
    // on this same thread, alive for the whole enumeration.
    let snapshot = unsafe { &mut *(lparam.0 as *mut MonitorSnapshot) };
    let mut info = MONITORINFOEXW::default();
    info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
    // SAFETY: monitor comes from EnumDisplayMonitors; info points to a
    // properly-sized MONITORINFOEXW whose first field is MONITORINFO.
    let ok = unsafe { GetMonitorInfoW(monitor, &mut info as *mut _ as *mut MONITORINFO) };
    if !ok.as_bool() {
        snapshot.incomplete = true;
        return BOOL(1);
    }
    let id = utf16_z(&info.szDevice);
    let rect = info.monitorInfo.rcMonitor;
    let width = (rect.right - rect.left).max(0) as u32;
    let height = (rect.bottom - rect.top).max(0) as u32;
    if width == 0 || height == 0 {
        snapshot.incomplete = true;
        return BOOL(1);
    }
    let name = id
        .strip_prefix(r"\\.\")
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| id.clone());
    snapshot.displays.push(DisplayHandle {
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
    fn incomplete_topology_is_rejected_only_for_strict_callers() {
        assert!(MonitorSnapshot {
            incomplete: true,
            ..Default::default()
        }
        .finish(true)
        .is_err());
        assert!(MonitorSnapshot {
            incomplete: true,
            ..Default::default()
        }
        .finish(false)
        .is_ok());
        assert!(MonitorSnapshot::default().finish(true).is_ok());
    }

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
