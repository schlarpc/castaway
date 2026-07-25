//! VA-API → DMA-BUF: getting a decoded surface out of libavcodec without touching libva.
//!
//! `av_hwframe_map` to `AV_PIX_FMT_DRM_PRIME` is the whole trick. It hands back an
//! `AVDRMFrameDescriptor` — per-plane file descriptors plus a DRM format modifier —
//! which is exactly what Vulkan's DMA-BUF import wants, and it does it without linking
//! libva, calling `vaExportSurfaceHandle`, or knowing that VA-API exists beyond the
//! device type. That keeps the Linux backend to one libavutil call and a struct read.
//!
//! `AV_HWFRAME_MAP_READ` (not `DIRECT`) is deliberate: DIRECT asks for a mapping that
//! skips any driver-side copy, and when the driver cannot honour that it fails the whole
//! map rather than falling back. On a live mirror a failed map is a dropped frame, so the
//! permissive flag is the right one — and if a driver did insert a copy it would show up
//! as a frame-rate cliff the transient budget in [`super::FallbackPolicy`] would catch.
#![allow(
    unsafe_code,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap,
    clippy::ptr_as_ptr
)]

use std::sync::Arc;

use castaway_core::GpuSurface;
use ffmpeg_sys_next as sys;

use super::dmabuf::DmaBufSurface;
use super::{ffmpeg_hw, HwBackendKind, HwGiveUp};

/// Map a decoded VA-API frame to DMA-BUF planes.
///
/// The returned surface owns a reference to `frame`, which is what stops libavcodec
/// recycling the underlying VA surface into the next picture while the compositor is
/// still sampling it.
///
/// # Safety
/// `frame` must point at a decoded `AVFrame` whose format is the VA-API hardware format.
///
/// # Errors
/// [`HwGiveUp::ExportFailed`] if the map or the descriptor is not usable. This is the
/// transient case: one surface failing to export is a dropped frame, and only a run of
/// them is worth abandoning hardware over.
pub unsafe fn export(frame: *mut sys::AVFrame) -> Result<Arc<dyn GpuSurface>, HwGiveUp> {
    // SAFETY: caller guarantees a live decoded frame.
    let color = unsafe { ffmpeg_hw::color_of(frame) };

    // SAFETY: allocates a fresh, empty frame; null on OOM.
    let mapped = unsafe { sys::av_frame_alloc() };
    if mapped.is_null() {
        return Err(HwGiveUp::ExportFailed("av_frame_alloc failed".into()));
    }

    // SAFETY: `mapped` is a fresh frame we own; setting the target format before the map
    // is how `av_hwframe_map` is told what to produce.
    unsafe {
        (*mapped).format = sys::AVPixelFormat::AV_PIX_FMT_DRM_PRIME as i32;
    }

    // SAFETY: both frames are live; `av_hwframe_map` fills `mapped` on success and leaves
    // it empty on failure.
    let rc = unsafe { sys::av_hwframe_map(mapped, frame, sys::AV_HWFRAME_MAP_READ as i32) };
    if rc < 0 {
        // SAFETY: `mapped` is still an allocated frame we own and nothing else holds it.
        unsafe { free_frame(mapped) };
        return Err(HwGiveUp::ExportFailed(format!(
            "av_hwframe_map to DRM_PRIME failed ({})",
            ffmpeg_hw::av_error(rc)
        )));
    }

    // SAFETY: the map succeeded, so `mapped` is a DRM_PRIME frame with a descriptor in
    // `data[0]`. Ownership of the reference moves into the surface.
    match unsafe { DmaBufSurface::from_drm_frame(mapped, color) } {
        Ok(surface) => Ok(Arc::new(surface)),
        Err(e) => {
            // SAFETY: `from_drm_frame` only takes ownership on success, so the frame is
            // still ours to free here.
            unsafe { free_frame(mapped) };
            Err(HwGiveUp::ExportFailed(format!(
                "{}: {e}",
                HwBackendKind::Vaapi
            )))
        }
    }
}

/// # Safety
/// `frame` must be an allocated `AVFrame` that nothing else owns.
unsafe fn free_frame(mut frame: *mut sys::AVFrame) {
    // SAFETY: caller guarantees sole ownership of an allocated frame.
    unsafe { sys::av_frame_free(&raw mut frame) };
}
