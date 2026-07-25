//! D3D11VA → a shared NV12 texture the D3D12 compositor can open.
//!
//! Windows costs one GPU-local copy that Linux does not, and it is not avoidable.
//! libavcodec allocates its decoder output as a `ID3D11Texture2D` *array* with
//! `D3D11_BIND_DECODER`, and a resource created with that bind flag is generally not
//! shareable — you cannot hand it a `D3D11_RESOURCE_MISC_SHARED_NTHANDLE`. So the decoded
//! subresource is copied into a texture we own and *did* create shareable, and that one
//! is handed across.
//!
//! It is still worth doing: `CopySubresourceRegion` between two textures on the same
//! adapter is a GPU-local blit at memory bandwidth, where the alternative
//! (`av_hwframe_transfer_data` to system memory and back up through `write_texture`) is a
//! round trip across PCIe in both directions. At 4K the readback is the thing that eats
//! the frame budget.
//!
//! ## Synchronisation
//!
//! The copy is issued on ffmpeg's D3D11 immediate context; the D3D12 device that opens
//! the handle is a different device. Rather than plumb a shared fence — `ID3D11Fence`
//! needs D3D11.4 interfaces that are not in the `winapi` bindings this tree already
//! links — the producer waits for its own copy to retire using a `D3D11_QUERY_EVENT`
//! before publishing the surface. That is a short block on the decode thread, which has
//! slack, and it makes "the handle is visible" imply "the pixels are there" with no
//! cross-device ordering left to reason about.
//!
//! **This path is compile-checked, not hardware-verified.** Everything above the copy —
//! the fallback policy, the colorimetry, the NV12 sampling — is exercised on Linux; the
//! D3D11/D3D12 bridge itself needs the deploy box.
#![allow(
    unsafe_code,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap,
    clippy::ptr_as_ptr
)]

use std::sync::Arc;

use castaway_core::{ColorInfo, GpuSurface};
use ffmpeg_sys_next as sys;
use winapi::shared::dxgi::IDXGIResource;
use winapi::shared::dxgi1_2::IDXGIResource1;
use winapi::shared::dxgiformat::DXGI_FORMAT_NV12;
use winapi::shared::winerror::S_OK;
use winapi::um::d3d11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11Query, ID3D11Texture2D, D3D11_BIND_SHADER_RESOURCE,
    D3D11_QUERY_DESC, D3D11_QUERY_EVENT, D3D11_RESOURCE_MISC_SHARED,
    D3D11_RESOURCE_MISC_SHARED_NTHANDLE, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
};
use winapi::um::handleapi::CloseHandle;
use winapi::um::winnt::{GENERIC_ALL, HANDLE};
use winapi::Interface as _;

use super::{ffmpeg_hw, HwGiveUp};

/// How many shared textures to cycle through.
///
/// One is not enough: the compositor may still be sampling the previous frame's texture
/// when the next decode completes. Four covers wgpu's default two frames of latency plus
/// the one being written, with a spare.
const POOL_DEPTH: usize = 4;

/// A shared NV12 texture, published to the render thread as an NT handle.
#[derive(Debug)]
pub struct D3d11SharedSurface {
    /// Picture width in pixels.
    pub width: u32,
    /// Picture height in pixels.
    pub height: u32,
    /// The NT handle the D3D12 device opens. Owned: closed when this surface drops.
    handle: SharedHandle,
    color: ColorInfo,
    /// Keeps the pool slot reserved until the compositor is done with this frame.
    _slot: Arc<PoolSlot>,
}

impl D3d11SharedSurface {
    /// The NT handle to open on the consuming device.
    #[must_use]
    pub fn handle(&self) -> HANDLE {
        self.handle.0
    }
}

impl GpuSurface for D3d11SharedSurface {
    fn color(&self) -> ColorInfo {
        self.color
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// An owned NT handle.
#[derive(Debug)]
struct SharedHandle(HANDLE);

// SAFETY: an NT handle is a process-wide token, not thread-affine; the only operation
// this wrapper performs is `CloseHandle` on drop.
unsafe impl Send for SharedHandle {}
// SAFETY: as above — `&SharedHandle` exposes only the numeric handle, which is valid to
// read from any thread.
unsafe impl Sync for SharedHandle {}

impl Drop for SharedHandle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: the handle came from `CreateSharedHandle` and is closed once.
            unsafe { CloseHandle(self.0) };
        }
    }
}

/// One texture in the ring. Its `Arc` strong count is what marks the slot busy: while a
/// published surface still exists, the slot is not reused.
#[derive(Debug)]
struct PoolSlot {
    texture: TexturePtr,
}

#[derive(Debug)]
struct TexturePtr(*mut ID3D11Texture2D);

// SAFETY: the pointer is only dereferenced on the decode thread that owns the pool; the
// `Drop` below releases it once. Publishing an `Arc<PoolSlot>` to the render thread hands
// over no way to touch the texture, only to keep the slot reserved.
unsafe impl Send for TexturePtr {}
// SAFETY: as above.
unsafe impl Sync for TexturePtr {}

impl Drop for TexturePtr {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: the texture was created by us and released exactly once.
            unsafe { (*self.0).Release() };
        }
    }
}

/// Copies decoded D3D11 surfaces into shareable textures.
pub struct D3d11Exporter {
    device: *mut ID3D11Device,
    context: *mut ID3D11DeviceContext,
    /// ffmpeg's own mutex around the shared immediate context.
    lock: Option<unsafe extern "C" fn(*mut std::ffi::c_void)>,
    unlock: Option<unsafe extern "C" fn(*mut std::ffi::c_void)>,
    lock_ctx: *mut std::ffi::c_void,
    pool: Vec<Arc<PoolSlot>>,
    size: (u32, u32),
}

// SAFETY: every pointer here belongs to the single decode thread that constructed the
// exporter; nothing is shared, only moved with the exporter itself.
unsafe impl Send for D3d11Exporter {}

impl Drop for D3d11Exporter {
    fn drop(&mut self) {
        // SAFETY: both interfaces were AddRef'd when the exporter was built.
        unsafe {
            if !self.context.is_null() {
                (*self.context).Release();
            }
            if !self.device.is_null() {
                (*self.device).Release();
            }
        }
    }
}

impl D3d11Exporter {
    /// Build an exporter from libavutil's D3D11VA device context.
    ///
    /// # Safety
    /// `device_ref` must be an `AVBufferRef` holding an `AVHWDeviceContext` of type
    /// `AV_HWDEVICE_TYPE_D3D11VA`.
    ///
    /// # Errors
    /// [`HwGiveUp::DeviceUnavailable`] if the context does not carry a D3D11 device.
    pub unsafe fn new(device_ref: *mut sys::AVBufferRef) -> Result<Self, HwGiveUp> {
        // SAFETY: caller guarantees the buffer holds an AVHWDeviceContext whose `hwctx`
        // is an AVD3D11VADeviceContext.
        let hwctx = unsafe {
            let ctx = (*device_ref).data.cast::<sys::AVHWDeviceContext>();
            if ctx.is_null() {
                return Err(HwGiveUp::DeviceUnavailable("null hw device context".into()));
            }
            (*ctx).hwctx.cast::<sys::AVD3D11VADeviceContext>()
        };
        if hwctx.is_null() {
            return Err(HwGiveUp::DeviceUnavailable("null D3D11VA hwctx".into()));
        }
        // SAFETY: non-null and fully initialized by `av_hwdevice_ctx_create`.
        let hwctx = unsafe { &*hwctx };
        let device = hwctx.device.cast::<ID3D11Device>();
        let context = hwctx.device_context.cast::<ID3D11DeviceContext>();
        if device.is_null() || context.is_null() {
            return Err(HwGiveUp::DeviceUnavailable(
                "D3D11VA context has no device".into(),
            ));
        }
        // SAFETY: both are live COM interfaces owned by libavutil; we take our own
        // references so they outlive any reordering of teardown.
        unsafe {
            (*device).AddRef();
            (*context).AddRef();
        }

        Ok(Self {
            device,
            context,
            lock: hwctx.lock,
            unlock: hwctx.unlock,
            lock_ctx: hwctx.lock_ctx,
            pool: Vec::new(),
            size: (0, 0),
        })
    }

    /// Copy a decoded frame into a shareable texture and publish it.
    ///
    /// # Safety
    /// `frame` must be a decoded `AVFrame` in `AV_PIX_FMT_D3D11`.
    ///
    /// # Errors
    /// [`HwGiveUp::ExportFailed`] if the pool is exhausted or any D3D call fails.
    pub unsafe fn export(
        &mut self,
        frame: *mut sys::AVFrame,
    ) -> Result<Arc<dyn GpuSurface>, HwGiveUp> {
        // SAFETY: caller guarantees a decoded D3D11 frame: `data[0]` is the texture array
        // and `data[1]` the subresource index within it.
        let (source, index, width, height) = unsafe {
            let f = &*frame;
            (
                f.data[0].cast::<ID3D11Texture2D>(),
                f.data[1] as usize as u32,
                u32::try_from((*frame).width).unwrap_or(0),
                u32::try_from((*frame).height).unwrap_or(0),
            )
        };
        if source.is_null() || width == 0 || height == 0 {
            return Err(HwGiveUp::ExportFailed(
                "decoded frame has no texture".into(),
            ));
        }
        // SAFETY: caller guarantees a live decoded frame.
        let color = unsafe { ffmpeg_hw::color_of(frame) };

        if self.size != (width, height) {
            // A sender that changes resolution mid-mirror invalidates every pooled
            // texture; rebuilding is rare enough to be the simple path.
            self.pool.clear();
            self.size = (width, height);
        }
        // SAFETY: `self.device` is live for the exporter's lifetime.
        let slot = unsafe { self.acquire_slot(width, height) }?;

        // SAFETY: the immediate context is shared with libavcodec's decode calls, so
        // ffmpeg's own mutex must be held across our use of it.
        unsafe {
            self.with_lock(|this| {
                (*this.context).CopySubresourceRegion(
                    slot.texture.0.cast(),
                    0,
                    0,
                    0,
                    0,
                    source.cast(),
                    index,
                    std::ptr::null(),
                );
                this.wait_for_gpu()
            })
        }?;

        // SAFETY: `slot.texture` is a live texture created with the shared misc flags.
        let handle = unsafe { share(slot.texture.0) }?;

        Ok(Arc::new(D3d11SharedSurface {
            width,
            height,
            handle,
            color,
            _slot: slot,
        }))
    }

    /// Run `body` with ffmpeg's device-context mutex held.
    ///
    /// # Safety
    /// `body` may use `self.context`; nothing else may hold the lock.
    unsafe fn with_lock<R>(&mut self, body: impl FnOnce(&mut Self) -> R) -> R {
        if let Some(lock) = self.lock {
            // SAFETY: the callback and its context come from libavutil and are paired
            // with `unlock` below.
            unsafe { lock(self.lock_ctx) };
        }
        let out = body(self);
        if let Some(unlock) = self.unlock {
            // SAFETY: pairs with the `lock` above.
            unsafe { unlock(self.lock_ctx) };
        }
        out
    }

    /// Block until the immediate context's queued work has retired.
    ///
    /// # Safety
    /// Must be called with the device-context lock held.
    unsafe fn wait_for_gpu(&mut self) -> Result<(), HwGiveUp> {
        let desc = D3D11_QUERY_DESC {
            Query: D3D11_QUERY_EVENT,
            MiscFlags: 0,
        };
        let mut query: *mut ID3D11Query = std::ptr::null_mut();
        // SAFETY: `device` is live; the query is written on success only.
        let hr = unsafe { (*self.device).CreateQuery(&desc, &raw mut query) };
        if hr != S_OK || query.is_null() {
            return Err(HwGiveUp::ExportFailed(format!(
                "CreateQuery(EVENT) failed ({hr:#010x})"
            )));
        }
        // SAFETY: `query` is live until released below; `End` marks the fence point after
        // the copy, and `GetData` polls it.
        unsafe {
            (*self.context).End(query.cast());
            loop {
                let hr = (*self.context).GetData(query.cast(), std::ptr::null_mut(), 0, 0);
                if hr == S_OK {
                    break;
                }
                if hr < 0 {
                    (*query).Release();
                    return Err(HwGiveUp::ExportFailed(format!(
                        "query GetData failed ({hr:#010x})"
                    )));
                }
                std::hint::spin_loop();
            }
            (*query).Release();
        }
        Ok(())
    }

    /// Take a free pool slot, growing the pool up to [`POOL_DEPTH`].
    ///
    /// A slot is free when nothing but the pool holds its `Arc` — i.e. the compositor has
    /// dropped the surface that referenced it.
    ///
    /// # Safety
    /// `self.device` must be live.
    unsafe fn acquire_slot(&mut self, width: u32, height: u32) -> Result<Arc<PoolSlot>, HwGiveUp> {
        if let Some(free) = self
            .pool
            .iter()
            .find(|slot| Arc::strong_count(slot) == 1)
            .cloned()
        {
            return Ok(free);
        }
        if self.pool.len() >= POOL_DEPTH {
            // Every texture is still referenced downstream. Dropping the frame is the
            // right answer for a live mirror, and the transient budget upstream decides
            // whether a run of these means hardware decode is not keeping up.
            return Err(HwGiveUp::ExportFailed(
                "all shared textures are still in use".into(),
            ));
        }
        // SAFETY: caller guarantees a live device.
        let texture = unsafe { create_shared_nv12(self.device, width, height) }?;
        let slot = Arc::new(PoolSlot {
            texture: TexturePtr(texture),
        });
        self.pool.push(Arc::clone(&slot));
        Ok(slot)
    }
}

/// Create an NV12 texture that can be opened by another device.
///
/// # Safety
/// `device` must be a live `ID3D11Device`.
unsafe fn create_shared_nv12(
    device: *mut ID3D11Device,
    width: u32,
    height: u32,
) -> Result<*mut ID3D11Texture2D, HwGiveUp> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_NV12,
        SampleDesc: winapi::shared::dxgitype::DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_SHADER_RESOURCE,
        CPUAccessFlags: 0,
        // NTHANDLE is what makes `CreateSharedHandle` (rather than the legacy
        // `GetSharedHandle`) usable, and an NT handle is what `ID3D12Device::
        // OpenSharedHandle` accepts.
        MiscFlags: D3D11_RESOURCE_MISC_SHARED_NTHANDLE | D3D11_RESOURCE_MISC_SHARED,
    };
    let mut texture: *mut ID3D11Texture2D = std::ptr::null_mut();
    // SAFETY: `device` is live; the texture is written on success only.
    let hr = unsafe { (*device).CreateTexture2D(&desc, std::ptr::null(), &raw mut texture) };
    if hr != S_OK || texture.is_null() {
        return Err(HwGiveUp::ExportFailed(format!(
            "CreateTexture2D(NV12 {width}×{height}, shared) failed ({hr:#010x})"
        )));
    }
    Ok(texture)
}

/// Produce an NT handle for a shareable texture.
///
/// # Safety
/// `texture` must be a live texture created with the shared misc flags.
unsafe fn share(texture: *mut ID3D11Texture2D) -> Result<SharedHandle, HwGiveUp> {
    let mut resource: *mut IDXGIResource1 = std::ptr::null_mut();
    // SAFETY: `texture` is live; `QueryInterface` writes the pointer on success.
    let hr = unsafe {
        (*texture.cast::<IDXGIResource>())
            .QueryInterface(&IDXGIResource1::uuidof(), (&raw mut resource).cast())
    };
    if hr != S_OK || resource.is_null() {
        return Err(HwGiveUp::ExportFailed(format!(
            "QueryInterface(IDXGIResource1) failed ({hr:#010x})"
        )));
    }
    let mut handle: HANDLE = std::ptr::null_mut();
    // SAFETY: `resource` is live and released below regardless of outcome.
    let hr = unsafe {
        let hr = (*resource).CreateSharedHandle(
            std::ptr::null(),
            GENERIC_ALL,
            std::ptr::null(),
            &raw mut handle,
        );
        (*resource).Release();
        hr
    };
    if hr != S_OK || handle.is_null() {
        return Err(HwGiveUp::ExportFailed(format!(
            "CreateSharedHandle failed ({hr:#010x})"
        )));
    }
    Ok(SharedHandle(handle))
}
