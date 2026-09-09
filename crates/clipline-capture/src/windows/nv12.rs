//! GPU BGRA→NV12 conversion + scaling via the D3D11 video processor —
//! WGC delivers BGRA, H.264 encoder MFTs consume NV12; ddoc §3/§7 require
//! the path to stay on the GPU.

use windows::core::{Error as WinError, Interface, Result as WinResult};
use windows::Win32::Foundation::{E_FAIL, RECT};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11Resource, ID3D11Texture2D, ID3D11VideoContext,
    ID3D11VideoContext1, ID3D11VideoDevice, ID3D11VideoProcessor, ID3D11VideoProcessorEnumerator,
    D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV,
    D3D11_VIDEO_COLOR, D3D11_VIDEO_COLOR_0, D3D11_VIDEO_COLOR_RGBA,
    D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE, D3D11_VIDEO_PROCESSOR_COLOR_SPACE,
    D3D11_VIDEO_PROCESSOR_CONTENT_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_STREAM,
    D3D11_VIDEO_USAGE_PLAYBACK_NORMAL, D3D11_VPIV_DIMENSION_TEXTURE2D,
    D3D11_VPOV_DIMENSION_TEXTURE2D,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709,
    DXGI_RATIONAL,
};

use crate::windows::d3d11;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CropRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl CropRect {
    pub fn in_frame(self, frame_width: u32, frame_height: u32) -> Option<Self> {
        let right = self.x.checked_add(self.width)?;
        let bottom = self.y.checked_add(self.height)?;
        if self.width < 2 || self.height < 2 || right > frame_width || bottom > frame_height {
            return None;
        }
        Some(self)
    }

    pub fn from_i32_in_frame(
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        frame_width: i32,
        frame_height: i32,
    ) -> Option<Self> {
        if x < 0 || y < 0 {
            return None;
        }
        let frame_width = u32::try_from(frame_width).ok()?;
        let frame_height = u32::try_from(frame_height).ok()?;
        Self {
            x: u32::try_from(x).ok()?,
            y: u32::try_from(y).ok()?,
            width: u32::try_from(width).ok()?,
            height: u32::try_from(height).ok()?,
        }
        .in_frame(frame_width, frame_height)
    }

    pub fn is_full_frame(self, frame_width: u32, frame_height: u32) -> bool {
        self.x == 0 && self.y == 0 && self.width == frame_width && self.height == frame_height
    }

    fn to_rect(self) -> RECT {
        RECT {
            left: self.x as i32,
            top: self.y as i32,
            right: (self.x + self.width) as i32,
            bottom: (self.y + self.height) as i32,
        }
    }
}

/// One converter per recording. The MP4 output size stays fixed, but window
/// captures can resize, so the video processor is rebuilt when input changes.
pub struct VideoConverter {
    device: ID3D11Device,
    video_context: ID3D11VideoContext,
    video_device: ID3D11VideoDevice,
    processor: ID3D11VideoProcessor,
    enumerator: ID3D11VideoProcessorEnumerator,
    in_width: u32,
    in_height: u32,
    out_width: u32,
    out_height: u32,
    crop: Option<CropRect>,
}

impl VideoConverter {
    pub fn new(
        device: &ID3D11Device,
        in_w: u32,
        in_h: u32,
        out_w: u32,
        out_h: u32,
    ) -> WinResult<Self> {
        Self::new_with_crop(device, in_w, in_h, out_w, out_h, None)
    }

    pub fn new_with_crop(
        device: &ID3D11Device,
        in_w: u32,
        in_h: u32,
        out_w: u32,
        out_h: u32,
        crop: Option<CropRect>,
    ) -> WinResult<Self> {
        conversion_rects(in_w, in_h, out_w, out_h, crop)?;
        d3d11::ensure_multithread_protected(device)?;
        let video_device: ID3D11VideoDevice = device.cast()?;
        // SAFETY: trivial getter on a valid device.
        let video_context: ID3D11VideoContext = unsafe { device.GetImmediateContext()? }.cast()?;
        let (enumerator, processor) =
            create_video_processor(&video_device, in_w, in_h, out_w, out_h)?;
        configure_video_processor_color_spaces(&video_context, &processor);
        Ok(Self {
            device: device.clone(),
            video_context,
            video_device,
            processor,
            enumerator,
            in_width: in_w,
            in_height: in_h,
            out_width: out_w,
            out_height: out_h,
            crop,
        })
    }

    /// Convert one BGRA texture into a freshly allocated NV12 texture
    /// (the encoder holds frames asynchronously; pooling is a follow-up).
    pub fn convert(&mut self, bgra: &ID3D11Texture2D) -> WinResult<ID3D11Texture2D> {
        let (in_width, in_height) = d3d11::texture_size(bgra);
        let (source_rect, dest_rect) = conversion_rects(
            in_width,
            in_height,
            self.out_width,
            self.out_height,
            self.crop,
        )?;
        if (in_width, in_height) != (self.in_width, self.in_height) {
            let (enumerator, processor) = create_video_processor(
                &self.video_device,
                in_width,
                in_height,
                self.out_width,
                self.out_height,
            )?;
            configure_video_processor_color_spaces(&self.video_context, &processor);
            self.enumerator = enumerator;
            self.processor = processor;
            self.in_width = in_width;
            self.in_height = in_height;
        }
        let out = d3d11::create_nv12_texture(&self.device, self.out_width, self.out_height)?;

        let in_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPIV {
                    MipSlice: 0,
                    ArraySlice: 0,
                },
            },
        };
        let mut in_view = None;
        // SAFETY: bgra is a live texture on self.device; desc initialized.
        unsafe {
            self.video_device.CreateVideoProcessorInputView(
                bgra,
                &self.enumerator,
                &in_desc,
                Some(&mut in_view),
            )?;
        }

        let out_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
            },
        };
        let mut out_view = None;
        // SAFETY: out is a render-target NV12 texture on self.device.
        unsafe {
            self.video_device.CreateVideoProcessorOutputView(
                &out,
                &self.enumerator,
                &out_desc,
                Some(&mut out_view),
            )?;
        }

        let target_rect = RECT {
            left: 0,
            top: 0,
            right: self.out_width as i32,
            bottom: self.out_height as i32,
        };
        let background = D3D11_VIDEO_COLOR {
            Anonymous: D3D11_VIDEO_COLOR_0 {
                RGBA: D3D11_VIDEO_COLOR_RGBA {
                    R: 0.0,
                    G: 0.0,
                    B: 0.0,
                    A: 1.0,
                },
            },
        };
        // SAFETY: live processor; validated source/destination rectangles. Fill
        // the entire output so freshly allocated letterbox pixels are defined.
        unsafe {
            self.video_context.VideoProcessorSetStreamSourceRect(
                &self.processor,
                0,
                true,
                Some(&source_rect),
            );
            self.video_context.VideoProcessorSetStreamDestRect(
                &self.processor,
                0,
                true,
                Some(&dest_rect),
            );
            self.video_context.VideoProcessorSetOutputTargetRect(
                &self.processor,
                true,
                Some(&target_rect),
            );
            self.video_context.VideoProcessorSetOutputBackgroundColor(
                &self.processor,
                false,
                &background,
            );
        }
        let stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: true.into(),
            pInputSurface: std::mem::ManuallyDrop::new(in_view),
            ..Default::default()
        };
        // SAFETY: processor/views are live; one enabled stream, no past or
        // future frames. ManuallyDrop field: we drop the view ourselves after.
        let result = unsafe {
            self.video_context.VideoProcessorBlt(
                &self.processor,
                out_view.as_ref().expect("out view set on Ok"),
                0,
                std::slice::from_ref(&stream),
            )
        };
        // Reclaim the input view reference wrapped in ManuallyDrop.
        drop(std::mem::ManuallyDrop::into_inner(stream.pInputSurface));
        result?;
        Ok(out)
    }
}

fn conversion_rects(
    in_w: u32,
    in_h: u32,
    out_w: u32,
    out_h: u32,
    crop: Option<CropRect>,
) -> WinResult<(RECT, RECT)> {
    if in_w > i32::MAX as u32 || in_h > i32::MAX as u32 {
        return Err(WinError::new(
            E_FAIL,
            "input dimensions exceed RECT coordinates",
        ));
    }
    let source = crop
        .unwrap_or(CropRect {
            x: 0,
            y: 0,
            width: in_w,
            height: in_h,
        })
        .in_frame(in_w, in_h)
        .ok_or_else(|| WinError::new(E_FAIL, "source crop is outside the current frame"))?;
    let dest = crate::video_layout::fitted_video_rect(source.width, source.height, out_w, out_h)
        .map_err(|message| WinError::new(E_FAIL, message))?;
    Ok((
        source.to_rect(),
        RECT {
            left: dest.x as i32,
            top: dest.y as i32,
            right: (dest.x + dest.width) as i32,
            bottom: (dest.y + dest.height) as i32,
        },
    ))
}

fn create_video_processor(
    video_device: &ID3D11VideoDevice,
    in_w: u32,
    in_h: u32,
    out_w: u32,
    out_h: u32,
) -> WinResult<(ID3D11VideoProcessorEnumerator, ID3D11VideoProcessor)> {
    let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
        InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
        InputFrameRate: DXGI_RATIONAL {
            Numerator: 60,
            Denominator: 1,
        },
        InputWidth: in_w,
        InputHeight: in_h,
        OutputFrameRate: DXGI_RATIONAL {
            Numerator: 60,
            Denominator: 1,
        },
        OutputWidth: out_w,
        OutputHeight: out_h,
        Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
    };
    // SAFETY: desc is fully initialized; out-params are valid.
    let enumerator = unsafe { video_device.CreateVideoProcessorEnumerator(&desc)? };
    // SAFETY: enumerator is valid; rate-conversion caps index 0 always exists.
    let processor = unsafe { video_device.CreateVideoProcessor(&enumerator, 0)? };
    Ok((enumerator, processor))
}

fn configure_video_processor_color_spaces(
    video_context: &ID3D11VideoContext,
    processor: &ID3D11VideoProcessor,
) {
    // WGC gives us SDR BGRA in full-range RGB. Encoders receive NV12, which
    // standard players expect as limited-range Rec.709 for screen recordings.
    // Letting drivers infer this caused dark/saturated clips on some systems.
    if let Ok(ctx1) = video_context.cast::<ID3D11VideoContext1>() {
        unsafe {
            ctx1.VideoProcessorSetStreamColorSpace1(
                processor,
                0,
                DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
            );
            ctx1.VideoProcessorSetOutputColorSpace1(
                processor,
                DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709,
            );
        }
        return;
    }

    // Older D3D11.0 fallback: Usage=playback, RGB input full-range, YCbCr
    // output with BT.709 matrix and 16-235 nominal range.
    let rgb_full_709 = D3D11_VIDEO_PROCESSOR_COLOR_SPACE {
        _bitfield: (2 << 4),
    };
    let ycbcr_limited_709 = D3D11_VIDEO_PROCESSOR_COLOR_SPACE {
        _bitfield: (1 << 2) | (1 << 4),
    };
    unsafe {
        video_context.VideoProcessorSetStreamColorSpace(processor, 0, &rgb_full_709);
        video_context.VideoProcessorSetOutputColorSpace(processor, &ycbcr_limited_709);
    }
}

/// Copy a GPU NV12 texture to a staging texture and pack it into contiguous
/// NV12 bytes (Y plane `width*height`, then interleaved UV `width*height/2`)
/// for piping to FFmpeg. Maps with the texture's row pitch and reads the UV
/// plane immediately after the Y plane — the conventional D3D11 NV12 layout.
/// Dimensions come from `src` itself so the packed size can't drift from the
/// texture the caller actually produced.
pub fn read_nv12(device: &ID3D11Device, src: &ID3D11Texture2D) -> WinResult<Vec<u8>> {
    d3d11::ensure_multithread_protected(device)?;
    let (width, height) = d3d11::texture_size(src);
    let staging = d3d11::create_nv12_staging(device, width, height)?;
    let dst: ID3D11Resource = staging.cast()?;
    let source: ID3D11Resource = src.cast()?;
    // SAFETY: trivial getter; the device owns a single immediate context.
    let ctx = unsafe { device.GetImmediateContext()? };
    // SAFETY: dst/source are valid resources of identical NV12 descriptions.
    unsafe {
        ctx.CopyResource(&dst, &source);
    }
    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    unsafe {
        ctx.Map(&dst, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
    }
    let _map = D3d11ReadMap::new(&ctx, &dst);
    let layout = nv12_readback_layout(width, height, mapped.RowPitch)?;
    if mapped.pData.is_null() {
        return Err(WinError::new(E_FAIL, "invalid mapped NV12 texture pointer"));
    }
    let mut out = vec![0u8; layout.packed_len];
    let base = mapped.pData as *const u8;
    for row in 0..layout.rows {
        let source_offset = row * layout.pitch;
        // SAFETY: layout validation proves the complete row lies within the
        // representable mapped span and `base` is non-null until `_map` drops.
        let source =
            unsafe { std::slice::from_raw_parts(base.add(source_offset), layout.row_bytes) };
        out[row * layout.row_bytes..(row + 1) * layout.row_bytes].copy_from_slice(source);
    }
    // SAFETY: `uv_offset` and every following UV row were checked as part of
    // `mapped_span`; D3D11 places the interleaved UV plane immediately after Y.
    let uv_base = unsafe { base.add(layout.uv_offset) };
    for row in 0..layout.uv_rows {
        let source_offset = row * layout.pitch;
        let source =
            unsafe { std::slice::from_raw_parts(uv_base.add(source_offset), layout.row_bytes) };
        let output_offset = layout.y_len + row * layout.row_bytes;
        out[output_offset..output_offset + layout.row_bytes].copy_from_slice(source);
    }
    Ok(out)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Nv12ReadbackLayout {
    row_bytes: usize,
    rows: usize,
    uv_rows: usize,
    pitch: usize,
    y_len: usize,
    uv_offset: usize,
    packed_len: usize,
    mapped_span: usize,
}

fn nv12_readback_layout(width: u32, height: u32, row_pitch: u32) -> WinResult<Nv12ReadbackLayout> {
    if width == 0 || height == 0 || !width.is_multiple_of(2) || !height.is_multiple_of(2) {
        return Err(WinError::new(
            E_FAIL,
            "invalid mapped NV12 texture dimensions",
        ));
    }
    let row_bytes =
        usize::try_from(width).map_err(|_| WinError::new(E_FAIL, "NV12 row size overflow"))?;
    let rows =
        usize::try_from(height).map_err(|_| WinError::new(E_FAIL, "NV12 row count overflow"))?;
    let uv_rows = rows / 2;
    let pitch =
        usize::try_from(row_pitch).map_err(|_| WinError::new(E_FAIL, "NV12 row pitch overflow"))?;
    if pitch < row_bytes {
        return Err(WinError::new(E_FAIL, "invalid mapped NV12 row pitch"));
    }
    let y_len = row_bytes
        .checked_mul(rows)
        .ok_or_else(|| WinError::new(E_FAIL, "NV12 Y-plane size overflow"))?;
    let uv_len = row_bytes
        .checked_mul(uv_rows)
        .ok_or_else(|| WinError::new(E_FAIL, "NV12 UV-plane size overflow"))?;
    let packed_len = y_len
        .checked_add(uv_len)
        .ok_or_else(|| WinError::new(E_FAIL, "NV12 readback size overflow"))?;
    let uv_offset = pitch
        .checked_mul(rows)
        .ok_or_else(|| WinError::new(E_FAIL, "NV12 UV offset overflow"))?;
    let final_uv_offset = pitch
        .checked_mul(uv_rows - 1)
        .and_then(|offset| offset.checked_add(row_bytes))
        .ok_or_else(|| WinError::new(E_FAIL, "NV12 mapped span overflow"))?;
    let mapped_span = uv_offset
        .checked_add(final_uv_offset)
        .filter(|span| *span <= isize::MAX as usize)
        .ok_or_else(|| WinError::new(E_FAIL, "NV12 mapped span overflow"))?;
    Ok(Nv12ReadbackLayout {
        row_bytes,
        rows,
        uv_rows,
        pitch,
        y_len,
        uv_offset,
        packed_len,
        mapped_span,
    })
}

struct D3d11ReadMap<'a> {
    context: &'a ID3D11DeviceContext,
    resource: &'a ID3D11Resource,
}

impl<'a> D3d11ReadMap<'a> {
    fn new(context: &'a ID3D11DeviceContext, resource: &'a ID3D11Resource) -> Self {
        Self { context, resource }
    }
}

impl Drop for D3d11ReadMap<'_> {
    fn drop(&mut self) {
        // SAFETY: the guard is created exactly once after a successful Map
        // and owns the corresponding single Unmap on every exit path.
        unsafe { self.context.Unmap(self.resource, 0) };
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct BgraReadback {
    pub bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub stride: usize,
}

/// Copy a GPU BGRA texture into CPU-readable packed rows. This path uses only
/// the core D3D11 device/context APIs and therefore works on WARP and Microsoft's
/// Basic Display Adapter without `ID3D11VideoDevice`.
pub fn read_bgra(device: &ID3D11Device, src: &ID3D11Texture2D) -> WinResult<BgraReadback> {
    d3d11::ensure_multithread_protected(device)?;
    let (width, height) = d3d11::texture_size(src);
    let staging = d3d11::create_bgra_staging(device, width, height)?;
    let dst: ID3D11Resource = staging.cast()?;
    let source: ID3D11Resource = src.cast()?;
    let ctx = unsafe { device.GetImmediateContext()? };
    let row_bytes = usize::try_from(width)
        .ok()
        .and_then(|value| value.checked_mul(4))
        .ok_or_else(|| WinError::new(E_FAIL, "BGRA row size overflow"))?;
    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    unsafe {
        ctx.CopyResource(&dst, &source);
        ctx.Map(&dst, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
    }
    let _map = D3d11ReadMap::new(&ctx, &dst);
    let result = (|| {
        let pitch = mapped.RowPitch as usize;
        if mapped.pData.is_null() || pitch < row_bytes {
            return Err(WinError::new(E_FAIL, "invalid mapped BGRA texture"));
        }
        let len = row_bytes
            .checked_mul(height as usize)
            .ok_or_else(|| WinError::new(E_FAIL, "BGRA readback size overflow"))?;
        let mut bytes = vec![0u8; len];
        let base = mapped.pData as *const u8;
        for row in 0..height as usize {
            let source = unsafe { std::slice::from_raw_parts(base.add(row * pitch), row_bytes) };
            bytes[row * row_bytes..(row + 1) * row_bytes].copy_from_slice(source);
        }
        Ok(BgraReadback {
            bytes,
            width,
            height,
            stride: row_bytes,
        })
    })();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crop_layout_uses_crop_shape_and_rejects_shrunk_input() {
        let crop = Some(CropRect {
            x: 16,
            y: 8,
            width: 32,
            height: 24,
        });
        let (_, dest) = conversion_rects(96, 64, 64, 64, crop).unwrap();
        assert_eq!(
            (dest.left, dest.top, dest.right, dest.bottom),
            (0, 8, 64, 56)
        );
        assert!(conversion_rects(40, 24, 64, 64, crop).is_err());
        assert!(conversion_rects(0, 64, 64, 64, None).is_err());
    }

    #[test]
    fn hardware_letterbox_pixels_follow_resize_and_crop() {
        if std::env::var_os("CI").is_some() {
            return;
        }
        let (device, context) = match d3d11::create_device() {
            Ok(pair) => pair,
            Err(e) => {
                eprintln!("SKIP: no hardware D3D11 device: {e}");
                return;
            }
        };
        let mut conv = match VideoConverter::new(&device, 128, 64, 64, 64) {
            Ok(converter) => converter,
            Err(e) => {
                eprintln!("SKIP: video processor unavailable: {e}");
                return;
            }
        };
        for (width, height, crop, expected) in [
            (128, 64, None, (0, 16, 64, 32)),
            (64, 128, None, (16, 0, 32, 64)),
            (64, 64, None, (0, 0, 64, 64)),
            (
                128,
                64,
                Some(CropRect {
                    x: 16,
                    y: 8,
                    width: 32,
                    height: 48,
                }),
                (10, 0, 42, 64),
            ),
        ] {
            conv.crop = crop;
            let src = d3d11::create_bgra_texture(&device, width, height).unwrap();
            let pixels = vec![255u8; (width * height * 4) as usize];
            let resource: ID3D11Resource = src.cast().unwrap();
            // SAFETY: initialized BGRA bytes cover every row of the live texture.
            unsafe {
                context.UpdateSubresource(&resource, 0, None, pixels.as_ptr().cast(), width * 4, 0);
            }
            let output = conv.convert(&src).unwrap();
            let bytes = read_nv12(&device, &output).unwrap();
            let (left, top, w, h) = expected;
            for y in 0..64 {
                for x in 0..64 {
                    let inside = x >= left && x < left + w && y >= top && y < top + h;
                    let expected_y = if inside { 235i16 } else { 16i16 };
                    assert!(
                        (i16::from(bytes[y * 64 + x]) - expected_y).abs() <= 3,
                        "source {width}x{height}, pixel ({x},{y}): {} expected {expected_y}",
                        bytes[y * 64 + x]
                    );
                }
            }
            assert!(bytes[64 * 64..]
                .iter()
                .all(|&v| (i16::from(v) - 128).abs() <= 3));
        }
        conv.crop = Some(CropRect {
            x: 16,
            y: 8,
            width: 32,
            height: 48,
        });
        let shrunk = d3d11::create_bgra_texture(&device, 32, 32).unwrap();
        assert!(conv.convert(&shrunk).is_err());
    }

    #[test]
    fn nv12_layout_checks_pitch_dimensions_offsets_and_overflow() {
        let layout = nv12_readback_layout(4, 4, 8).expect("valid padded NV12 layout");
        assert_eq!(layout.row_bytes, 4);
        assert_eq!(layout.rows, 4);
        assert_eq!(layout.uv_rows, 2);
        assert_eq!(layout.y_len, 16);
        assert_eq!(layout.uv_offset, 32);
        assert_eq!(layout.packed_len, 24);
        assert_eq!(layout.mapped_span, 44);

        for invalid in [
            nv12_readback_layout(4, 4, 3),
            nv12_readback_layout(0, 4, 4),
            nv12_readback_layout(4, 0, 4),
            nv12_readback_layout(3, 4, 4),
            nv12_readback_layout(4, 3, 4),
            nv12_readback_layout(u32::MAX - 1, u32::MAX - 1, u32::MAX - 1),
            nv12_readback_layout(2, u32::MAX - 1, u32::MAX),
        ] {
            assert!(invalid.is_err());
        }
    }

    #[test]
    fn reads_bgra_texture_on_warp_without_video_processor() {
        let (device, context) =
            crate::windows::d3d11::create_unprotected_device_for_tests().unwrap();
        let multithread: windows::Win32::Graphics::Direct3D10::ID3D10Multithread =
            device.cast().unwrap();
        assert!(!unsafe { multithread.GetMultithreadProtected() }.as_bool());
        let texture = crate::windows::d3d11::create_bgra_texture(&device, 2, 2).unwrap();
        let resource: ID3D11Resource = texture.cast().unwrap();
        let pixels = [
            1u8, 2, 3, 255, 4, 5, 6, 255, // row 0
            7, 8, 9, 255, 10, 11, 12, 255, // row 1
        ];
        unsafe {
            context.UpdateSubresource(&resource, 0, None, pixels.as_ptr().cast(), 8, 0);
        }

        let readback = read_bgra(&device, &texture).unwrap();

        assert!(unsafe { multithread.GetMultithreadProtected() }.as_bool());
        assert_eq!((readback.width, readback.height), (2, 2));
        assert_eq!(readback.stride, 8);
        assert_eq!(readback.bytes, pixels);
    }

    #[test]
    fn reads_back_converted_nv12_as_contiguous_bytes() {
        // WARP has no ID3D11VideoDevice — needs real hardware; skips on CI.
        let device = match crate::windows::d3d11::create_device() {
            Ok((device, _ctx)) => device,
            Err(e) => {
                eprintln!("SKIP: no hardware D3D11 device: {e}");
                return;
            }
        };
        let mut conv = match VideoConverter::new(&device, 64, 64, 64, 64) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("SKIP: video processor unavailable: {e}");
                return;
            }
        };
        let src = crate::windows::d3d11::create_bgra_texture(&device, 64, 64).expect("src");
        let nv12 = conv.convert(&src).expect("convert");
        let bytes = read_nv12(&device, &nv12).expect("readback");
        assert_eq!(bytes.len(), 64 * 64 * 3 / 2, "tightly packed NV12");
    }

    #[test]
    fn converts_bgra_texture_to_nv12_with_scaling() {
        // WARP has no ID3D11VideoDevice — needs real hardware; skips on CI.
        let device = match crate::windows::d3d11::create_device() {
            Ok((device, _ctx)) => device,
            Err(e) => {
                eprintln!("SKIP: no hardware D3D11 device: {e}");
                return;
            }
        };
        let mut conv = match VideoConverter::new(&device, 64, 64, 32, 32) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("SKIP: video processor unavailable: {e}");
                return;
            }
        };
        let src = crate::windows::d3d11::create_bgra_texture(&device, 64, 64).expect("src");
        let nv12 = conv.convert(&src).expect("convert");
        let desc = crate::windows::d3d11::texture_desc(&nv12);
        assert_eq!((desc.Width, desc.Height), (32, 32));
        assert_eq!(
            desc.Format,
            windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_NV12
        );
    }

    #[test]
    fn converts_bgra_texture_with_source_crop() {
        // WARP has no ID3D11VideoDevice — needs real hardware; skips on CI.
        let device = match crate::windows::d3d11::create_device() {
            Ok((device, _ctx)) => device,
            Err(e) => {
                eprintln!("SKIP: no hardware D3D11 device: {e}");
                return;
            }
        };
        let crop = CropRect {
            x: 16,
            y: 8,
            width: 32,
            height: 24,
        };
        let mut conv = match VideoConverter::new_with_crop(&device, 96, 64, 48, 36, Some(crop)) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("SKIP: video processor unavailable: {e}");
                return;
            }
        };
        let src = crate::windows::d3d11::create_bgra_texture(&device, 96, 64).expect("src");
        let nv12 = conv.convert(&src).expect("convert");
        let desc = crate::windows::d3d11::texture_desc(&nv12);
        assert_eq!((desc.Width, desc.Height), (48, 36));
        assert_eq!(
            desc.Format,
            windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_NV12
        );
    }
}
