//! One name for "turn this decoded hardware frame into a portable GPU surface", so the
//! decode loop has no `cfg` in it.
//!
//! The two platforms differ in more than the call: Linux is stateless (map the frame,
//! read the descriptor, done) while Windows owns a ring of shareable textures to copy
//! into. Hiding that behind a single stateful exporter keeps the difference here instead
//! of spreading it through [`crate::ffmpeg_decode`].
#![allow(unsafe_code)]

use std::sync::Arc;

use castaway_core::GpuSurface;
use ffmpeg_sys_next as sys;

use super::ffmpeg_hw::HwDevice;
use super::HwGiveUp;

/// Exports decoded hardware frames as [`GpuSurface`]s the compositor can import.
pub struct SurfaceExporter {
    #[cfg(windows)]
    inner: super::d3d11va::D3d11Exporter,
}

impl SurfaceExporter {
    /// Build the exporter for an open hardware device.
    ///
    /// # Errors
    /// [`HwGiveUp`] if the platform needs setup that failed — on Windows, reaching the
    /// D3D11 device inside libavutil's context.
    #[allow(unused_variables)]
    pub fn new(device: &HwDevice) -> Result<Self, HwGiveUp> {
        #[cfg(windows)]
        {
            // SAFETY: `device` holds a live D3D11VA `AVHWDeviceContext`, which is what
            // `D3d11Exporter::new` requires.
            let inner = unsafe { super::d3d11va::D3d11Exporter::new(device.raw()) }?;
            Ok(Self { inner })
        }
        #[cfg(unix)]
        {
            // VA-API needs nothing kept between frames: `av_hwframe_map` works straight
            // off the decoded frame.
            Ok(Self {})
        }
        #[cfg(not(any(unix, windows)))]
        {
            Err(HwGiveUp::NotCompiled)
        }
    }

    /// Export one decoded frame.
    ///
    /// # Safety
    /// `frame` must be a decoded `AVFrame` in this backend's hardware pixel format.
    ///
    /// # Errors
    /// [`HwGiveUp::ExportFailed`] — usually transient, and treated as such upstream.
    #[allow(unused_variables)]
    pub unsafe fn export(
        &mut self,
        frame: *mut sys::AVFrame,
    ) -> Result<Arc<dyn GpuSurface>, HwGiveUp> {
        #[cfg(unix)]
        {
            // SAFETY: caller guarantees a decoded VA-API frame.
            unsafe { super::vaapi::export(frame) }
        }
        #[cfg(windows)]
        {
            // SAFETY: caller guarantees a decoded D3D11 frame.
            unsafe { self.inner.export(frame) }
        }
        #[cfg(not(any(unix, windows)))]
        {
            Err(HwGiveUp::NotCompiled)
        }
    }
}
