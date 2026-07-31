//! The render-thread half of hardware decode: turning a decoder's surface into a
//! `wgpu::Texture` on the compositor's device, and opening a device that can do it.
//!
//! Both halves of the zero-copy path have to agree, and they are set up at opposite ends
//! of the program: the compositor opens its device long before any sender connects, and
//! the extensions it needs to import external memory are not ones `wgpu` requests on its
//! own. So the device is opened *here*, with the interop extensions layered on top of
//! what wgpu asks for, and the result is recorded in [`import_capability`] for the decode
//! side to consult before it commits to a hardware decoder.
//!
//! That global is doing real work rather than hiding a plumbing problem: the compositor
//! is a process singleton (one render thread, one device — architecture §6), the decode
//! threads that need the answer are spawned by a different subsystem, and the alternative
//! is discovering the mismatch one dropped frame at a time on the render thread, which
//! cannot report back to the decoder that produced them.

use std::sync::atomic::{AtomicU8, Ordering};

use castaway_core::GpuSurface;

use super::HwBackendKind;
use crate::error::PipelineError;

#[cfg(all(feature = "hwaccel", unix))]
use super::vulkan_import::VulkanImporter as PlatformImporter;

#[cfg(all(feature = "hwaccel", windows))]
use super::dx12_import::Dx12Importer as PlatformImporter;

/// Whether the compositor's device can take a hardware decoder's surfaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SurfaceImport {
    /// Surfaces from this backend import zero-copy.
    Supported(HwBackendKind),
    /// No import path — anything decoded on the GPU would have to be read back, which
    /// costs more than it saves. Decode belongs on the CPU.
    Unsupported,
}

impl std::fmt::Display for SurfaceImport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Supported(kind) => write!(f, "{kind}"),
            Self::Unsupported => f.write_str("none"),
        }
    }
}

/// Tri-state: not yet determined / supported / not supported. An `AtomicU8` rather than a
/// `OnceLock` because tests build several offscreen compositors in a process and the
/// answer is a property of the last device opened, not of the first.
const UNKNOWN: u8 = 0;
const YES: u8 = 1;
const NO: u8 = 2;
static IMPORT_CAPABILITY: AtomicU8 = AtomicU8::new(UNKNOWN);

/// What the most recently opened compositor device can import.
///
/// Returns [`SurfaceImport::Unsupported`] before any compositor exists, which is the
/// conservative answer: without a compositor there is nothing to import *into*.
#[must_use]
pub fn import_capability() -> SurfaceImport {
    match IMPORT_CAPABILITY.load(Ordering::Relaxed) {
        YES => backend_kind().map_or(SurfaceImport::Unsupported, SurfaceImport::Supported),
        _ => SurfaceImport::Unsupported,
    }
}

/// Record that GPU surface import has proven unusable, so the next session does not try.
///
/// Called from the render thread when imports keep failing on a device that claimed to
/// support them — the decode thread has no other way to learn that its surfaces are
/// landing nowhere.
pub fn mark_import_broken() {
    if IMPORT_CAPABILITY.swap(NO, Ordering::Relaxed) != NO {
        tracing::warn!(
            "GPU surface import is failing on this device; future sessions will decode in software",
        );
    }
}

const fn backend_kind() -> Option<HwBackendKind> {
    HwBackendKind::for_this_platform()
}

/// Opens the compositor's device and imports decoder surfaces into it.
pub struct GpuImporter {
    #[cfg(all(feature = "hwaccel", any(unix, windows)))]
    inner: PlatformImporter,
}

impl GpuImporter {
    /// Open a render device that can import external GPU surfaces.
    ///
    /// `Ok(None)` means this build has no hardware backend at all — expected, and not a
    /// downgrade worth logging. `Err` means a backend exists but its device could not be
    /// opened this way, which *is* worth logging: it is the difference between "this
    /// build doesn't do hwaccel" and "this box won't".
    ///
    /// # Errors
    /// [`PipelineError::GpuInit`] if the interop-capable device could not be created.
    #[allow(unused_variables)]
    pub fn open_device(
        adapter: &wgpu::Adapter,
        limits: wgpu::Limits,
    ) -> Result<Option<(wgpu::Device, wgpu::Queue, Self)>, PipelineError> {
        #[cfg(all(feature = "hwaccel", any(unix, windows)))]
        {
            match PlatformImporter::open_device(adapter, limits) {
                Ok((device, queue, inner)) => {
                    IMPORT_CAPABILITY.store(YES, Ordering::Relaxed);
                    Ok(Some((device, queue, Self { inner })))
                }
                Err(e) => {
                    IMPORT_CAPABILITY.store(NO, Ordering::Relaxed);
                    Err(e)
                }
            }
        }
        #[cfg(not(all(feature = "hwaccel", any(unix, windows))))]
        {
            IMPORT_CAPABILITY.store(NO, Ordering::Relaxed);
            Ok(None)
        }
    }

    /// What this importer can take.
    #[must_use]
    pub fn capability(&self) -> SurfaceImport {
        backend_kind().map_or(SurfaceImport::Unsupported, SurfaceImport::Supported)
    }

    /// Import a single-plane browser frame (D36).
    ///
    /// Separate from [`Self::import`] because the producer is different in kind: a
    /// decoder hands over NV12 surfaces from a pool it owns, a browser hands over one
    /// RGBA buffer per paint that it will recycle the moment we release it.
    ///
    /// # Errors
    /// [`PipelineError::GpuImport`] if this build has no importer or the frame's layout
    /// is one the single-plane path cannot describe.
    #[cfg(all(feature = "hwaccel", unix))]
    pub fn import_single_plane(
        &mut self,
        device: &wgpu::Device,
        geometry: super::FrameGeometry,
        modifier: u64,
        plane: super::dmabuf::PlaneLayout,
        owner: std::sync::Arc<dyn GpuSurface>,
    ) -> Result<wgpu::Texture, PipelineError> {
        self.inner
            .import_single_plane(device, geometry, modifier, plane, owner)
    }

    /// Import one decoder surface as an NV12 texture on `device`.
    ///
    /// # Errors
    /// [`PipelineError::GpuImport`] if the surface is not one this platform understands,
    /// or the driver refused the import.
    #[cfg_attr(
        not(all(feature = "hwaccel", any(unix, windows))),
        allow(unused_variables)
    )]
    pub fn import(
        &mut self,
        device: &wgpu::Device,
        surface: &std::sync::Arc<dyn GpuSurface>,
    ) -> Result<wgpu::Texture, PipelineError> {
        #[cfg(all(feature = "hwaccel", any(unix, windows)))]
        {
            self.inner.import(device, surface)
        }
        #[cfg(not(all(feature = "hwaccel", any(unix, windows))))]
        {
            Err(PipelineError::GpuImport(
                "this build has no GPU surface importer".into(),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Note: the capability flag is process-global, so it is deliberately *not* mutated
    // from unit tests — doing so would race the compositor tests running beside them.
    // Its real exercise is the offscreen hwaccel test, which opens a device and then
    // decodes through it.

    #[test]
    fn no_compositor_means_no_import() {
        // The conservative answer matters: without a compositor there is nothing to
        // import *into*, so a decoder must not start producing GPU surfaces.
        assert_eq!(
            match UNKNOWN {
                YES => backend_kind().map_or(SurfaceImport::Unsupported, SurfaceImport::Supported),
                _ => SurfaceImport::Unsupported,
            },
            SurfaceImport::Unsupported,
        );
    }

    #[test]
    fn capability_renders_for_logs() {
        assert_eq!(
            SurfaceImport::Supported(HwBackendKind::Vaapi).to_string(),
            "vaapi",
        );
        assert_eq!(SurfaceImport::Unsupported.to_string(), "none");
    }
}
