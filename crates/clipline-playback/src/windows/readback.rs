use std::fmt;
use std::time::Instant;

use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11Resource, ID3D11Texture2D, D3D11_CPU_ACCESS_READ,
    D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    DXGI_ERROR_DEVICE_HUNG, DXGI_ERROR_DEVICE_REMOVED, DXGI_ERROR_DEVICE_RESET,
    DXGI_ERROR_DRIVER_INTERNAL_ERROR,
};
use windows_core::Interface;

use super::D3D11VideoSurface;
use crate::{BackendComponent, BackendError, BackendErrorKind, RecoveryDisposition};

pub const MAX_DIAGNOSTIC_RGB_PIXELS: usize = 3_840 * 2_160;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Nv12ReadbackFormat {
    pub width: u32,
    pub height: u32,
    pub rgb_bytes: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Nv12ReadbackSample {
    pub copy_time_100ns: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Nv12ReadbackTelemetry {
    pub configurations: u64,
    pub frames_read: u64,
    pub latest_copy_time_100ns: u64,
    pub max_copy_time_100ns: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Nv12ReadbackError {
    InvalidDimensions {
        width: u32,
        height: u32,
    },
    FrameTooLarge {
        pixels: usize,
        max: usize,
    },
    InvalidStride {
        width: u32,
        y_stride: usize,
        uv_stride: usize,
    },
    InputTooShort {
        actual: usize,
        required: usize,
    },
    OutputSize {
        actual: usize,
        required: usize,
    },
}

impl fmt::Display for Nv12ReadbackError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for Nv12ReadbackError {}

#[derive(Debug, Clone, Copy)]
pub struct Nv12FrameView<'a> {
    bytes: &'a [u8],
    width: usize,
    height: usize,
    y_stride: usize,
    uv_stride: usize,
    uv_offset: usize,
}

impl<'a> Nv12FrameView<'a> {
    pub fn new(
        bytes: &'a [u8],
        width: u32,
        height: u32,
        y_stride: usize,
        uv_stride: usize,
    ) -> Result<Self, Nv12ReadbackError> {
        if width == 0 || height == 0 || !width.is_multiple_of(2) || !height.is_multiple_of(2) {
            return Err(Nv12ReadbackError::InvalidDimensions { width, height });
        }
        let width_usize = width as usize;
        let height_usize = height as usize;
        let pixels =
            width_usize
                .checked_mul(height_usize)
                .ok_or(Nv12ReadbackError::FrameTooLarge {
                    pixels: usize::MAX,
                    max: MAX_DIAGNOSTIC_RGB_PIXELS,
                })?;
        if pixels > MAX_DIAGNOSTIC_RGB_PIXELS {
            return Err(Nv12ReadbackError::FrameTooLarge {
                pixels,
                max: MAX_DIAGNOSTIC_RGB_PIXELS,
            });
        }
        if y_stride < width_usize || uv_stride < width_usize {
            return Err(Nv12ReadbackError::InvalidStride {
                width,
                y_stride,
                uv_stride,
            });
        }
        let uv_offset =
            y_stride
                .checked_mul(height_usize)
                .ok_or(Nv12ReadbackError::InputTooShort {
                    actual: bytes.len(),
                    required: usize::MAX,
                })?;
        let required = uv_stride
            .checked_mul(height_usize / 2)
            .and_then(|chroma| uv_offset.checked_add(chroma))
            .ok_or(Nv12ReadbackError::InputTooShort {
                actual: bytes.len(),
                required: usize::MAX,
            })?;
        if bytes.len() < required {
            return Err(Nv12ReadbackError::InputTooShort {
                actual: bytes.len(),
                required,
            });
        }
        Ok(Self {
            bytes,
            width: width_usize,
            height: height_usize,
            y_stride,
            uv_stride,
            uv_offset,
        })
    }

    pub const fn width(self) -> usize {
        self.width
    }

    pub const fn height(self) -> usize {
        self.height
    }
}

pub fn convert_nv12_to_rgb8(
    frame: Nv12FrameView<'_>,
    output: &mut [u8],
) -> Result<(), Nv12ReadbackError> {
    let required = frame
        .width
        .checked_mul(frame.height)
        .and_then(|pixels| pixels.checked_mul(3))
        .ok_or(Nv12ReadbackError::OutputSize {
            actual: output.len(),
            required: usize::MAX,
        })?;
    if output.len() != required {
        return Err(Nv12ReadbackError::OutputSize {
            actual: output.len(),
            required,
        });
    }

    for y in 0..frame.height {
        let y_row = y * frame.y_stride;
        let uv_row = frame.uv_offset + (y / 2) * frame.uv_stride;
        for x in 0..frame.width {
            let luma = i32::from(frame.bytes[y_row + x]);
            let chroma = uv_row + (x / 2) * 2;
            let u = i32::from(frame.bytes[chroma]);
            let v = i32::from(frame.bytes[chroma + 1]);
            let c = (luma - 16).max(0);
            let d = u - 128;
            let e = v - 128;
            let red = clamp_rgb((298 * c + 459 * e + 128) >> 8);
            let green = clamp_rgb((298 * c - 55 * d - 136 * e + 128) >> 8);
            let blue = clamp_rgb((298 * c + 541 * d + 128) >> 8);
            let destination = (y * frame.width + x) * 3;
            output[destination] = red;
            output[destination + 1] = green;
            output[destination + 2] = blue;
        }
    }
    Ok(())
}

pub struct WindowsNv12Readback {
    device: Option<ID3D11Device>,
    context: Option<ID3D11DeviceContext>,
    staging: Option<ID3D11Texture2D>,
    staging_resource: Option<ID3D11Resource>,
    device_identity: Option<usize>,
    format: Option<Nv12ReadbackFormat>,
    telemetry: Nv12ReadbackTelemetry,
}

impl std::fmt::Debug for WindowsNv12Readback {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WindowsNv12Readback")
            .field("format", &self.format)
            .field("telemetry", &self.telemetry)
            .finish_non_exhaustive()
    }
}

impl Default for WindowsNv12Readback {
    fn default() -> Self {
        Self::new()
    }
}

impl WindowsNv12Readback {
    pub const fn new() -> Self {
        Self {
            device: None,
            context: None,
            staging: None,
            staging_resource: None,
            device_identity: None,
            format: None,
            telemetry: Nv12ReadbackTelemetry {
                configurations: 0,
                frames_read: 0,
                latest_copy_time_100ns: 0,
                max_copy_time_100ns: 0,
            },
        }
    }

    pub const fn telemetry(&self) -> Nv12ReadbackTelemetry {
        self.telemetry
    }

    pub fn configure(
        &mut self,
        surface: &D3D11VideoSurface,
    ) -> Result<Nv12ReadbackFormat, BackendError> {
        let texture = surface.texture();
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: the move-only surface owns a live texture for this call.
        unsafe { texture.GetDesc(&mut desc) };
        let rgb_bytes = rgb_output_bytes(desc.Width, desc.Height).map_err(validation_backend)?;
        if desc.Format != DXGI_FORMAT_NV12 || desc.ArraySize != 1 || desc.MipLevels != 1 {
            return Err(validation_backend(Nv12ReadbackError::InvalidDimensions {
                width: desc.Width,
                height: desc.Height,
            }));
        }
        // SAFETY: the texture is a live D3D11 device child.
        let device = unsafe { texture.GetDevice() }
            .map_err(|error| windows_backend(error, "query diagnostic surface device"))?;
        let identity = device.as_raw() as usize;
        let format = Nv12ReadbackFormat {
            width: desc.Width,
            height: desc.Height,
            rgb_bytes,
        };
        if self.device_identity == Some(identity) && self.format == Some(format) {
            return Ok(format);
        }

        // SAFETY: the live device owns one immediate context.
        let context = unsafe { device.GetImmediateContext() }
            .map_err(|error| windows_backend(error, "query diagnostic D3D11 context"))?;
        let staging_desc = D3D11_TEXTURE2D_DESC {
            Width: desc.Width,
            Height: desc.Height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let mut staging = None;
        // SAFETY: the validated descriptor and out-pointer are live.
        unsafe {
            device
                .CreateTexture2D(&staging_desc, None, Some(&mut staging))
                .map_err(|error| windows_backend(error, "create diagnostic NV12 staging texture"))?
        };
        let staging = staging.ok_or_else(|| {
            validation_message("D3D11 returned no diagnostic NV12 staging texture")
        })?;
        let staging_resource: ID3D11Resource = staging
            .cast()
            .map_err(|error| windows_backend(error, "query diagnostic staging resource"))?;
        self.device = Some(device);
        self.context = Some(context);
        self.staging = Some(staging);
        self.staging_resource = Some(staging_resource);
        self.device_identity = Some(identity);
        self.format = Some(format);
        self.telemetry.configurations = self.telemetry.configurations.saturating_add(1);
        Ok(format)
    }

    pub fn read_rgb8(
        &mut self,
        surface: &D3D11VideoSurface,
        output: &mut [u8],
    ) -> Result<Nv12ReadbackSample, BackendError> {
        let format = self.configure(surface)?;
        if output.len() != format.rgb_bytes {
            return Err(validation_backend(Nv12ReadbackError::OutputSize {
                actual: output.len(),
                required: format.rgb_bytes,
            }));
        }
        let texture = surface.texture();
        // SAFETY: the texture is a live D3D11 device child.
        let source_device = unsafe { texture.GetDevice() }
            .map_err(|error| windows_backend(error, "query diagnostic source device"))?;
        if Some(source_device.as_raw() as usize) != self.device_identity {
            return Err(validation_message(
                "diagnostic readback rejected a surface from another D3D11 device",
            ));
        }
        let source: ID3D11Resource = texture
            .cast()
            .map_err(|error| windows_backend(error, "query diagnostic source resource"))?;
        let context = self
            .context
            .as_ref()
            .ok_or_else(|| validation_message("diagnostic readback has no configured context"))?;
        let staging = self.staging_resource.as_ref().ok_or_else(|| {
            validation_message("diagnostic readback has no configured staging resource")
        })?;
        let started = Instant::now();
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: source/staging share the same validated device and NV12 dimensions.
        unsafe {
            context.CopyResource(staging, &source);
            context
                .Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .map_err(|error| windows_backend(error, "map diagnostic NV12 staging texture"))?;
        }
        let map = D3d11ReadMap::new(context, staging);
        if mapped.pData.is_null() {
            return Err(validation_message(
                "D3D11 mapped a null diagnostic NV12 pointer",
            ));
        }
        let pitch = mapped.RowPitch as usize;
        let mapped_len = pitch
            .checked_mul(format.height as usize)
            .and_then(|luma| {
                pitch
                    .checked_mul(format.height as usize / 2)
                    .and_then(|chroma| luma.checked_add(chroma))
            })
            .ok_or_else(|| validation_message("diagnostic mapped NV12 span overflow"))?;
        // SAFETY: Map succeeded, the pointer is non-null, and D3D11 NV12 staging
        // places height Y rows followed by height/2 interleaved UV rows at RowPitch.
        let bytes = unsafe { std::slice::from_raw_parts(mapped.pData.cast::<u8>(), mapped_len) };
        let view = Nv12FrameView::new(bytes, format.width, format.height, pitch, pitch)
            .map_err(validation_backend)?;
        convert_nv12_to_rgb8(view, output).map_err(validation_backend)?;
        drop(map);
        if let Some(device) = self.device.as_ref() {
            // SAFETY: the configured device remains live for the readback lifetime.
            unsafe { device.GetDeviceRemovedReason() }
                .map_err(|error| windows_backend(error, "finish diagnostic NV12 readback"))?;
        }
        let copy_time_100ns = u64::try_from(started.elapsed().as_nanos() / 100).unwrap_or(u64::MAX);
        self.telemetry.frames_read = self.telemetry.frames_read.saturating_add(1);
        self.telemetry.latest_copy_time_100ns = copy_time_100ns;
        self.telemetry.max_copy_time_100ns =
            self.telemetry.max_copy_time_100ns.max(copy_time_100ns);
        Ok(Nv12ReadbackSample { copy_time_100ns })
    }
}

struct D3d11ReadMap<'a> {
    context: &'a ID3D11DeviceContext,
    resource: &'a ID3D11Resource,
}

impl<'a> D3d11ReadMap<'a> {
    const fn new(context: &'a ID3D11DeviceContext, resource: &'a ID3D11Resource) -> Self {
        Self { context, resource }
    }
}

impl Drop for D3d11ReadMap<'_> {
    fn drop(&mut self) {
        // SAFETY: this guard is created exactly once after a successful Map.
        unsafe { self.context.Unmap(self.resource, 0) };
    }
}

fn rgb_output_bytes(width: u32, height: u32) -> Result<usize, Nv12ReadbackError> {
    if width == 0 || height == 0 || !width.is_multiple_of(2) || !height.is_multiple_of(2) {
        return Err(Nv12ReadbackError::InvalidDimensions { width, height });
    }
    let pixels =
        (width as usize)
            .checked_mul(height as usize)
            .ok_or(Nv12ReadbackError::FrameTooLarge {
                pixels: usize::MAX,
                max: MAX_DIAGNOSTIC_RGB_PIXELS,
            })?;
    if pixels > MAX_DIAGNOSTIC_RGB_PIXELS {
        return Err(Nv12ReadbackError::FrameTooLarge {
            pixels,
            max: MAX_DIAGNOSTIC_RGB_PIXELS,
        });
    }
    pixels
        .checked_mul(3)
        .ok_or(Nv12ReadbackError::FrameTooLarge {
            pixels,
            max: MAX_DIAGNOSTIC_RGB_PIXELS,
        })
}

fn validation_backend(error: Nv12ReadbackError) -> BackendError {
    validation_message(error.to_string())
}

fn validation_message(message: impl Into<String>) -> BackendError {
    BackendError {
        component: BackendComponent::FramePublisher,
        kind: BackendErrorKind::CorruptInput,
        recovery: RecoveryDisposition::RetryPipeline,
        native_code: None,
        message: message.into(),
    }
}

fn windows_backend(error: windows_core::Error, operation: &'static str) -> BackendError {
    let native_code = error.code().0;
    let device_lost = [
        DXGI_ERROR_DEVICE_REMOVED.0,
        DXGI_ERROR_DEVICE_RESET.0,
        DXGI_ERROR_DEVICE_HUNG.0,
        DXGI_ERROR_DRIVER_INTERNAL_ERROR.0,
    ]
    .contains(&native_code);
    BackendError {
        component: BackendComponent::FramePublisher,
        kind: if device_lost {
            BackendErrorKind::DeviceLost
        } else {
            BackendErrorKind::PublicationFailure
        },
        recovery: if device_lost {
            RecoveryDisposition::RecreateComponent
        } else {
            RecoveryDisposition::RetryPipeline
        },
        native_code: Some(i64::from(native_code)),
        message: format!("{operation}: {error}"),
    }
}

fn clamp_rgb(value: i32) -> u8 {
    value.clamp(0, 255) as u8
}
