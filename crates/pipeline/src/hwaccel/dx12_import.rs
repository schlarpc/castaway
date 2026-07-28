//! Opening a D3D11-produced shared texture on the compositor's D3D12 device.
//!
//! The Windows counterpart of [`super::vulkan_import`], and much shorter, because D3D12
//! needs no special device setup to accept a shared handle — no extension list, no
//! interop-specific device creation. `ID3D12Device::OpenSharedHandle` on an NT handle is
//! the whole import.
//!
//! What it does need is [`wgpu::Features::TEXTURE_FORMAT_NV12`], for the same reason
//! Linux does: without per-plane views there is no way to sample the surface.
//!
//! Opened resources are cached by handle value. The producer cycles a small ring of
//! textures ([`super::d3d11va::POOL_DEPTH`] of them), so the same handles recur and the
//! cache converges to that ring after the first few frames rather than re-opening on
//! every one.
//!
//! **Compile-checked, not hardware-verified** — see the note in [`super::d3d11va`].
#![allow(
    unsafe_code,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap,
    clippy::ptr_as_ptr
)]

use std::collections::HashMap;

use castaway_core::GpuSurface;
use wgpu::hal::api::Dx12;
use winapi::shared::winerror::S_OK;
use winapi::um::d3d12::ID3D12Resource;
use winapi::um::winnt::HANDLE;
use winapi::Interface as _;

use super::d3d11va::D3d11SharedSurface;
use crate::error::PipelineError;

/// A resource opened on the D3D12 device, kept alive for reuse.
struct OpenedResource(d3d12::Resource);

// SAFETY: the importer lives on the render thread and is never shared; the COM pointer is
// only touched there. `Send` is claimed so the importer can be moved into the compositor
// at construction.
unsafe impl Send for OpenedResource {}

/// Imports D3D11-produced shared NV12 textures into the compositor's D3D12 device.
pub struct Dx12Importer {
    /// Cache from shared-handle value to the resource opened on our device.
    opened: HashMap<usize, OpenedResource>,
}

impl Dx12Importer {
    /// Open a DX12 device with NV12 sampling, and an importer for it.
    ///
    /// Unlike Vulkan there is nothing to add to device creation — a D3D12 device can open
    /// a shared handle from any device on the same adapter — so this is the ordinary
    /// `request_device` with one feature added.
    ///
    /// # Errors
    /// [`PipelineError::GpuInit`] if the adapter is not DX12 or lacks NV12 support.
    pub fn open_device(
        adapter: &wgpu::Adapter,
        limits: wgpu::Limits,
    ) -> Result<(wgpu::Device, wgpu::Queue, Self), PipelineError> {
        let info = adapter.get_info();
        if info.backend != wgpu::Backend::Dx12 {
            return Err(PipelineError::GpuInit(format!(
                "shared-texture import needs the DX12 backend, this adapter is {:?}",
                info.backend
            )));
        }
        if !adapter
            .features()
            .contains(wgpu::Features::TEXTURE_FORMAT_NV12)
        {
            return Err(PipelineError::GpuInit(
                "adapter does not support TEXTURE_FORMAT_NV12".into(),
            ));
        }

        let (device, queue) = pollster::block_on(adapter.request_device(
            &wgpu::DeviceDescriptor {
                label: Some("castaway-compositor-interop"),
                required_features: wgpu::Features::TEXTURE_FORMAT_NV12,
                required_limits: limits,
                memory_hints: wgpu::MemoryHints::Performance,
            },
            None,
        ))
        .map_err(|e| PipelineError::GpuInit(format!("request_device (interop): {e}")))?;

        Ok((
            device,
            queue,
            Self {
                opened: HashMap::new(),
            },
        ))
    }

    /// Import one shared surface as an NV12 `wgpu::Texture`.
    ///
    /// # Errors
    /// [`PipelineError::GpuImport`] if the surface is not a D3D11 shared one, or the
    /// device refuses the handle.
    pub fn import(
        &mut self,
        device: &wgpu::Device,
        surface: &std::sync::Arc<dyn GpuSurface>,
    ) -> Result<wgpu::Texture, PipelineError> {
        let surface: &D3d11SharedSurface = surface.as_any().downcast_ref().ok_or_else(|| {
            PipelineError::GpuImport("surface is not a D3D11 shared texture".into())
        })?;

        let key = surface.handle() as usize;
        if !self.opened.contains_key(&key) {
            // SAFETY: the device is live and `handle` is an NT handle owned by the
            // surface, which outlives this call.
            let resource = unsafe { open_shared(device, surface.handle()) }?;
            self.opened.insert(key, OpenedResource(resource));
        }
        let resource = self
            .opened
            .get(&key)
            .ok_or_else(|| PipelineError::GpuImport("shared resource vanished".into()))?;

        let extent = wgpu::Extent3d {
            width: surface.width,
            height: surface.height,
            depth_or_array_layers: 1,
        };
        // SAFETY: the resource was opened on this device and is an NV12 2D texture with
        // one mip and one sample, matching the descriptor. No drop guard is passed
        // because the cache owns the resource; wgpu must not release it.
        let hal_texture = unsafe {
            wgpu::hal::dx12::Device::texture_from_raw(
                resource.0.clone(),
                wgpu::TextureFormat::NV12,
                wgpu::TextureDimension::D2,
                extent,
                1,
                1,
            )
        };

        // SAFETY: built from this device's own resource, matching the descriptor.
        Ok(unsafe {
            device.create_texture_from_hal::<Dx12>(
                hal_texture,
                &wgpu::TextureDescriptor {
                    label: Some("imported-nv12"),
                    size: extent,
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: wgpu::TextureFormat::NV12,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING,
                    view_formats: &[],
                },
            )
        })
    }
}

/// One imported browser frame: the texture, plus everything that must outlive it.
///
/// This type exists because of an asymmetry in wgpu rather than in Windows.
/// `wgpu::hal::vulkan::Device::texture_from_raw` accepts a drop guard, so the Linux path
/// can hand ownership to wgpu and let its resource tracking decide when the frame is
/// retired. The DX12 entry point takes no guard, so there is nowhere to hang it — which
/// means the lifetime has to be visible to the caller instead of hidden.
///
/// Holding this *is* the frame's lifetime. `OpenSharedHandle` aliases the browser's own
/// buffer rather than copying it, so dropping this before the GPU has finished sampling
/// lets Chromium recycle the pixels out from under a frame still on screen — the same
/// tearing bug [`super::dmabuf`] documents for VA-API surfaces, with the same shape and
/// the same "only under load, only on some drivers" symptom. Drop it, then ack release
/// to the browser; not the other way round.
pub struct ImportedFrame {
    /// The sampleable texture.
    pub texture: wgpu::Texture,
    /// Our reference to the shared resource. Released when this drops.
    _resource: d3d12::Resource,
    /// The duplicated NT handle. `OpenSharedHandle` does not need it kept open, but the
    /// frame's owner may carry other state, so it rides along and closes here.
    _owner: std::sync::Arc<dyn GpuSurface>,
}

impl std::fmt::Debug for ImportedFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImportedFrame")
            .field("size", &self.texture.size())
            .field("format", &self.texture.format())
            .finish_non_exhaustive()
    }
}

impl Dx12Importer {
    /// Import a single-plane BGRA/RGBA shared texture — the shape an offscreen browser
    /// delivers (D36/Q40).
    ///
    /// The Windows counterpart of
    /// [`super::vulkan_import::VulkanImporter::import_single_plane`], and shorter for the
    /// same reason the NV12 pair is: `OpenSharedHandle` needs no layout description, so
    /// there is no Windows equivalent of the DRM-modifier trap that makes a wrong import
    /// render plausible garbage. Whatever the handle describes — tiling, swizzle — is
    /// carried inside it.
    ///
    /// Deliberately **not** cached, where the decoder path's [`Self::import`] caches by
    /// handle value. That cache works because the decoder's own pool hands back a stable
    /// set of handles; here the handle is a duplicate we minted this frame, so its value
    /// is fresh every time and a cache keyed on it would never hit while growing without
    /// bound. Opening per frame is the correct starting point; if the box shows it costs
    /// real time, the fix is to key a cache on the *browser-side* handle value, which
    /// means carrying it separately rather than inferring it from ours.
    ///
    /// **Compile-checked, not hardware-verified** — see the note in [`super::d3d11va`].
    /// Note this one has never had a GPU under it *and* has never had a browser over it,
    /// so a first run on the box is testing two unproven layers at once; prove the
    /// decoder bridge first so a failure names one of them.
    ///
    /// # Errors
    /// [`PipelineError::GpuImport`] if the format is not BGRA8/RGBA8 or the device
    /// refuses the handle.
    pub fn import_single_plane(
        device: &wgpu::Device,
        geometry: super::FrameGeometry,
        handle: HANDLE,
        owner: std::sync::Arc<dyn GpuSurface>,
    ) -> Result<ImportedFrame, PipelineError> {
        let geometry = geometry.validate()?;

        // SAFETY: the device is live, and `handle` is an NT handle owned by `owner`,
        // which outlives this call and is moved into the returned frame.
        let resource = unsafe { open_shared(device, handle) }?;

        let extent = geometry.extent();
        // SAFETY: the resource was opened on this device and is a 2D texture with one mip
        // and one sample, matching the descriptor. No drop guard is passed because
        // `ImportedFrame` owns the resource; wgpu must not release it.
        let hal_texture = unsafe {
            wgpu::hal::dx12::Device::texture_from_raw(
                resource.clone(),
                geometry.format,
                wgpu::TextureDimension::D2,
                extent,
                1,
                1,
            )
        };
        // SAFETY: built from this device's own resource, matching the descriptor below.
        let texture = unsafe {
            device.create_texture_from_hal::<Dx12>(
                hal_texture,
                &wgpu::TextureDescriptor {
                    label: Some("imported-browser-frame"),
                    size: extent,
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: geometry.format,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING,
                    view_formats: &[],
                },
            )
        };

        Ok(ImportedFrame {
            texture,
            _resource: resource,
            _owner: owner,
        })
    }
}

/// `ID3D12Device::OpenSharedHandle` on the wgpu device's underlying D3D12 device.
///
/// # Safety
/// `handle` must be a live NT handle produced by `IDXGIResource1::CreateSharedHandle`.
unsafe fn open_shared(
    device: &wgpu::Device,
    handle: HANDLE,
) -> Result<d3d12::Resource, PipelineError> {
    // SAFETY: `as_hal` lends the backend device for the closure's duration; nothing
    // escapes it but the opened resource, which is independent of the borrow.
    let opened = unsafe {
        device.as_hal::<Dx12, _, _>(|d| {
            let raw = d?.raw_device();
            let mut resource: *mut ID3D12Resource = std::ptr::null_mut();
            // SAFETY: `raw` is a live ID3D12Device; the pointer is written on success.
            let hr = unsafe {
                raw.OpenSharedHandle(
                    handle,
                    &ID3D12Resource::uuidof(),
                    (&raw mut resource).cast(),
                )
            };
            if hr != S_OK || resource.is_null() {
                return Some(Err(PipelineError::GpuImport(format!(
                    "OpenSharedHandle failed ({hr:#010x})"
                ))));
            }
            // SAFETY: `OpenSharedHandle` returned an owned reference; `from_raw` AddRefs,
            // so the extra reference is released immediately after.
            let wrapped = unsafe {
                let wrapped = d3d12::Resource::from_raw(resource.cast());
                (*resource).Release();
                wrapped
            };
            Some(Ok(wrapped))
        })
    };
    // `Device::as_hal` wraps the callback's own result in an `Option` for the non-core
    // backends, so there are two layers to unwrap: "no DX12 backend" and "the open failed".
    opened
        .flatten()
        .ok_or_else(|| PipelineError::GpuImport("device has no DX12 backend".into()))?
}
