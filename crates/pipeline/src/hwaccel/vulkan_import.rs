//! DMA-BUF → `VkImage` → `wgpu::Texture`, without a copy.
//!
//! This is the slice that makes hardware decode worth doing on Linux. Everything else in
//! the hwaccel path is bookkeeping around it.
//!
//! Two problems had to be solved, and neither is visible from `wgpu`'s safe API:
//!
//! **The device has to be opened differently.** `wgpu` never requests
//! `VK_KHR_external_memory_fd`, `VK_EXT_external_memory_dma_buf`, or
//! `VK_EXT_image_drm_format_modifier`, because nothing in its own API needs them. So the
//! logical device is built here — the extension list `wgpu-hal` would have used, plus the
//! interop ones — and handed back through `Adapter::create_device_from_hal`. That is the
//! documented seam for exactly this (`physical_device_features`'s docs say adding
//! extensions is fine), not a hole we are climbing through.
//!
//! **The image has to describe the driver's tiling.** A decoded surface is not linear; it
//! carries a DRM format modifier saying how it is swizzled and whether it is
//! DCC-compressed. `VkImageDrmFormatModifierExplicitCreateInfoEXT` is how that, plus the
//! per-plane offsets and pitches, is handed to Vulkan. Get it wrong and the import
//! *succeeds* and renders a plausible-looking scrambled picture, which is why the
//! offscreen readback test decodes a known pattern rather than merely asserting no error.
//!
//! Once the image exists, `wgpu` does the rest: `TextureFormat::NV12` with
//! `TextureAspect::Plane0`/`Plane1` views is already supported on Vulkan and DX12, so the
//! sampling half needed no raw HAL at all.
#![allow(
    unsafe_code,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap,
    clippy::ptr_as_ptr
)]

use std::os::fd::{AsRawFd as _, FromRawFd as _, IntoRawFd as _, OwnedFd, RawFd};

use ash::vk;
use castaway_core::GpuSurface;
use wgpu::hal::api::Vulkan;

use super::dmabuf::DmaBufSurface;
use crate::error::PipelineError;

/// The DMA-BUF external memory handle type, used for both the image and the allocation.
const DMA_BUF: vk::ExternalMemoryHandleTypeFlags = vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT;

/// The interop extensions layered on top of what `wgpu-hal` asks for.
///
/// `image_format_list` is not optional decoration: `VK_EXT_image_drm_format_modifier`
/// requires it whenever the image is created with `MUTABLE_FORMAT`, which per-plane views
/// force.
fn interop_extensions() -> [&'static std::ffi::CStr; 4] {
    [
        ash::khr::external_memory_fd::NAME,
        ash::ext::external_memory_dma_buf::NAME,
        ash::ext::image_drm_format_modifier::NAME,
        ash::khr::image_format_list::NAME,
    ]
}

/// Imports VA-API surfaces into the compositor's Vulkan device.
pub struct VulkanImporter {
    device: ash::Device,
    external_memory_fd: ash::khr::external_memory_fd::Device,
    memory_properties: vk::PhysicalDeviceMemoryProperties,
}

impl VulkanImporter {
    /// Open a Vulkan device that can import DMA-BUFs, and a `wgpu` device on top of it.
    ///
    /// # Errors
    /// [`PipelineError::GpuInit`] if the adapter is not Vulkan, lacks NV12 or the interop
    /// extensions, or the device cannot be created. Every one of these is a "decode in
    /// software instead" answer, not a fatal one.
    pub fn open_device(
        adapter: &wgpu::Adapter,
        limits: wgpu::Limits,
    ) -> Result<(wgpu::Device, wgpu::Queue, Self), PipelineError> {
        let info = adapter.get_info();
        if info.backend != wgpu::Backend::Vulkan {
            return Err(PipelineError::GpuInit(format!(
                "DMA-BUF import needs the Vulkan backend, this adapter is {:?}",
                info.backend
            )));
        }
        // NV12 sampling is the point; without it an imported surface has no shader path.
        if !adapter
            .features()
            .contains(wgpu::Features::TEXTURE_FORMAT_NV12)
        {
            return Err(PipelineError::GpuInit(
                "adapter does not support TEXTURE_FORMAT_NV12".into(),
            ));
        }

        let features = wgpu::Features::TEXTURE_FORMAT_NV12;
        let memory_hints = wgpu::MemoryHints::Performance;

        // Everything that needs the hal adapter happens inside this closure, and only an
        // owned `OpenDevice` comes back out: calling other `wgpu` entry points while the
        // adapter is borrowed from the hub would risk re-entering its registry locks.
        // SAFETY: `as_hal` hands out the backend adapter for the duration of the closure
        // and we neither store it nor use it afterwards.
        let opened = unsafe {
            adapter.as_hal::<Vulkan, _, _>(|hal_adapter| {
                let hal_adapter = hal_adapter.ok_or_else(|| {
                    PipelineError::GpuInit("adapter has no Vulkan backend".into())
                })?;
                open_hal_device(hal_adapter, features, &memory_hints)
            })
        }?;

        // SAFETY: `opened` was produced by this adapter's own `device_from_raw`, with the
        // extensions and features named in the descriptor below.
        let (device, queue) = unsafe {
            adapter.create_device_from_hal::<Vulkan>(
                opened,
                &wgpu::DeviceDescriptor {
                    label: Some("castaway-compositor-interop"),
                    required_features: features,
                    required_limits: limits,
                    memory_hints,
                },
                None,
            )
        }
        .map_err(|e| PipelineError::GpuInit(format!("create_device_from_hal: {e}")))?;

        // SAFETY: the device was just created and is not in use; we only clone handles,
        // which is explicitly not "manually destroying" them.
        // (`Device::as_hal` wraps the callback's result in its own `Option` for the
        // non-core backends, hence the flatten.)
        let handles = unsafe {
            device.as_hal::<Vulkan, _, _>(|d| {
                d.map(|d| {
                    (
                        d.raw_device().clone(),
                        d.shared_instance().raw_instance().clone(),
                        d.raw_physical_device(),
                    )
                })
            })
        }
        .flatten();
        let (raw_device, raw_instance, physical) =
            handles.ok_or_else(|| PipelineError::GpuInit("device has no Vulkan backend".into()))?;

        let external_memory_fd =
            ash::khr::external_memory_fd::Device::new(&raw_instance, &raw_device);
        // SAFETY: `physical` belongs to `raw_instance` and the query only reads.
        let memory_properties =
            unsafe { raw_instance.get_physical_device_memory_properties(physical) };

        Ok((
            device,
            queue,
            Self {
                device: raw_device,
                external_memory_fd,
                memory_properties,
            },
        ))
    }

    /// Import one DMA-BUF surface as an NV12 `wgpu::Texture`.
    ///
    /// # Errors
    /// [`PipelineError::GpuImport`] if the surface is not a DMA-BUF one, or Vulkan
    /// refuses the image, allocation, or bind.
    pub fn import(
        &mut self,
        device: &wgpu::Device,
        surface: &std::sync::Arc<dyn GpuSurface>,
    ) -> Result<wgpu::Texture, PipelineError> {
        let dmabuf: &DmaBufSurface = surface
            .as_any()
            .downcast_ref()
            .ok_or_else(|| PipelineError::GpuImport("surface is not a DMA-BUF surface".into()))?;

        // `vkAllocateMemory` takes ownership of the fd it is given, and the surface's fds
        // belong to the AVFrame it holds — so every import works on duplicates.
        let disjoint = dmabuf.planes[0].fd != dmabuf.planes[1].fd;
        let fds = if disjoint {
            vec![dup_fd(dmabuf.planes[0].fd)?, dup_fd(dmabuf.planes[1].fd)?]
        } else {
            vec![dup_fd(dmabuf.planes[0].fd)?]
        };

        // SAFETY: every call below is against `self.device`, which outlives this import;
        // the guard installed at the end owns the image and memory and destroys them when
        // wgpu is finished with the texture.
        let imported =
            unsafe { self.build_image(dmabuf, std::sync::Arc::clone(surface), fds, disjoint) }?;

        let extent = wgpu::Extent3d {
            width: dmabuf.width,
            height: dmabuf.height,
            depth_or_array_layers: 1,
        };
        let hal_desc = wgpu::hal::TextureDescriptor {
            label: Some("imported-nv12"),
            size: extent,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::NV12,
            usage: wgpu::hal::TextureUses::RESOURCE,
            memory_flags: wgpu::hal::MemoryFlags::empty(),
            view_formats: vec![],
        };

        // SAFETY: `imported.image` was created respecting `hal_desc` (same extent,
        // format, single mip and sample), and the drop guard destroys it, which is
        // exactly the contract `texture_from_raw` states for a `Some(drop_guard)`.
        let hal_texture = unsafe {
            wgpu::hal::vulkan::Device::texture_from_raw(
                imported.image,
                &hal_desc,
                Some(Box::new(imported.guard)),
            )
        };

        // SAFETY: the hal texture was built from this device's own image and matches the
        // descriptor below.
        Ok(unsafe {
            device.create_texture_from_hal::<Vulkan>(
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

    /// Import a single-plane RGBA-family DMA-BUF as a `wgpu::Texture`.
    ///
    /// This is the shape an offscreen browser delivers (D36/#64): Electron's
    /// shared-texture OSR hands over one `NativePixmapHandle` plane of BGRA, where the
    /// decoder path above hands two planes of NV12. Same extensions, same explicit
    /// modifier layout, no disjoint case and no `MUTABLE_FORMAT` — an RGBA image is
    /// sampled as itself, so no per-plane views and no format list.
    ///
    /// `owner` is dropped only when wgpu has retired every submission referencing the
    /// texture. For a browser frame that is what gates the `release()` ack back to the
    /// producer — Chromium recycles the buffer exactly the way libavcodec recycles VA
    /// surfaces, so releasing early is the same tearing bug `DmaBufSurface` documents.
    ///
    /// # Errors
    /// [`PipelineError::GpuImport`] if the format is not BGRA/RGBA8 or Vulkan refuses
    /// the image, allocation, or bind.
    pub fn import_single_plane(
        &mut self,
        device: &wgpu::Device,
        geometry: super::FrameGeometry,
        modifier: u64,
        plane: super::dmabuf::PlaneLayout,
        owner: std::sync::Arc<dyn GpuSurface>,
    ) -> Result<wgpu::Texture, PipelineError> {
        let geometry = geometry.validate()?;
        // The `SRGB` formats describe the same bytes and the same DRM layout as their
        // `UNORM` siblings — the difference is only that sampling decodes. RADV lists
        // the same modifiers for both.
        let vk_format = match geometry.format {
            wgpu::TextureFormat::Bgra8Unorm => vk::Format::B8G8R8A8_UNORM,
            wgpu::TextureFormat::Bgra8UnormSrgb => vk::Format::B8G8R8A8_SRGB,
            wgpu::TextureFormat::Rgba8UnormSrgb => vk::Format::R8G8B8A8_SRGB,
            // Validated above, so this arm is the RGBA one and nothing else can reach it.
            _ => vk::Format::R8G8B8A8_UNORM,
        };

        let fd = dup_fd(plane.fd)?;

        let plane_layouts = [vk::SubresourceLayout {
            offset: plane.offset,
            size: 0,
            row_pitch: plane.pitch,
            array_pitch: 0,
            depth_pitch: 0,
        }];
        let mut modifier_info = vk::ImageDrmFormatModifierExplicitCreateInfoEXT::default()
            .drm_format_modifier(modifier)
            .plane_layouts(&plane_layouts);
        let mut external_info = vk::ExternalMemoryImageCreateInfo::default().handle_types(DMA_BUF);

        let create_info = vk::ImageCreateInfo::default()
            .push_next(&mut external_info)
            .push_next(&mut modifier_info)
            .image_type(vk::ImageType::TYPE_2D)
            .format(vk_format)
            .extent(vk::Extent3D {
                width: geometry.width,
                height: geometry.height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
            .usage(vk::ImageUsageFlags::SAMPLED)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            // See the NV12 path: `UNDEFINED` is the only legal initial layout for an
            // imported DRM-modifier image, and the driver preserves the pixels across
            // the first transition.
            .initial_layout(vk::ImageLayout::UNDEFINED);

        // SAFETY: `create_info` and every chained structure live until after this call.
        let image = unsafe { self.device.create_image(&create_info, None) }
            .map_err(|e| PipelineError::GpuImport(format!("vkCreateImage: {e}")))?;

        let mut guard = ImageGuard {
            device: self.device.clone(),
            image,
            memory: Vec::new(),
            _surface: owner,
        };

        // SAFETY: `image` is live and `fd` is a duplicate this call may consume.
        let memory = unsafe { self.import_memory(image, fd, None) }?;
        guard.memory.push(memory);
        let infos = [vk::BindImageMemoryInfo::default()
            .image(image)
            .memory(memory)
            .memory_offset(0)];
        // SAFETY: image and memory are live and owned by the guard.
        unsafe { self.device.bind_image_memory2(&infos) }
            .map_err(|e| PipelineError::GpuImport(format!("vkBindImageMemory2: {e}")))?;

        let extent = geometry.extent();
        let hal_desc = wgpu::hal::TextureDescriptor {
            label: Some("imported-browser-frame"),
            size: extent,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: geometry.format,
            usage: wgpu::hal::TextureUses::RESOURCE,
            memory_flags: wgpu::hal::MemoryFlags::empty(),
            view_formats: vec![],
        };
        // SAFETY: `image` was created respecting `hal_desc`, and the guard destroys it —
        // the contract `texture_from_raw` states for a `Some(drop_guard)`.
        let hal_texture = unsafe {
            wgpu::hal::vulkan::Device::texture_from_raw(image, &hal_desc, Some(Box::new(guard)))
        };
        // SAFETY: the hal texture was built from this device's own image and matches the
        // descriptor below.
        Ok(unsafe {
            device.create_texture_from_hal::<Vulkan>(
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
        })
    }

    /// Create the `VkImage`, import the DMA-BUFs behind it, and bind them.
    ///
    /// # Safety
    /// `fds` must be duplicates this call may take ownership of, matching `disjoint`:
    /// one fd when both planes share a buffer, two when they do not.
    unsafe fn build_image(
        &self,
        surface: &DmaBufSurface,
        owner: std::sync::Arc<dyn GpuSurface>,
        fds: Vec<OwnedFd>,
        disjoint: bool,
    ) -> Result<Imported, PipelineError> {
        // The explicit plane layouts are what tell the driver where each plane starts and
        // how wide its rows are; `size` and the pitches beyond `row_pitch` are required to
        // be zero for a DRM-modifier image.
        let plane_layouts: Vec<vk::SubresourceLayout> = surface
            .planes
            .iter()
            .map(|p| vk::SubresourceLayout {
                offset: p.offset,
                size: 0,
                row_pitch: p.pitch,
                array_pitch: 0,
                depth_pitch: 0,
            })
            .collect();

        let mut modifier_info = vk::ImageDrmFormatModifierExplicitCreateInfoEXT::default()
            .drm_format_modifier(surface.modifier)
            .plane_layouts(&plane_layouts);
        let mut external_info = vk::ExternalMemoryImageCreateInfo::default().handle_types(DMA_BUF);

        // `MUTABLE_FORMAT` is required to take single-plane views (R8 luma, RG8 chroma)
        // of a two-plane image, and `image_format_list` is required alongside it.
        let view_formats = [
            vk::Format::G8_B8R8_2PLANE_420_UNORM,
            vk::Format::R8_UNORM,
            vk::Format::R8G8_UNORM,
        ];
        let mut format_list = vk::ImageFormatListCreateInfo::default().view_formats(&view_formats);

        let mut flags = vk::ImageCreateFlags::MUTABLE_FORMAT | vk::ImageCreateFlags::ALIAS;
        if disjoint {
            flags |= vk::ImageCreateFlags::DISJOINT;
        }

        let create_info = vk::ImageCreateInfo::default()
            .push_next(&mut external_info)
            .push_next(&mut modifier_info)
            .push_next(&mut format_list)
            .flags(flags)
            .image_type(vk::ImageType::TYPE_2D)
            .format(vk::Format::G8_B8R8_2PLANE_420_UNORM)
            .extent(vk::Extent3D {
                width: surface.width,
                height: surface.height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
            .usage(vk::ImageUsageFlags::SAMPLED)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            // A DMA-BUF arrives with contents but no Vulkan layout; `UNDEFINED` is the
            // only legal initial layout, and wgpu's first barrier moves it to
            // shader-read. Mesa preserves the pixels across that transition for a
            // DRM-modifier image — which the offscreen readback test is there to prove,
            // because the spec permits a driver to discard them.
            .initial_layout(vk::ImageLayout::UNDEFINED);

        // SAFETY: `create_info` and every structure it chains live until after this call.
        let image = unsafe { self.device.create_image(&create_info, None) }
            .map_err(|e| PipelineError::GpuImport(format!("vkCreateImage: {e}")))?;

        // From here on any failure must destroy the image, so the guard is built first
        // and takes ownership of everything as it is created.
        let mut guard = ImageGuard {
            device: self.device.clone(),
            image,
            memory: Vec::new(),
            _surface: owner,
        };

        // `vkBindImageMemory2` covers both shapes, but the disjoint one needs a
        // per-plane chained struct. The two cases are written out rather than looped
        // because each `BindImagePlaneMemoryInfo` has to be a distinct live local for the
        // duration of the call — a `Vec` of them cannot be borrowed mutably twice.
        let mut fds = fds.into_iter();
        let first = fds
            .next()
            .ok_or_else(|| PipelineError::GpuImport("no DMA-BUF to import".into()))?;

        if disjoint {
            let second = fds
                .next()
                .ok_or_else(|| PipelineError::GpuImport("disjoint surface has one fd".into()))?;
            // SAFETY: `image` is live and each fd is a duplicate we own.
            let luma = unsafe {
                self.import_memory(image, first, Some(vk::ImageAspectFlags::MEMORY_PLANE_0_EXT))
            }?;
            guard.memory.push(luma);
            // SAFETY: as above.
            let chroma = unsafe {
                self.import_memory(
                    image,
                    second,
                    Some(vk::ImageAspectFlags::MEMORY_PLANE_1_EXT),
                )
            }?;
            guard.memory.push(chroma);

            let mut plane0 =
                vk::BindImagePlaneMemoryInfo::default().plane_aspect(vk::ImageAspectFlags::PLANE_0);
            let mut plane1 =
                vk::BindImagePlaneMemoryInfo::default().plane_aspect(vk::ImageAspectFlags::PLANE_1);
            let infos = [
                vk::BindImageMemoryInfo::default()
                    .image(image)
                    .memory(luma)
                    .memory_offset(0)
                    .push_next(&mut plane0),
                vk::BindImageMemoryInfo::default()
                    .image(image)
                    .memory(chroma)
                    .memory_offset(0)
                    .push_next(&mut plane1),
            ];
            // SAFETY: image and memories are live and owned by the guard; the chained
            // plane structs outlive the call.
            unsafe { self.device.bind_image_memory2(&infos) }
                .map_err(|e| PipelineError::GpuImport(format!("vkBindImageMemory2: {e}")))?;
        } else {
            // SAFETY: `image` is live and `first` is a duplicate we own.
            let memory = unsafe { self.import_memory(image, first, None) }?;
            guard.memory.push(memory);
            let infos = [vk::BindImageMemoryInfo::default()
                .image(image)
                .memory(memory)
                .memory_offset(0)];
            // SAFETY: image and memory are live and owned by the guard.
            unsafe { self.device.bind_image_memory2(&infos) }
                .map_err(|e| PipelineError::GpuImport(format!("vkBindImageMemory2: {e}")))?;
        }

        Ok(Imported { image, guard })
    }

    /// Import one DMA-BUF as device memory sized for `image` (or for one of its planes).
    ///
    /// # Safety
    /// `image` must be live and `fd` a duplicate whose ownership may transfer to Vulkan.
    unsafe fn import_memory(
        &self,
        image: vk::Image,
        fd: OwnedFd,
        aspect: Option<vk::ImageAspectFlags>,
    ) -> Result<vk::DeviceMemory, PipelineError> {
        let raw_fd = fd.as_raw_fd();

        // What memory types this particular DMA-BUF can be imported as…
        let mut fd_properties = vk::MemoryFdPropertiesKHR::default();
        // SAFETY: `raw_fd` is a live DMA-BUF; the query does not consume it and writes
        // only into `fd_properties`.
        unsafe {
            self.external_memory_fd
                .get_memory_fd_properties(DMA_BUF, raw_fd, &mut fd_properties)
        }
        .map_err(|e| PipelineError::GpuImport(format!("vkGetMemoryFdPropertiesKHR: {e}")))?;

        // …intersected with what the image will accept.
        let mut plane_info =
            aspect.map(|a| vk::ImagePlaneMemoryRequirementsInfo::default().plane_aspect(a));
        let mut requirements_info = vk::ImageMemoryRequirementsInfo2::default().image(image);
        if let Some(plane) = plane_info.as_mut() {
            requirements_info = requirements_info.push_next(plane);
        }
        let mut requirements = vk::MemoryRequirements2::default();
        // SAFETY: `image` is live; the call only writes into `requirements`.
        unsafe {
            self.device
                .get_image_memory_requirements2(&requirements_info, &mut requirements);
        }

        let allowed =
            requirements.memory_requirements.memory_type_bits & fd_properties.memory_type_bits;
        let type_index = self.pick_memory_type(allowed).ok_or_else(|| {
            PipelineError::GpuImport(
                "no memory type is shared by this DMA-BUF and the image".into(),
            )
        })?;

        let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
        let mut import_info = vk::ImportMemoryFdInfoKHR::default()
            .handle_type(DMA_BUF)
            .fd(raw_fd);
        let allocate_info = vk::MemoryAllocateInfo::default()
            .push_next(&mut dedicated)
            .push_next(&mut import_info)
            .allocation_size(requirements.memory_requirements.size)
            .memory_type_index(type_index);

        // SAFETY: the chained structures live until the call returns.
        let memory = unsafe { self.device.allocate_memory(&allocate_info, None) }
            .map_err(|e| PipelineError::GpuImport(format!("vkAllocateMemory (import): {e}")))?;

        // Vulkan took ownership of the descriptor on success, so it must not be closed
        // here. On failure the `?` above drops `fd` and closes it, which is correct.
        let _ = fd.into_raw_fd();
        Ok(memory)
    }

    /// Prefer device-local memory among the types allowed, else the first allowed one.
    fn pick_memory_type(&self, allowed: u32) -> Option<u32> {
        let types = &self.memory_properties.memory_types
            [..self.memory_properties.memory_type_count as usize];
        let device_local = types.iter().enumerate().find(|(i, t)| {
            allowed & (1 << i) != 0
                && t.property_flags
                    .contains(vk::MemoryPropertyFlags::DEVICE_LOCAL)
        });
        device_local
            .or_else(|| {
                types
                    .iter()
                    .enumerate()
                    .find(|(i, _)| allowed & (1 << i) != 0)
            })
            .and_then(|(i, _)| u32::try_from(i).ok())
    }
}

/// A freshly imported image and the guard that will free it.
struct Imported {
    image: vk::Image,
    guard: ImageGuard,
}

/// Destroys an imported image and its memory once `wgpu` is finished with the texture.
///
/// Handed to `texture_from_raw` as the drop guard, so the teardown is ordered by wgpu's
/// own resource lifetime tracking rather than by us guessing when the GPU is done.
struct ImageGuard {
    device: ash::Device,
    image: vk::Image,
    memory: Vec<vk::DeviceMemory>,
    /// The decoder's surface, released only once wgpu has retired every submission that
    /// referenced this image — which is exactly when it is safe for libavcodec to hand
    /// the underlying VA surface to the next picture.
    _surface: std::sync::Arc<dyn GpuSurface>,
}

impl Drop for ImageGuard {
    fn drop(&mut self) {
        // SAFETY: wgpu drops this guard only after the texture is retired, so no
        // submission still references the image. Each handle is destroyed once.
        unsafe {
            self.device.destroy_image(self.image, None);
            for memory in self.memory.drain(..) {
                self.device.free_memory(memory, None);
            }
        }
    }
}

/// Duplicate a borrowed descriptor so Vulkan can take ownership of the copy.
fn dup_fd(fd: RawFd) -> Result<OwnedFd, PipelineError> {
    // SAFETY: `fd` is owned by the surface's `AVFrame`, which outlives this call, so it
    // is live for the duration of `dup`.
    let duplicated = unsafe { libc_dup(fd) };
    if duplicated < 0 {
        return Err(PipelineError::GpuImport(format!(
            "dup({fd}) failed: {}",
            std::io::Error::last_os_error()
        )));
    }
    // SAFETY: `dup` returned a fresh descriptor nothing else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(duplicated) })
}

extern "C" {
    /// `dup(2)`. Declared here rather than pulling in the `libc` crate for one symbol.
    #[link_name = "dup"]
    fn libc_dup(fd: RawFd) -> RawFd;
}

/// Open a Vulkan logical device with the interop extensions on top of wgpu's own list.
///
/// Mirrors `wgpu_hal::vulkan::Adapter::open` — deliberately, because any divergence in
/// queue family or feature setup would produce a device wgpu-core thinks it configured
/// and did not.
fn open_hal_device(
    hal_adapter: &wgpu::hal::vulkan::Adapter,
    features: wgpu::Features,
    memory_hints: &wgpu::MemoryHints,
) -> Result<wgpu::hal::OpenDevice<wgpu::hal::api::Vulkan>, PipelineError> {
    let instance = hal_adapter.shared_instance().raw_instance();
    let physical = hal_adapter.raw_physical_device();

    // SAFETY: `physical` belongs to `instance`; the query only reads.
    let available = unsafe { instance.enumerate_device_extension_properties(physical) }
        .map_err(|e| PipelineError::GpuInit(format!("enumerate device extensions: {e}")))?;
    let supported = |name: &std::ffi::CStr| {
        available.iter().any(|properties| {
            // SAFETY: `extension_name` is a NUL-terminated fixed array written by the
            // driver.
            let found = unsafe { std::ffi::CStr::from_ptr(properties.extension_name.as_ptr()) };
            found == name
        })
    };

    let mut extensions = hal_adapter.required_device_extensions(features);
    for name in interop_extensions() {
        if !supported(name) {
            return Err(PipelineError::GpuInit(format!(
                "driver lacks {}, which DMA-BUF import needs",
                name.to_string_lossy()
            )));
        }
        if !extensions.contains(&name) {
            extensions.push(name);
        }
    }

    let mut phd_features = hal_adapter.physical_device_features(&extensions, features);

    // Queue family 0 with a single queue, matching `Adapter::open` exactly — the index is
    // passed back to `device_from_raw` below and must agree with what was created.
    let family_index = 0;
    let queue_infos = [vk::DeviceQueueCreateInfo::default()
        .queue_family_index(family_index)
        .queue_priorities(&[1.0])];
    let names: Vec<*const std::ffi::c_char> = extensions.iter().map(|s| s.as_ptr()).collect();
    let pre_info = vk::DeviceCreateInfo::default()
        .queue_create_infos(&queue_infos)
        .enabled_extension_names(&names);
    let info = phd_features.add_to_device_create(pre_info);

    // SAFETY: `info` and everything it chains outlive the call; `physical` belongs to
    // `instance`.
    let raw_device = unsafe { instance.create_device(physical, &info, None) }
        .map_err(|e| PipelineError::GpuInit(format!("vkCreateDevice (interop): {e}")))?;

    // SAFETY: `raw_device` was just created from this adapter with exactly `extensions`
    // and `phd_features`, which is what `device_from_raw` requires. `true` transfers
    // ownership of the handle to wgpu-hal.
    unsafe {
        hal_adapter.device_from_raw(
            raw_device,
            true,
            &extensions,
            features,
            memory_hints,
            family_index,
            0,
        )
    }
    .map_err(|e| PipelineError::GpuInit(format!("device_from_raw: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dup_gives_an_independent_descriptor() {
        // The import path hands its duplicate to `vkAllocateMemory`, which takes
        // ownership; if `dup` were ever elided the decoder's own fd would be closed out
        // from under the AVFrame and the *next* frame would fail, not this one.
        let file = std::fs::File::open("/dev/null").expect("open /dev/null");
        let original = file.as_raw_fd();
        let copy = dup_fd(original).expect("dup should succeed");
        assert_ne!(copy.as_raw_fd(), original);
        drop(copy);
        // The original is still usable after the duplicate is closed.
        assert!(dup_fd(original).is_ok());
    }

    #[test]
    fn dup_of_a_closed_descriptor_is_an_error_not_a_panic() {
        // A malformed descriptor from a driver must degrade to a give-up, not a crash.
        assert!(dup_fd(-1).is_err());
    }

    #[test]
    fn the_interop_extension_list_is_the_one_the_spec_requires() {
        // `image_format_list` is easy to drop as "optional"; it is not, once
        // MUTABLE_FORMAT is set, and omitting it makes image creation fail on validation
        // layers only — i.e. it works in dev and breaks nowhere useful.
        let names: Vec<_> = interop_extensions()
            .iter()
            .map(|c| c.to_string_lossy().into_owned())
            .collect();
        assert!(names.contains(&"VK_KHR_external_memory_fd".to_string()));
        assert!(names.contains(&"VK_EXT_external_memory_dma_buf".to_string()));
        assert!(names.contains(&"VK_EXT_image_drm_format_modifier".to_string()));
        assert!(names.contains(&"VK_KHR_image_format_list".to_string()));
    }
}
