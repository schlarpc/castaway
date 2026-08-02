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
//! Resources are opened per frame, not cached. There *was* a cache keyed on the shared
//! handle's value, on the stated premise that "the producer cycles a small ring of
//! textures, so the same handles recur". The premise was false about its own producer:
//! [`super::d3d11va::D3d11Exporter::export`] mints a fresh NT handle with
//! `CreateSharedHandle` on every frame and closes it when that frame is released. The ring
//! it cycles is textures, not handles.
//!
//! Windows reuses freed handle-table slots aggressively, so the values recurred anyway —
//! on a period unrelated to the four-slot texture ring. A "hit" therefore returned the
//! D3D12 resource belonging to *a different pool slot*: a texture holding an older frame,
//! or one the decode thread was writing into at that moment. That is what stale, juddering
//! mirroring looked like, and — because a rebuilt ring after a resolution change mints
//! handles whose values collide with the just-freed old ones — what "the aspect is right
//! but the picture inside it is stretched" looked like too. One cache, both symptoms.
//!
//! [`Self::import_single_plane`] had already reasoned this out correctly for the browser
//! path and declined to cache. Both paths mint per-frame handles; only one drew the right
//! conclusion. If `OpenSharedHandle` per frame ever shows up in a profile on the box, the
//! fix is a cache keyed on something the producer guarantees stable — a pool-slot
//! generation carried on the surface — never on the handle value.
//!
//! **Compile-checked, not hardware-verified** — see the note in [`super::d3d11va`].
#![allow(
    unsafe_code,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap,
    clippy::ptr_as_ptr
)]

use castaway_core::GpuSurface;
use wgpu::hal::api::Dx12;
use winapi::shared::winerror::S_OK;
use winapi::um::d3d12::ID3D12Resource;
use winapi::um::winnt::HANDLE;
use winapi::Interface as _;

use super::d3d11va::D3d11SharedSurface;
use crate::error::PipelineError;

/// Imports D3D11-produced shared NV12 textures into the compositor's D3D12 device.
///
/// Stateless: see the module note on why the handle-keyed cache that used to live here was
/// not a cache but an aliasing bug.
pub struct Dx12Importer;

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

        Ok((device, queue, Self))
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

        // SAFETY: the device is live and `handle` is an NT handle owned by the surface,
        // which outlives this call. Opened fresh each frame — the handle identifies *this*
        // frame's texture and nothing else (module note).
        let resource = unsafe { open_shared(device, surface.handle()) }?;

        let extent = wgpu::Extent3d {
            width: surface.width,
            height: surface.height,
            depth_or_array_layers: 1,
        };
        // SAFETY: `resource` was just opened on this device and is live.
        unsafe { check_extent(&resource, extent, wgpu::TextureFormat::NV12) }?;
        // SAFETY: the resource was opened on this device and is an NV12 2D texture with
        // one mip and one sample, matching the descriptor. The reference is *moved* into
        // the texture rather than cloned: with no cache there is no second owner, so
        // wgpu's release when the layer drops is the one that balances `OpenSharedHandle`.
        // The producer's surface is kept alive independently by the compositor's layer
        // (`LayerTexture::Nv12::_surface`), which is what keeps the handle valid.
        let hal_texture = unsafe {
            wgpu::hal::dx12::Device::texture_from_raw(
                resource,
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
    /// The sampleable texture. It owns the only reference to the shared resource: the
    /// resource is *moved* into `texture_from_raw`, so wgpu's release when the texture
    /// drops is the one that balances `OpenSharedHandle`.
    pub texture: wgpu::Texture,
    /// The duplicated NT handle. `OpenSharedHandle` does not need it kept open, but the
    /// frame's owner may carry other state, so it rides along and closes here.
    _owner: std::sync::Arc<dyn GpuSurface>,
}

impl ImportedFrame {
    /// Yield the texture, dropping everything this type was holding for it.
    ///
    /// Nothing is forgotten here, and both of the things that used to be are worth naming,
    /// because each cost a distinct failure on the panel.
    ///
    /// `_resource` was `std::mem::forget`-ed on the reasoning that "D3D12 resources are
    /// refcounted, and the reference is released when the texture wgpu owns is dropped".
    /// True of *a* reference, but there were two: `open_shared` returned one and the
    /// texture was built from a `clone()` of it. wgpu released the clone; the original was
    /// forgotten, so the resource was never freed. One leaked shared texture per browser
    /// paint at 60 Hz — the receiver ran for about three minutes on the Dell and then died
    /// in `Queue::write_buffer` with "Not enough memory left". The resource is now moved
    /// into the texture at import, so there is only ever one reference and this field is
    /// gone entirely.
    ///
    /// `_owner` was forgotten too. It is the borrow whose `Drop` acks the frame back to
    /// Chromium, so no frame was ever acked; the browser stops sending once
    /// `MAX_INFLIGHT_FRAMES` are unreleased, which would have frozen the page after four
    /// frames. It is dropped here because the caller keeps its own clone and hangs it on
    /// the layer for the texture's lifetime.
    #[must_use]
    pub fn into_texture(self) -> wgpu::Texture {
        let Self { texture, _owner } = self;
        drop(_owner);
        texture
    }
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
    /// delivers (D36/#64).
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
        // SAFETY: `resource` was just opened on this device and is live.
        unsafe { check_extent(&resource, extent, geometry.format) }?;
        // SAFETY: the resource was opened on this device and is a 2D texture with one mip
        // and one sample, matching the descriptor. Moved, not cloned: one reference in,
        // one release out, when wgpu drops the texture with its layer.
        let hal_texture = unsafe {
            wgpu::hal::dx12::Device::texture_from_raw(
                resource,
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
            _owner: owner,
        })
    }
}

/// Check that a resource really is the size and format we are about to tell wgpu it is.
///
/// This is the only place in the import path where the *real* dimensions are knowable.
/// `texture_from_raw` takes the extent on trust and does not consult the resource, so the
/// descriptor handed to wgpu is a claim, not a reading — and everything downstream
/// (`Texture::width()`, the compositor's own guard, the sampler's addressing) reports the
/// claim back. Comparing those to each other can only ever agree with itself, which is
/// what #103 was: a size guard that could not fire. `GetDesc` asks the resource.
///
/// A mismatch is a mis-import, not a rendering artefact to live with: a texture bound at
/// the wrong extent samples memory outside the picture, which on a 1088-row decoder
/// texture described as 1080 rows is the stale-frame class of bug that #102 is about at
/// the other end of the same handle.
///
/// # Safety
/// `resource` must be a live D3D12 resource.
unsafe fn check_extent(
    resource: &d3d12::Resource,
    claimed: wgpu::Extent3d,
    format: wgpu::TextureFormat,
) -> Result<(), PipelineError> {
    // SAFETY: `resource` is live; `GetDesc` returns by value and cannot fail.
    let desc = unsafe { resource.GetDesc() };

    if desc.Width != u64::from(claimed.width) || desc.Height != claimed.height {
        return Err(PipelineError::GpuImport(format!(
            "shared resource is {}×{} but was imported as {}×{}",
            desc.Width, desc.Height, claimed.width, claimed.height
        )));
    }
    // The format travels with the handle rather than being chosen here, so a producer
    // that changed it — an NV12 pool rebuilt as P010 for a 10-bit stream, say — would
    // otherwise be sampled through the wrong per-plane views and paint plausible garbage.
    let accepted = dxgi_formats(format);
    if !accepted.is_empty() && !accepted.contains(&desc.Format) {
        return Err(PipelineError::GpuImport(format!(
            "shared resource is DXGI format {} but was imported as {format:?}",
            desc.Format
        )));
    }
    Ok(())
}

/// Every DXGI format a `wgpu` format may legitimately arrive as, or empty for one this
/// path does not import — which is not an error here, only a check declined.
///
/// A family rather than a single value because the `_TYPELESS` and `_SRGB` members of an
/// 8-bit RGBA family differ only in how a *view* reads the same bits, and a producer
/// picks whichever suits it: Chromium creates its shared images typeless on some paths so
/// the same resource can be read both ways. Rejecting those would turn this guard into a
/// new way to lose the browser layer, which is not what it is for — it is for catching a
/// resource that is a different *thing* than we believe.
fn dxgi_formats(format: wgpu::TextureFormat) -> &'static [winapi::shared::dxgiformat::DXGI_FORMAT] {
    use winapi::shared::dxgiformat as fmt;
    match format {
        wgpu::TextureFormat::NV12 => &[fmt::DXGI_FORMAT_NV12],
        wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb => &[
            fmt::DXGI_FORMAT_B8G8R8A8_UNORM,
            fmt::DXGI_FORMAT_B8G8R8A8_UNORM_SRGB,
            fmt::DXGI_FORMAT_B8G8R8A8_TYPELESS,
        ],
        wgpu::TextureFormat::Rgba8Unorm | wgpu::TextureFormat::Rgba8UnormSrgb => &[
            fmt::DXGI_FORMAT_R8G8B8A8_UNORM,
            fmt::DXGI_FORMAT_R8G8B8A8_UNORM_SRGB,
            fmt::DXGI_FORMAT_R8G8B8A8_TYPELESS,
        ],
        _ => &[],
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
