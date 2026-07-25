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
    opened.ok_or_else(|| PipelineError::GpuImport("device has no DX12 backend".into()))?
}
