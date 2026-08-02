use windows::Win32::Foundation::{E_FAIL, HMODULE};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D10::ID3D10Multithread;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_BIND_DECODER,
    D3D11_BIND_SHADER_RESOURCE, D3D11_BOX, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Media::MediaFoundation::{IMFDXGIDeviceManager, MFCreateDXGIDeviceManager};
use windows_core::{Error, Interface, Result};

pub(crate) struct PlaybackD3D11Device {
    pub(crate) device: ID3D11Device,
    pub(crate) context: ID3D11DeviceContext,
    pub(crate) manager: IMFDXGIDeviceManager,
    reset_token: u32,
    adapter_luid: Option<u64>,
}

impl PlaybackD3D11Device {
    pub(crate) fn hardware() -> Result<Self> {
        let mut device = None;
        let mut context = None;
        // SAFETY: out-pointers are valid, the hardware driver type requires no
        // adapter or software module, and feature-level selection is optional.
        unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )?;
        }
        let device = device.ok_or_else(|| Error::new(E_FAIL, "D3D11 returned no device"))?;
        let context = context.ok_or_else(|| Error::new(E_FAIL, "D3D11 returned no context"))?;
        ensure_multithread_protected(&device)?;

        let mut reset_token = 0;
        let mut manager = None;
        // SAFETY: out-pointers are valid and Media Foundation is initialized
        // by the owning decoder before the D3D device is constructed.
        unsafe { MFCreateDXGIDeviceManager(&mut reset_token, &mut manager)? };
        let manager = manager
            .ok_or_else(|| Error::new(E_FAIL, "Media Foundation returned no DXGI manager"))?;
        // SAFETY: binds the live D3D11 device using the manager's reset token.
        unsafe { manager.ResetDevice(&device, reset_token)? };

        let adapter_luid = adapter_luid(&device).ok();
        Ok(Self {
            device,
            context,
            manager,
            reset_token,
            adapter_luid,
        })
    }

    pub(crate) fn adapter_luid(&self) -> Option<u64> {
        self.adapter_luid
    }

    pub(crate) fn reset_manager(&self) -> Result<()> {
        // SAFETY: the token and device belong to this manager instance.
        unsafe { self.manager.ResetDevice(&self.device, self.reset_token) }
    }

    pub(crate) fn create_nv12_texture(&self, width: u32, height: u32) -> Result<ID3D11Texture2D> {
        if width == 0 || height == 0 {
            return Err(Error::new(
                E_FAIL,
                "NV12 texture dimensions must be non-zero",
            ));
        }
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut texture = None;
        // SAFETY: descriptor is initialized and the out-pointer is valid.
        unsafe {
            self.device
                .CreateTexture2D(&desc, None, Some(&mut texture))?
        };
        texture.ok_or_else(|| Error::new(E_FAIL, "D3D11 returned no NV12 texture"))
    }

    pub(crate) fn create_decoder_output_nv12_texture(
        &self,
        width: u32,
        height: u32,
    ) -> Result<ID3D11Texture2D> {
        if width == 0 || height == 0 {
            return Err(Error::new(
                E_FAIL,
                "decoder NV12 texture dimensions must be non-zero",
            ));
        }
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_DECODER.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut texture = None;
        // SAFETY: descriptor is initialized and the out-pointer is valid.
        unsafe {
            self.device
                .CreateTexture2D(&desc, None, Some(&mut texture))?
        };
        texture.ok_or_else(|| Error::new(E_FAIL, "D3D11 returned no decoder NV12 texture"))
    }

    pub(crate) fn copy_texture(
        &self,
        destination: &ID3D11Texture2D,
        source: &ID3D11Texture2D,
        source_subresource: u32,
        width: u32,
        height: u32,
    ) -> Result<()> {
        self.check_removed()?;
        let source_box = D3D11_BOX {
            left: 0,
            top: 0,
            front: 0,
            right: width,
            bottom: height,
            back: 1,
        };
        // SAFETY: both resources are live textures on the playback device;
        // negotiated dimensions and NV12 subtype are validated before copy.
        unsafe {
            self.context.CopySubresourceRegion(
                destination,
                0,
                0,
                0,
                0,
                source,
                source_subresource,
                Some(&source_box),
            );
        }
        self.check_removed()
    }

    pub(crate) fn upload_nv12(
        &self,
        destination: &ID3D11Texture2D,
        packed_nv12: &[u8],
        width: u32,
        height: u32,
    ) -> Result<()> {
        let expected = usize::try_from(width)
            .ok()
            .and_then(|width| {
                usize::try_from(height)
                    .ok()
                    .and_then(|height| width.checked_mul(height))
            })
            .and_then(|luma| luma.checked_add(luma / 2))
            .ok_or_else(|| Error::new(E_FAIL, "NV12 upload size overflow"))?;
        if packed_nv12.len() != expected {
            return Err(Error::new(
                E_FAIL,
                format!(
                    "NV12 upload has {} bytes, expected {expected}",
                    packed_nv12.len()
                ),
            ));
        }
        self.check_removed()?;
        // SAFETY: destination is a matching default-usage NV12 texture and
        // the source slice is exact-size packed NV12 with `width` row pitch.
        unsafe {
            self.context.UpdateSubresource(
                destination,
                0,
                None,
                packed_nv12.as_ptr().cast(),
                width,
                0,
            );
        }
        self.check_removed()
    }

    pub(crate) fn check_removed(&self) -> Result<()> {
        // SAFETY: a status query on a live D3D11 device.
        unsafe { self.device.GetDeviceRemovedReason() }
    }
}

pub(crate) fn ensure_multithread_protected(device: &ID3D11Device) -> Result<()> {
    let multithread: ID3D10Multithread = device.cast()?;
    // SAFETY: these accessors operate on a live device interface. Query after
    // setting so drivers cannot silently leave protection disabled.
    if !unsafe { multithread.GetMultithreadProtected() }.as_bool() {
        let _ = unsafe { multithread.SetMultithreadProtected(true) };
    }
    if !unsafe { multithread.GetMultithreadProtected() }.as_bool() {
        return Err(Error::new(
            E_FAIL,
            "D3D11 multithread protection could not be enabled",
        ));
    }
    Ok(())
}

pub(crate) fn adapter_luid(device: &ID3D11Device) -> Result<u64> {
    let dxgi_device: IDXGIDevice = device.cast()?;
    // SAFETY: both queries return owned COM wrappers/values.
    let adapter = unsafe { dxgi_device.GetAdapter()? };
    let desc = unsafe { adapter.GetDesc()? };
    Ok(((desc.AdapterLuid.HighPart as u32 as u64) << 32) | u64::from(desc.AdapterLuid.LowPart))
}
