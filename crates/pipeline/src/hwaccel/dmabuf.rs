//! The Linux hand-off type: a decoded VA-API surface described as DMA-BUF planes.
//!
//! This is what crosses the thread boundary between the decode thread and the render
//! thread. It is deliberately *not* a Vulkan object: the decode thread has no access to
//! the compositor's device, and building one there would put GPU-API calls on the wrong
//! thread. What travels is a description — file descriptors, a DRM format modifier, and
//! per-plane offsets and pitches — plus, crucially, a reference that keeps the decoder's
//! surface out of its own reuse pool.
//!
//! ## Why the `AVFrame` reference is load-bearing
//!
//! A DMA-BUF fd keeps the *buffer object* alive in the kernel, so the memory cannot be
//! freed underneath us. It does **not** stop libavcodec from handing the same VA surface
//! back to the decoder for the next picture — which would overwrite the pixels we are
//! still displaying, producing a tear that only shows up under load and only on some
//! drivers. Holding a ref on the source frame is what makes the pool skip it. That is the
//! single subtlest thing in the Linux path, so the reference is owned by this type rather
//! than left to the caller to remember.
#![allow(
    unsafe_code,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap,
    clippy::ptr_as_ptr
)]

use std::os::fd::RawFd;

use castaway_core::{ColorInfo, GpuSurface};
use ffmpeg_sys_next as sys;

/// `DRM_FORMAT_NV12` — `fourcc_code('N', 'V', '1', '2')`.
pub const DRM_FORMAT_NV12: u32 =
    (b'N' as u32) | ((b'V' as u32) << 8) | ((b'1' as u32) << 16) | ((b'2' as u32) << 24);

/// `DRM_FORMAT_R8` — the single-channel layer VA-API sometimes reports the luma plane as.
pub const DRM_FORMAT_R8: u32 =
    (b'R' as u32) | ((b'8' as u32) << 8) | ((b' ' as u32) << 16) | ((b' ' as u32) << 24);

/// `DRM_FORMAT_GR88` — likewise for the interleaved chroma plane.
pub const DRM_FORMAT_GR88: u32 =
    (b'G' as u32) | ((b'R' as u32) << 8) | ((b'8' as u32) << 16) | ((b'8' as u32) << 24);

/// One plane of an imported surface.
#[derive(Debug, Clone, Copy)]
pub struct PlaneLayout {
    /// The DMA-BUF this plane lives in. Borrowed from the frame this surface holds — the
    /// importer dups it, because `vkAllocateMemory` takes ownership of what it is given.
    pub fd: RawFd,
    /// Byte offset of the plane within its buffer.
    pub offset: u64,
    /// Bytes per row.
    pub pitch: u64,
}

/// An `AVFrame` we own a reference to, safe to move between threads.
///
/// Exists only to be dropped at the right time — nothing reads through it after
/// construction, which is what makes the `Send`/`Sync` claims below true.
struct FrameRef(*mut sys::AVFrame);

// SAFETY: `AVFrame`'s reference count is atomic, and this wrapper exposes no way to read
// or mutate the frame after it is built — the only operation is `av_frame_free` in
// `Drop`. Moving the sole owner between threads is therefore free of data races.
unsafe impl Send for FrameRef {}
// SAFETY: as above; `&FrameRef` grants no access to the frame at all, so sharing a
// reference across threads cannot race.
unsafe impl Sync for FrameRef {}

impl Drop for FrameRef {
    fn drop(&mut self) {
        // SAFETY: `self.0` came from `av_frame_alloc` and has not been freed — `Drop`
        // runs once. `av_frame_free` unrefs the buffers (releasing the VA surface back to
        // the decoder's pool and closing the DMA-BUF fds) and nulls the pointer.
        unsafe { sys::av_frame_free(&raw mut self.0) };
    }
}

impl std::fmt::Debug for FrameRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FrameRef(<AVFrame>)")
    }
}

/// A decoded NV12 surface, described as DMA-BUF planes.
#[derive(Debug)]
pub struct DmaBufSurface {
    /// Picture width in pixels.
    pub width: u32,
    /// Picture height in pixels.
    pub height: u32,
    /// The DRM format modifier all planes share (tiling/compression layout).
    pub modifier: u64,
    /// Luma then chroma. Always exactly two: this path handles 8-bit NV12 only.
    pub planes: [PlaneLayout; 2],
    color: ColorInfo,
    /// Keeps the DMA-BUFs open *and* the decoder's surface out of its reuse pool. See
    /// the module docs — dropping this early is a tearing bug, not a leak.
    _frame: FrameRef,
}

impl GpuSurface for DmaBufSurface {
    fn color(&self) -> ColorInfo {
        self.color
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

impl DmaBufSurface {
    /// Build a surface from a mapped `AV_PIX_FMT_DRM_PRIME` frame, taking ownership of
    /// the frame reference.
    ///
    /// # Safety
    /// `frame` must be a live `AVFrame` whose `data[0]` points at an
    /// `AVDRMFrameDescriptor`, i.e. the output of `av_hwframe_map` to `DRM_PRIME`.
    /// Ownership of the reference transfers to the returned surface.
    pub unsafe fn from_drm_frame(
        frame: *mut sys::AVFrame,
        color: ColorInfo,
    ) -> Result<Self, DmaBufError> {
        // SAFETY: the caller guarantees `frame` is a live mapped DRM_PRIME frame, so
        // `data[0]` is the descriptor libavutil wrote there.
        let (desc, width, height) = unsafe {
            let f = &*frame;
            (
                f.data[0].cast::<sys::AVDRMFrameDescriptor>(),
                f.width,
                f.height,
            )
        };
        if desc.is_null() {
            return Err(DmaBufError::NoDescriptor);
        }
        // SAFETY: non-null and produced by `av_hwframe_map`, which fully initializes it.
        let desc = unsafe { &*desc };

        let layout = plane_layout(desc)?;
        let width = u32::try_from(width).map_err(|_| DmaBufError::BadGeometry)?;
        let height = u32::try_from(height).map_err(|_| DmaBufError::BadGeometry)?;
        if width == 0 || height == 0 {
            return Err(DmaBufError::BadGeometry);
        }

        Ok(Self {
            width,
            height,
            modifier: layout.modifier,
            planes: layout.planes,
            color,
            _frame: FrameRef(frame),
        })
    }
}

/// The normalized two-plane view of whatever shape the driver described.
struct Normalized {
    modifier: u64,
    planes: [PlaneLayout; 2],
}

/// Reduce an `AVDRMFrameDescriptor` to "NV12, two planes, one modifier", or say why it
/// cannot be.
///
/// Drivers describe the same surface two different ways: one NV12 layer with two planes,
/// or two single-channel layers (R8 luma + GR88 chroma). Both are accepted; anything else
/// is a format this path does not handle, and the honest answer is to fall back rather
/// than guess at the layout.
fn plane_layout(desc: &sys::AVDRMFrameDescriptor) -> Result<Normalized, DmaBufError> {
    let objects = usize::try_from(desc.nb_objects).map_err(|_| DmaBufError::BadDescriptor)?;
    let layers = usize::try_from(desc.nb_layers).map_err(|_| DmaBufError::BadDescriptor)?;
    if objects == 0 || objects > desc.objects.len() || layers == 0 || layers > desc.layers.len() {
        return Err(DmaBufError::BadDescriptor);
    }

    // Every object must agree on the modifier: a single Vulkan image is created with one
    // `drmFormatModifier`, so two different tilings cannot be one image.
    let modifier = desc.objects[0].format_modifier;
    for object in &desc.objects[..objects] {
        if object.format_modifier != modifier {
            return Err(DmaBufError::MixedModifiers);
        }
    }

    let mut collected: Vec<PlaneLayout> = Vec::with_capacity(2);
    for layer in &desc.layers[..layers] {
        let format = layer.format;
        if !matches!(format, DRM_FORMAT_NV12 | DRM_FORMAT_R8 | DRM_FORMAT_GR88) {
            return Err(DmaBufError::UnsupportedFormat(format));
        }
        let count = usize::try_from(layer.nb_planes).map_err(|_| DmaBufError::BadDescriptor)?;
        if count > layer.planes.len() {
            return Err(DmaBufError::BadDescriptor);
        }
        for plane in &layer.planes[..count] {
            let index =
                usize::try_from(plane.object_index).map_err(|_| DmaBufError::BadDescriptor)?;
            let object = desc
                .objects
                .get(index)
                .filter(|_| index < objects)
                .ok_or(DmaBufError::BadDescriptor)?;
            collected.push(PlaneLayout {
                fd: object.fd,
                offset: u64::try_from(plane.offset).map_err(|_| DmaBufError::BadDescriptor)?,
                pitch: u64::try_from(plane.pitch).map_err(|_| DmaBufError::BadDescriptor)?,
            });
        }
    }

    match collected.as_slice() {
        [luma, chroma] => Ok(Normalized {
            modifier,
            planes: [*luma, *chroma],
        }),
        other => Err(DmaBufError::PlaneCount(other.len())),
    }
}

/// Why a decoded surface could not be described as importable DMA-BUF planes.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum DmaBufError {
    /// The mapped frame carried no DRM descriptor.
    #[error("mapped frame has no DRM descriptor")]
    NoDescriptor,
    /// Object/layer/plane counts were outside what the descriptor can hold.
    #[error("DRM descriptor is internally inconsistent")]
    BadDescriptor,
    /// Zero or absurd picture dimensions.
    #[error("surface has unusable dimensions")]
    BadGeometry,
    /// Planes live in buffers with different tilings; one Vulkan image cannot cover them.
    #[error("planes use different DRM format modifiers")]
    MixedModifiers,
    /// A layer format this path does not handle — 10-bit P010 lands here, and falling
    /// back is the right answer rather than reinterpreting the bytes.
    #[error("unsupported DRM layer format {0:#010x}")]
    UnsupportedFormat(u32),
    /// Not the two planes NV12 requires.
    #[error("expected 2 planes for NV12, got {0}")]
    PlaneCount(usize),
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// One layer as the test spells it: a DRM fourcc and its planes, each an
    /// `(object index, byte offset, row pitch)` triple.
    type LayerSpec<'a> = (u32, &'a [(u32, isize, isize)]);

    /// Build a descriptor the way a driver would, so the normalizer can be tested without
    /// a GPU. Everything it reads is plain data.
    fn descriptor(objects: &[(RawFd, u64)], layers: &[LayerSpec<'_>]) -> sys::AVDRMFrameDescriptor {
        // SAFETY: `AVDRMFrameDescriptor` is a plain C aggregate of integers and fixed
        // arrays with no invalid bit patterns, so an all-zero value is a valid one.
        let mut desc: sys::AVDRMFrameDescriptor = unsafe { std::mem::zeroed() };
        desc.nb_objects = i32::try_from(objects.len()).unwrap();
        for (slot, (fd, modifier)) in desc.objects.iter_mut().zip(objects) {
            slot.fd = *fd;
            slot.size = 4096;
            slot.format_modifier = *modifier;
        }
        desc.nb_layers = i32::try_from(layers.len()).unwrap();
        for (slot, (format, planes)) in desc.layers.iter_mut().zip(layers) {
            slot.format = *format;
            slot.nb_planes = i32::try_from(planes.len()).unwrap();
            for (pslot, (object_index, offset, pitch)) in slot.planes.iter_mut().zip(*planes) {
                pslot.object_index = i32::try_from(*object_index).unwrap();
                pslot.offset = *offset;
                pslot.pitch = *pitch;
            }
        }
        desc
    }

    #[test]
    fn one_nv12_layer_with_two_planes_normalizes() {
        // The common AMD/RADV shape: both planes in one buffer at different offsets.
        let desc = descriptor(
            &[(7, 0x02)],
            &[(DRM_FORMAT_NV12, &[(0, 0, 1920), (0, 2_073_600, 1920)])],
        );
        let got = plane_layout(&desc).expect("should normalize");
        assert_eq!(got.modifier, 0x02);
        assert_eq!(got.planes[0].fd, 7);
        assert_eq!(got.planes[0].offset, 0);
        assert_eq!(got.planes[1].offset, 2_073_600);
        assert_eq!(got.planes[1].pitch, 1920);
    }

    #[test]
    fn two_single_channel_layers_normalize_to_the_same_thing() {
        // The other shape drivers report — R8 luma plus GR88 chroma, in separate buffers.
        // Treating this as "unsupported" would drop hwaccel on hardware that works fine.
        let desc = descriptor(
            &[(3, 0x02), (4, 0x02)],
            &[
                (DRM_FORMAT_R8, &[(0, 0, 1920)]),
                (DRM_FORMAT_GR88, &[(1, 0, 1920)]),
            ],
        );
        let got = plane_layout(&desc).expect("should normalize");
        assert_eq!(got.planes[0].fd, 3);
        assert_eq!(got.planes[1].fd, 4);
    }

    #[test]
    fn mixed_modifiers_are_refused() {
        // One Vulkan image carries one modifier; importing these as one image would
        // describe the second plane's tiling incorrectly and render garbage.
        let desc = descriptor(
            &[(3, 0x02), (4, 0x09)],
            &[
                (DRM_FORMAT_R8, &[(0, 0, 64)]),
                (DRM_FORMAT_GR88, &[(1, 0, 64)]),
            ],
        );
        assert!(matches!(
            plane_layout(&desc),
            Err(DmaBufError::MixedModifiers),
        ));
    }

    #[test]
    fn a_ten_bit_surface_is_refused_rather_than_reinterpreted() {
        // P010 arrives as a format this path does not describe. Reading it as NV12 would
        // render a plausible-looking but wrong picture; refusing lets the caller fall
        // back to software, which is the whole point of the give-up path.
        let p010 =
            (b'P' as u32) | ((b'0' as u32) << 8) | ((b'1' as u32) << 16) | ((b'0' as u32) << 24);
        let desc = descriptor(&[(3, 0)], &[(p010, &[(0, 0, 3840)])]);
        assert!(matches!(
            plane_layout(&desc),
            Err(DmaBufError::UnsupportedFormat(f)) if f == p010,
        ));
    }

    #[test]
    fn a_single_plane_surface_is_refused() {
        let desc = descriptor(&[(3, 0)], &[(DRM_FORMAT_R8, &[(0, 0, 64)])]);
        assert!(matches!(
            plane_layout(&desc),
            Err(DmaBufError::PlaneCount(1))
        ));
    }

    #[test]
    fn a_plane_pointing_past_the_object_table_is_refused() {
        // A malformed descriptor must not become an out-of-bounds read.
        let desc = descriptor(&[(3, 0)], &[(DRM_FORMAT_NV12, &[(0, 0, 64), (5, 0, 64)])]);
        assert!(matches!(
            plane_layout(&desc),
            Err(DmaBufError::BadDescriptor)
        ));
    }

    #[test]
    fn fourcc_constants_match_the_drm_headers() {
        // These are copied by hand from `drm_fourcc.h`; a typo would show up as an
        // "unsupported format" on every frame, so pin the byte values.
        assert_eq!(DRM_FORMAT_NV12, 0x3231_564e);
        assert_eq!(DRM_FORMAT_R8, 0x2020_3852);
        assert_eq!(DRM_FORMAT_GR88, 0x3838_5247);
    }
}
