//! Experimental, read-only use of an undocumented export. No WGC or injection.
//! ABI reference: https://undoc.airesoft.co.uk/user32.dll/DwmGetDxSharedSurface.php
//! This is NOT dwmapi!DwmDxGetWindowSharedSurface (a different driver API).

use std::collections::hash_map::DefaultHasher;
use std::error::Error;
use std::fs::{self, File};
use std::hash::{Hash, Hasher};
use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use clipline_capture::windows::{enumerate_capturable_windows, window_from_raw_handle};
use windows::core::{s, w, BOOL};
use windows::Win32::Foundation::{HANDLE, HMODULE, HWND, LUID};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory4};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows::Win32::UI::WindowsAndMessaging::{GetWindowDisplayAffinity, IsIconic};

type ProbeResult<T> = Result<T, Box<dyn Error>>;
type GetSurface =
    unsafe extern "system" fn(HWND, *mut HANDLE, *mut LUID, *mut u32, *mut u32, *mut u64) -> BOOL;

struct Reader {
    get_surface: GetSurface,
    // These handles are legacy graphics handles, not owned NT handles. Never CloseHandle.
    previous_handle: HANDLE,
    adapter: Option<LUID>,
    device: Option<(ID3D11Device, ID3D11DeviceContext)>,
    staging: Option<(u32, u32, DXGI_FORMAT, ID3D11Texture2D)>,
}

struct Sample {
    bmp: Vec<u8>,
    width: u32,
    height: u32,
    format: i32,
    update_id: u64,
    handle_changed: bool,
}

impl Reader {
    fn new() -> ProbeResult<Self> {
        // SAFETY: user32 is already loaded by the window-manager imports below and
        // remains loaded for this process. Exact null-terminated export name.
        let export = unsafe {
            GetProcAddress(
                GetModuleHandleW(w!("user32.dll"))?,
                s!("DwmGetDxSharedSurface"),
            )
        }
        .ok_or("DwmGetDxSharedSurface is unavailable on this Windows build")?;
        // SAFETY: the experimental ABI is recorded above. No static OS import, and
        // no assumption that an absent export can be replaced by another API.
        let get_surface = unsafe {
            std::mem::transmute::<unsafe extern "system" fn() -> isize, GetSurface>(export)
        };
        Ok(Self {
            get_surface,
            previous_handle: HANDLE::default(),
            adapter: None,
            device: None,
            staging: None,
        })
    }

    fn sample(&mut self, hwnd: HWND) -> ProbeResult<Sample> {
        if window_from_raw_handle(hwnd.0 as isize).is_none() {
            return Err("target window closed or hidden".into());
        }
        let mut affinity = 0;
        // SAFETY: borrowed HWND; these calls do not modify the target. A failed
        // affinity query is an error, never permission to capture an excluded window.
        unsafe {
            GetWindowDisplayAffinity(hwnd, &mut affinity)?;
            if affinity != 0 {
                return Err("target has capture protection enabled".into());
            }
            if IsIconic(hwnd).as_bool() {
                return Err("target is minimized".into());
            }
        }
        let mut handle = HANDLE::default();
        let mut luid = LUID::default();
        let (mut format, mut flags, mut update_id) = (0, 0, 0);
        // SAFETY: each out-pointer has the ABI's correct size and lives through the
        // call. A false BOOL does not promise a meaningful GetLastError value.
        let available = unsafe {
            (self.get_surface)(
                hwnd,
                &mut handle,
                &mut luid,
                &mut format,
                &mut flags,
                &mut update_id,
            )
        };
        if !available.as_bool() || handle.is_invalid() {
            return Err("DWM returned no shared surface (unsupported window/render mode)".into());
        }
        if self.adapter != Some(luid) {
            // SAFETY: match the producer's adapter instead of guessing the default GPU.
            let factory: IDXGIFactory4 = unsafe { CreateDXGIFactory1()? };
            let adapter: IDXGIAdapter1 = unsafe { factory.EnumAdapterByLuid(luid)? };
            let desc = unsafe { adapter.GetDesc1()? };
            let end = desc
                .Description
                .iter()
                .position(|c| *c == 0)
                .unwrap_or(desc.Description.len());
            println!(
                "adapter={} luid={:08x}:{:08x}",
                String::from_utf16_lossy(&desc.Description[..end]),
                luid.HighPart,
                luid.LowPart
            );
            let (mut device, mut context) = (None, None);
            // SAFETY: adapter is live, UNKNOWN is required for an explicit adapter,
            // and both out-pointers are owned Options.
            unsafe {
                D3D11CreateDevice(
                    &adapter,
                    D3D_DRIVER_TYPE_UNKNOWN,
                    HMODULE::default(),
                    D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                    None,
                    D3D11_SDK_VERSION,
                    Some(&mut device),
                    None,
                    Some(&mut context),
                )?;
            }
            self.device = Some((
                device.ok_or("D3D11 returned no device")?,
                context.ok_or("D3D11 returned no context")?,
            ));
            self.adapter = Some(luid);
            self.staging = None;
        }
        let (device, context) = self.device.as_ref().ok_or("device missing")?;
        let mut shared: Option<ID3D11Texture2D> = None;
        // SAFETY: opens the borrowed legacy graphics handle; the resulting COM
        // reference is owned. Reopen each sample to avoid caching an obsolete surface.
        unsafe {
            device.OpenSharedResource(handle, &mut shared)?;
        }
        let shared = shared.ok_or("OpenSharedResource returned no texture")?;
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe {
            shared.GetDesc(&mut desc);
        }
        let rgba = match desc.Format {
            DXGI_FORMAT_R8G8B8A8_UNORM | DXGI_FORMAT_R8G8B8A8_UNORM_SRGB => true,
            DXGI_FORMAT_B8G8R8A8_UNORM
            | DXGI_FORMAT_B8G8R8A8_UNORM_SRGB
            | DXGI_FORMAT_B8G8R8X8_UNORM
            | DXGI_FORMAT_B8G8R8X8_UNORM_SRGB => false,
            _ => {
                return Err(format!(
                    "unsupported texture format {:?}; probe supports 32-bit SDR only",
                    desc.Format
                )
                .into())
            }
        };
        if desc.MipLevels != 1
            || desc.ArraySize != 1
            || desc.SampleDesc.Count != 1
            || desc.Width == 0
            || desc.Height == 0
            || u64::from(desc.Width) * u64::from(desc.Height) > 64 * 1024 * 1024
            || desc.MiscFlags & D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX.0 as u32 != 0
        {
            return Err(
                "unsupported texture dimensions/layout or keyed-mutex synchronization".into(),
            );
        }
        if self
            .staging
            .as_ref()
            .is_none_or(|(w, h, f, _)| (*w, *h, *f) != (desc.Width, desc.Height, desc.Format))
        {
            let staging_desc = D3D11_TEXTURE2D_DESC {
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                MiscFlags: 0,
                ..desc
            };
            let mut texture = None;
            unsafe {
                device.CreateTexture2D(&staging_desc, None, Some(&mut texture))?;
            }
            self.staging = Some((
                desc.Width,
                desc.Height,
                desc.Format,
                texture.ok_or("no staging texture")?,
            ));
        }
        let staging = &self.staging.as_ref().ok_or("staging missing")?.3;
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: identical single-subresource layouts. Only the probe-owned staging
        // texture is written/mapped. Map waits for our copy, not for a DWM frame boundary.
        // ponytail: unsynchronized producer read, validate tearing before any production backend.
        unsafe {
            context.CopyResource(staging, &shared);
            context.Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
        }
        let bmp = (|| -> ProbeResult<Vec<u8>> {
            let length = (mapped.RowPitch as usize)
                .checked_mul(desc.Height as usize)
                .ok_or("mapped size overflow")?;
            if mapped.pData.is_null() || length > 512 * 1024 * 1024 {
                return Err("invalid mapped allocation".into());
            }
            // SAFETY: Map succeeded; D3D11 guarantees RowPitch bytes per row for this
            // validated uncompressed texture. This slice never escapes the mapped period.
            let bytes = unsafe { std::slice::from_raw_parts(mapped.pData.cast::<u8>(), length) };
            let mut bmp =
                super::bmp_bytes(desc.Width, desc.Height, mapped.RowPitch as usize, bytes)?;
            for pixel in bmp[54..].as_chunks_mut::<4>().0 {
                if rgba {
                    pixel.swap(0, 2);
                }
                pixel[3] = 255;
            }
            Ok(bmp)
        })();
        // SAFETY: unmap on success AND conversion error, after all borrowed bytes expire.
        unsafe {
            context.Unmap(staging, 0);
        }
        let bmp = bmp?;
        let handle_changed = self.previous_handle != handle;
        self.previous_handle = handle;
        Ok(Sample {
            bmp,
            width: desc.Width,
            height: desc.Height,
            format: desc.Format.0,
            update_id,
            handle_changed,
        })
    }
}

pub fn list() {
    for window in enumerate_capturable_windows() {
        println!("{}\t{}\t{}", window.handle, window.exe_name, window.title);
    }
}

pub fn run(options: super::Options) -> ProbeResult<()> {
    let hwnd = if let Some(raw) = options.hwnd {
        window_from_raw_handle(raw).ok_or("invalid or invisible HWND")?
    } else {
        let needle = options
            .window
            .as_deref()
            .ok_or("target missing")?
            .to_lowercase();
        let matches: Vec<_> = enumerate_capturable_windows()
            .into_iter()
            .filter(|window| window.title.to_lowercase().contains(&needle))
            .collect();
        if matches.len() != 1 {
            return Err(format!(
                "expected one matching window, found {}; use --list and --hwnd",
                matches.len()
            )
            .into());
        }
        window_from_raw_handle(matches[0].handle).ok_or("window disappeared")?
    };
    let mut reader = Reader::new()?;
    let output = options.output.unwrap_or_else(|| {
        PathBuf::from(format!(
            "dwm-probe-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
        ))
    });
    fs::create_dir(&output)?; // Never overwrite an existing experiment.
    let output = fs::canonicalize(output)?;
    println!("DWM ONLY; hwnd={} windows_build={:?}\noutput={}\nKeep the target animating. Images may contain its visible content.", hwnd.0 as isize, clipline_capture::windows::wasapi::windows_build_number(), output.display());
    let mut csv = File::create(output.join("samples.csv"))?;
    writeln!(
        csv,
        "elapsed_s,read_ms,width,height,format,update_id,handle_changed,pixel_hash,error"
    )?;
    let start = Instant::now();
    let duration = Duration::from_secs(options.seconds);
    let interval = Duration::from_secs_f64(1.0 / f64::from(options.fps));
    let (mut reads, mut changes, mut errors, mut total_ms, mut max_ms) =
        (0u64, 0u64, 0u64, 0.0, 0.0f64);
    let (mut previous_hash, mut last_bmp) = (None, None);
    let mut midpoint_saved = false;
    let mut previous_error = String::new();
    while start.elapsed() < duration {
        let tick = Instant::now();
        match reader.sample(hwnd) {
            Ok(sample) => {
                let read_ms = tick.elapsed().as_secs_f64() * 1000.0;
                total_ms += read_ms;
                max_ms = max_ms.max(read_ms);
                let mut hasher = DefaultHasher::new();
                sample.bmp.hash(&mut hasher);
                let hash = hasher.finish();
                if previous_hash.is_some_and(|previous| previous != hash) {
                    changes += 1;
                }
                previous_hash = Some(hash);
                if reads == 0 {
                    fs::write(output.join("first.bmp"), &sample.bmp)?;
                }
                if !midpoint_saved && start.elapsed() >= duration / 2 {
                    fs::write(output.join("middle.bmp"), &sample.bmp)?;
                    midpoint_saved = true;
                }
                writeln!(
                    csv,
                    "{:.6},{read_ms:.3},{},{},{},{},{},{hash:016x},",
                    start.elapsed().as_secs_f64(),
                    sample.width,
                    sample.height,
                    sample.format,
                    sample.update_id,
                    sample.handle_changed
                )?;
                last_bmp = Some(sample.bmp);
                reads += 1;
                previous_error.clear();
            }
            Err(error) => {
                errors += 1;
                let message = error.to_string().replace(['\r', '\n'], " ");
                if previous_error != message {
                    eprintln!("sample unavailable: {message}");
                    previous_error = message.clone();
                }
                writeln!(
                    csv,
                    "{:.6},{:.3},,,,,,,\"{}\"",
                    start.elapsed().as_secs_f64(),
                    tick.elapsed().as_secs_f64() * 1000.0,
                    message.replace('"', "\"\"")
                )?;
            }
        }
        std::thread::sleep(interval.saturating_sub(tick.elapsed()));
    }
    if let Some(bmp) = last_bmp {
        fs::write(output.join("last.bmp"), bmp)?;
    }
    let elapsed = start.elapsed().as_secs_f64();
    let summary = format!("elapsed_s={elapsed:.3}\nsuccessful_reads={reads}\nchanged_samples={changes}\nerrors={errors}\nreads_per_second={:.2}\nmean_read_ms={:.3}\nmax_read_ms={max_ms:.3}\nChanges are sampled pixel differences, NOT unique game FPS or proof of tear-free capture.\n", reads as f64 / elapsed, total_ms / reads.max(1) as f64);
    fs::write(output.join("summary.txt"), &summary)?;
    print!("{summary}");
    // Evidence is already persisted, including for a failed motion-required run.
    super::capture_verdict(reads, changes, options.require_motion)?;
    if changes == 0 {
        eprintln!("No pixel changes detected: animate the target before judging freshness.");
    }
    Ok(())
}
