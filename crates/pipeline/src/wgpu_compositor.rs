//! A real [`Compositor`] backed by `wgpu` (Vulkan on Linux, DX12 on Windows). It owns
//! the GPU device/queue and draws each [`Layer`] as a textured quad placed by its
//! [`Transform`], back-to-front by `z`, with per-layer opacity and alpha blending —
//! "PiP is just a layer with a scale+translate transform" (architecture §4).
//!
//! Two targets: an **offscreen** texture (headless — used by the tests to read pixels
//! back and prove the GPU actually drew) and a **surface** (the winit kiosk window).
//! Frame pixels arrive via [`WgpuCompositor::upload_texture`]; the decoder/pipeline
//! calls it on the render thread.

use std::collections::HashMap;

use bytemuck::{Pod, Zeroable};
use castaway_core::GpuSurface;
use wgpu::util::DeviceExt as _;

use crate::color::YuvMatrix;
use crate::compositor::{Compositor, DirtyRect, Layer, LayerId, Transform};
use crate::error::PipelineError;
use crate::hwaccel::{GpuImporter, SurfaceImport};

/// Build the wgpu instance restricted to the backend this platform is meant to run on.
///
/// Windows gets **DX12 only**, not `Backends::all()`. The deploy target's whole render
/// path is D3D12 — and the interop that lands on top of it (CEF's shared textures, the
/// Miracast encode path) is DXGI-specific — so a silent fallback to Vulkan or GL would
/// paper over a broken driver install and then fail much later, somewhere less obvious.
/// Failing at adapter selection is the diagnosable outcome.
///
/// Linux keeps `Backends::all()`: Vulkan is what we actually use, but the GL fallback is
/// load-bearing for headless/software CI runs. `WGPU_BACKEND` overrides either way, which
/// is the escape hatch for bisecting a backend-specific bug on the panel.
pub(crate) fn create_instance() -> wgpu::Instance {
    #[cfg(windows)]
    let backends = wgpu::Backends::DX12;
    #[cfg(not(windows))]
    let backends = wgpu::Backends::all();

    wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::util::backend_bits_from_env().unwrap_or(backends),
        ..Default::default()
    })
}

const SHADER: &str = r#"
// {vec4, f32} lays out as 32 bytes (struct align 16), matching the Rust `Uniform`
// which pads to 32. Do NOT add a vec3 pad here — that would round the size up to 48.
struct Uniform { transform: vec4<f32>, opacity: f32 };
@group(0) @binding(0) var tex: texture_2d<f32>;
@group(0) @binding(1) var smp: sampler;
@group(0) @binding(2) var<uniform> u: Uniform;

struct VsOut { @builtin(position) pos: vec4<f32>, @location(0) uv: vec2<f32> };

@vertex
fn vs_main(@builtin(vertex_index) vi: u32) -> VsOut {
    var quad = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 0.0), vec2<f32>(1.0, 0.0), vec2<f32>(0.0, 1.0),
        vec2<f32>(0.0, 1.0), vec2<f32>(1.0, 0.0), vec2<f32>(1.0, 1.0),
    );
    let uv = quad[vi];
    let sx = u.transform.x; let sy = u.transform.y;
    let ox = u.transform.z; let oy = u.transform.w;
    // Map the layer's [0,1] surface-space rect into clip space (flip Y).
    let x = (ox + uv.x * sx) * 2.0 - 1.0;
    let y = 1.0 - (oy + uv.y * sy) * 2.0;
    var out: VsOut;
    out.pos = vec4<f32>(x, y, 0.0, 1.0);
    out.uv = uv;
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let c = textureSample(tex, smp, in.uv);
    return vec4<f32>(c.rgb, c.a * u.opacity);
}
"#;

/// The hardware-decode path's shader. Same quad and same transform as [`SHADER`], but the
/// texture is a two-plane NV12 surface the decoder wrote and we never copied, so the
/// YUV→RGB conversion swscale used to do on the CPU happens here instead — three dot
/// products against a matrix [`YuvMatrix`] derived from the surface's own colorimetry.
const SHADER_NV12: &str = r#"
// {vec4 ×5} = 80 bytes, align 16. `offset.xyz` is the YUV zero point; `offset.w` carries
// opacity so the struct stays five clean vec4s.
struct Uniform {
    transform: vec4<f32>,
    m0: vec4<f32>,
    m1: vec4<f32>,
    m2: vec4<f32>,
    offset: vec4<f32>,
};
@group(0) @binding(0) var luma: texture_2d<f32>;
@group(0) @binding(1) var chroma: texture_2d<f32>;
@group(0) @binding(2) var smp: sampler;
@group(0) @binding(3) var<uniform> u: Uniform;

struct VsOut { @builtin(position) pos: vec4<f32>, @location(0) uv: vec2<f32> };

@vertex
fn vs_main(@builtin(vertex_index) vi: u32) -> VsOut {
    var quad = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 0.0), vec2<f32>(1.0, 0.0), vec2<f32>(0.0, 1.0),
        vec2<f32>(0.0, 1.0), vec2<f32>(1.0, 0.0), vec2<f32>(1.0, 1.0),
    );
    let uv = quad[vi];
    let sx = u.transform.x; let sy = u.transform.y;
    let ox = u.transform.z; let oy = u.transform.w;
    let x = (ox + uv.x * sx) * 2.0 - 1.0;
    let y = 1.0 - (oy + uv.y * sy) * 2.0;
    var out: VsOut;
    out.pos = vec4<f32>(x, y, 0.0, 1.0);
    out.uv = uv;
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    // Plane 0 is full-resolution luma (R8); plane 1 is half-resolution interleaved
    // chroma (RG8). Both are sampled with the same normalized uv — the hardware handles
    // the 2× chroma upsample as part of filtering.
    let y = textureSample(luma, smp, in.uv).r;
    let cbcr = textureSample(chroma, smp, in.uv).rg;
    let s = vec3<f32>(y, cbcr.r, cbcr.g) - u.offset.xyz;
    let rgb = vec3<f32>(dot(u.m0.xyz, s), dot(u.m1.xyz, s), dot(u.m2.xyz, s));
    return vec4<f32>(clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0)), u.offset.w);
}
"#;

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Uniform {
    transform: [f32; 4],
    opacity: f32,
    _pad: [f32; 3],
}

impl Uniform {
    fn from_layer(t: Transform, opacity: f32) -> Self {
        Self {
            transform: [t.scale_x, t.scale_y, t.offset_x, t.offset_y],
            opacity,
            _pad: [0.0; 3],
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Nv12Uniform {
    transform: [f32; 4],
    m0: [f32; 4],
    m1: [f32; 4],
    m2: [f32; 4],
    /// `xyz` = YUV zero point, `w` = opacity.
    offset: [f32; 4],
}

impl Nv12Uniform {
    fn from_layer(t: Transform, opacity: f32, yuv: YuvMatrix) -> Self {
        let row = |r: [f32; 3]| [r[0], r[1], r[2], 0.0];
        Self {
            transform: [t.scale_x, t.scale_y, t.offset_x, t.offset_y],
            m0: row(yuv.matrix[0]),
            m1: row(yuv.matrix[1]),
            m2: row(yuv.matrix[2]),
            offset: [yuv.offset[0], yuv.offset[1], yuv.offset[2], opacity],
        }
    }
}

/// Pixel layout accepted by [`WgpuCompositor::upload_texture`]. BGRA sources (CEF
/// `on_paint`, some decoders) upload as native `Bgra8Unorm` — no CPU swizzle pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TexelFormat {
    /// Packed RGBA8, sample values taken as-is.
    ///
    /// For pixels that are already linear. Uploading *authored* colours — anything a
    /// human picked, or a font rasterised — through this is the sRGB double-encode: the
    /// sampler reads sRGB bytes as if they were linear and the sRGB swapchain encodes
    /// them a second time on the way out. `#0d1428` reaches the panel as `#404f6e`.
    Rgba8,
    /// Packed BGRA8, sample values taken as-is.
    Bgra8,
    /// Packed RGBA8 holding sRGB-encoded values.
    ///
    /// What every CPU-authored surface wants: the sampler decodes to linear, the
    /// compositor blends in linear, and the swapchain re-encodes — a round trip that
    /// preserves the colour that was chosen.
    Rgba8Srgb,
}

impl TexelFormat {
    fn to_wgpu(self) -> wgpu::TextureFormat {
        match self {
            Self::Rgba8 => wgpu::TextureFormat::Rgba8Unorm,
            Self::Bgra8 => wgpu::TextureFormat::Bgra8Unorm,
            Self::Rgba8Srgb => wgpu::TextureFormat::Rgba8UnormSrgb,
        }
    }
}

/// How a layer's pixels got onto the GPU, and therefore which pipeline draws it.
///
/// Modeled as an enum rather than a `format` field with a magic value because the two
/// cases differ in bind-group *shape*, not just in format: an imported surface binds two
/// plane views and a color matrix, an uploaded one binds a single packed texture. A
/// mismatch between the two must not be representable.
enum LayerTexture {
    /// A packed RGBA/BGRA texture we own and write into.
    Packed {
        texture: wgpu::Texture,
        format: TexelFormat,
    },
    /// A two-plane NV12 surface produced by a hardware decoder and imported in place.
    Nv12 {
        /// Held so the imported surface outlives the bind group referencing its views.
        _texture: wgpu::Texture,
        /// Held so the *decoder's* surface is not recycled while we sample it. A DMA-BUF
        /// keeps the memory alive but does nothing to stop libavcodec handing the same
        /// VA surface to the next picture, so this reference is what prevents tearing.
        _surface: std::sync::Arc<dyn GpuSurface>,
        yuv: YuvMatrix,
    },
}

struct LayerGpu {
    texture: LayerTexture,
    uniform: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
}

impl LayerGpu {
    /// The packed texture this layer draws from, if it is on the upload path. `None` for
    /// an imported surface — which is exactly why a re-upload cannot silently write into
    /// one.
    const fn packed(&self) -> Option<(&wgpu::Texture, TexelFormat)> {
        match &self.texture {
            LayerTexture::Packed { texture, format } => Some((texture, *format)),
            LayerTexture::Nv12 { .. } => None,
        }
    }

    /// Rewrite this layer's uniform for a new transform/opacity, in whichever layout its
    /// pipeline expects.
    fn write_uniform(&self, queue: &wgpu::Queue, transform: Transform, opacity: f32) {
        match &self.texture {
            LayerTexture::Packed { .. } => queue.write_buffer(
                &self.uniform,
                0,
                bytemuck::bytes_of(&Uniform::from_layer(transform, opacity)),
            ),
            LayerTexture::Nv12 { yuv, .. } => queue.write_buffer(
                &self.uniform,
                0,
                bytemuck::bytes_of(&Nv12Uniform::from_layer(transform, opacity, *yuv)),
            ),
        }
    }
}

struct LayerState {
    meta: Layer,
    gpu: Option<LayerGpu>,
}

enum Target {
    Offscreen {
        texture: wgpu::Texture,
        size: (u32, u32),
    },
    Surface {
        surface: wgpu::Surface<'static>,
        config: wgpu::SurfaceConfiguration,
    },
}

/// The two draw programs a layer can use, built once for the target's format.
struct Programs {
    packed: wgpu::RenderPipeline,
    packed_bgl: wgpu::BindGroupLayout,
    nv12: wgpu::RenderPipeline,
    nv12_bgl: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
}

/// The wgpu compositor.
pub struct WgpuCompositor {
    device: wgpu::Device,
    queue: wgpu::Queue,
    programs: Programs,
    target: Target,
    layers: HashMap<LayerId, LayerState>,
    /// Set when the device was opened with the external-memory extensions a hardware
    /// decoder's surfaces need. `None` once we have concluded it cannot import.
    importer: Option<GpuImporter>,
}

impl WgpuCompositor {
    /// Create an offscreen compositor rendering into a `width`×`height` texture. Used by
    /// the headless tests (and any capture path); no window/surface required.
    ///
    /// # Errors
    /// [`PipelineError::GpuInit`] if no adapter/device is available.
    pub fn new_offscreen(width: u32, height: u32) -> Result<Self, PipelineError> {
        let instance = create_instance();
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: None,
            force_fallback_adapter: false,
        }))
        .ok_or_else(|| PipelineError::GpuInit("no GPU adapter".into()))?;
        let (device, queue, importer) = open_device(&adapter)?;

        let format = wgpu::TextureFormat::Rgba8Unorm;
        let programs = build_programs(&device, format);
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("offscreen-target"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });

        Ok(Self {
            device,
            queue,
            programs,
            target: Target::Offscreen {
                texture,
                size: (width, height),
            },
            layers: HashMap::new(),
            importer,
        })
    }

    /// Create a compositor rendering into a window surface (the kiosk output). The
    /// caller owns `instance` and the `surface` created from the window.
    ///
    /// # Errors
    /// [`PipelineError`] if no compatible adapter/device is found or config fails.
    pub fn new_for_surface(
        instance: wgpu::Instance,
        surface: wgpu::Surface<'static>,
        width: u32,
        height: u32,
    ) -> Result<Self, PipelineError> {
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
        }))
        .ok_or_else(|| PipelineError::GpuInit("no GPU adapter for surface".into()))?;
        let (device, queue, importer) = open_device(&adapter)?;

        let caps = surface.get_capabilities(&adapter);
        let format = caps
            .formats
            .iter()
            .copied()
            .find(wgpu::TextureFormat::is_srgb)
            .unwrap_or(caps.formats[0]);
        // Prefer Mailbox (latest-frame, no tearing, and immune to the Wayland frame-
        // callback stalls that make Fifo's acquire time out on some compositors);
        // Fifo is the spec-guaranteed fallback.
        let present_mode = if caps.present_modes.contains(&wgpu::PresentMode::Mailbox) {
            wgpu::PresentMode::Mailbox
        } else {
            wgpu::PresentMode::Fifo
        };
        let config = wgpu::SurfaceConfiguration {
            // COPY_SRC so the composited surface can be read back: screenshots and the
            // stream tee both need it, and it cannot be added after configuration
            // (Q30). Costs nothing when nothing reads.
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            format,
            width: width.max(1),
            height: height.max(1),
            present_mode,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &config);

        let programs = build_programs(&device, format);
        Ok(Self {
            device,
            queue,
            programs,
            target: Target::Surface { surface, config },
            layers: HashMap::new(),
            importer,
        })
    }

    /// Whether this compositor's device can import a hardware decoder's surfaces
    /// zero-copy. The decode side asks *before* choosing a path: decoding to GPU surfaces
    /// that nothing can sample is strictly worse than decoding on the CPU.
    #[must_use]
    pub fn surface_import(&self) -> SurfaceImport {
        self.importer
            .as_ref()
            .map_or(SurfaceImport::Unsupported, GpuImporter::capability)
    }

    /// Resize the surface target (no-op for offscreen).
    /// The current target size in device pixels. Layers place themselves in normalized
    /// surface coords, so anything that wants to rasterize at native scale (the OSD) has
    /// to ask what that scale currently is.
    #[must_use]
    pub fn target_size(&self) -> (u32, u32) {
        match &self.target {
            Target::Offscreen { size, .. } => *size,
            Target::Surface { config, .. } => (config.width, config.height),
        }
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        if let Target::Surface { surface, config } = &mut self.target {
            config.width = width.max(1);
            config.height = height.max(1);
            surface.configure(&self.device, config);
        }
    }

    /// Upload packed pixels for a layer's texture (a decoded frame or OSD/browser
    /// paint). Creates the layer's GPU resources on first upload; a same-size,
    /// same-format re-upload writes into the existing texture, so the per-frame
    /// video/browser paths never rebuild texture + bind group.
    ///
    /// # Errors
    /// [`PipelineError::InvalidFrame`] if the buffer is smaller than `width*height*4`.
    pub fn upload_texture(
        &mut self,
        id: LayerId,
        width: u32,
        height: u32,
        format: TexelFormat,
        pixels: &[u8],
    ) -> Result<(), PipelineError> {
        let need = (width as usize) * (height as usize) * 4;
        if width == 0 || height == 0 || pixels.len() < need {
            return Err(PipelineError::InvalidFrame("pixel buffer too small"));
        }

        if let Some((texture, existing)) = self
            .layers
            .get(&id)
            .and_then(|s| s.gpu.as_ref())
            .and_then(LayerGpu::packed)
        {
            if existing == format && texture.width() == width && texture.height() == height {
                write_pixels_region(
                    &self.queue,
                    texture,
                    width,
                    &pixels[..need],
                    DirtyRect::full(width, height),
                );
                return Ok(());
            }
        }

        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("layer-texture"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: format.to_wgpu(),
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        write_pixels_region(
            &self.queue,
            &texture,
            width,
            &pixels[..need],
            DirtyRect::full(width, height),
        );
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());

        let meta = self.layers.get(&id).map_or(
            Layer {
                id,
                z: default_z(id),
                opacity: 1.0,
                transform: Transform::default(),
            },
            |s| s.meta.clone(),
        );
        let uniform = self
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("layer-uniform"),
                contents: bytemuck::bytes_of(&Uniform::from_layer(meta.transform, meta.opacity)),
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            });
        let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("layer-bg"),
            layout: &self.programs.packed_bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.programs.sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: uniform.as_entire_binding(),
                },
            ],
        });

        self.layers.insert(
            id,
            LayerState {
                meta,
                gpu: Some(LayerGpu {
                    texture: LayerTexture::Packed { texture, format },
                    uniform,
                    bind_group,
                }),
            },
        );
        Ok(())
    }

    /// Bind a hardware decoder's surface as a layer, **without copying it**.
    ///
    /// This is the payoff for the whole hwaccel exercise: the decoder wrote NV12 into
    /// video memory, and the same allocation is handed to the sampler. The alternative —
    /// `av_hwframe_transfer_data` back to system memory so [`Self::upload_texture`] can
    /// take it — costs a GPU→CPU→GPU round trip that, at 4K, is usually more expensive
    /// than the CPU decode it was meant to replace.
    ///
    /// A fresh `wgpu::Texture` (and therefore a fresh bind group) is built per frame:
    /// the decoder cycles a pool of surfaces and gives no stable identity to cache on.
    /// The import is a handful of driver objects, not a copy, so it costs microseconds.
    ///
    /// # Errors
    /// [`PipelineError::GpuImport`] if this device cannot import external surfaces or the
    /// surface is not one this platform's importer understands. The caller's answer to
    /// that is to fall back to software decode, not to fail the session.
    pub fn import_surface(
        &mut self,
        id: LayerId,
        width: u32,
        height: u32,
        surface: &std::sync::Arc<dyn GpuSurface>,
    ) -> Result<(), PipelineError> {
        let importer = self
            .importer
            .as_mut()
            .ok_or_else(|| PipelineError::GpuImport("device cannot import GPU surfaces".into()))?;
        let texture = importer.import(&self.device, surface)?;
        if texture.width() != width || texture.height() != height {
            // The frame's declared size and the surface's real size disagreeing means a
            // resize was mishandled somewhere upstream; drawing it would sample garbage
            // outside the picture.
            return Err(PipelineError::GpuImport(format!(
                "surface is {}×{} but the frame claims {width}×{height}",
                texture.width(),
                texture.height(),
            )));
        }

        // NV12 is sampled through per-plane views: plane 0 is full-res R8 luma, plane 1
        // is half-res RG8 chroma. wgpu exposes exactly this via `TextureAspect`, so the
        // only part that needed raw hal was getting the surface in.
        let luma = texture.create_view(&wgpu::TextureViewDescriptor {
            label: Some("nv12-luma"),
            format: Some(wgpu::TextureFormat::R8Unorm),
            aspect: wgpu::TextureAspect::Plane0,
            ..Default::default()
        });
        let chroma = texture.create_view(&wgpu::TextureViewDescriptor {
            label: Some("nv12-chroma"),
            format: Some(wgpu::TextureFormat::Rg8Unorm),
            aspect: wgpu::TextureAspect::Plane1,
            ..Default::default()
        });

        let meta = self.layers.get(&id).map_or(
            Layer {
                id,
                z: default_z(id),
                opacity: 1.0,
                transform: Transform::default(),
            },
            |s| s.meta.clone(),
        );
        let yuv = YuvMatrix::new(surface.color());
        // `width`/`height` were consumed by the size check above; the texture carries the
        // real extent from here on.
        let uniform = self
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("layer-uniform-nv12"),
                contents: bytemuck::bytes_of(&Nv12Uniform::from_layer(
                    meta.transform,
                    meta.opacity,
                    yuv,
                )),
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            });
        let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("layer-bg-nv12"),
            layout: &self.programs.nv12_bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&luma),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&chroma),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(&self.programs.sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: uniform.as_entire_binding(),
                },
            ],
        });

        self.layers.insert(
            id,
            LayerState {
                meta,
                gpu: Some(LayerGpu {
                    texture: LayerTexture::Nv12 {
                        _texture: texture,
                        _surface: std::sync::Arc::clone(surface),
                        yuv,
                    },
                    uniform,
                    bind_group,
                }),
            },
        );
        Ok(())
    }

    /// Write only `rects` regions of the full `width`×`height` frame in `pixels` into
    /// the layer's existing texture — the partial-update path for CEF dirty rects.
    /// Falls back to a full [`Self::upload_texture`] when the layer has no texture yet
    /// or its size/format changed (the caller always passes the complete frame, so the
    /// fallback needs nothing extra).
    ///
    /// # Errors
    /// [`PipelineError::InvalidFrame`] if the buffer is smaller than `width*height*4`.
    pub fn upload_texture_regions(
        &mut self,
        id: LayerId,
        width: u32,
        height: u32,
        format: TexelFormat,
        pixels: &[u8],
        rects: &[DirtyRect],
    ) -> Result<(), PipelineError> {
        let need = (width as usize) * (height as usize) * 4;
        if width == 0 || height == 0 || pixels.len() < need {
            return Err(PipelineError::InvalidFrame("pixel buffer too small"));
        }
        if let Some((texture, existing)) = self
            .layers
            .get(&id)
            .and_then(|s| s.gpu.as_ref())
            .and_then(LayerGpu::packed)
        {
            if existing == format && texture.width() == width && texture.height() == height {
                for rect in rects {
                    if let Some(r) = rect.clamped(width, height) {
                        write_pixels_region(&self.queue, texture, width, &pixels[..need], r);
                    }
                }
                return Ok(());
            }
        }
        self.upload_texture(id, width, height, format, pixels)
    }

    /// Read back the offscreen target as RGBA8 (`width*height*4` bytes). Offscreen only.
    ///
    /// # Errors
    /// [`PipelineError`] if called on a surface target or the map fails.
    /// Present, optionally reading the frame back as RGBA8.
    ///
    /// The capture happens *inside* the frame because a surface texture only exists
    /// between acquire and present — there is nothing to copy from afterwards. Returns
    /// `None` when not asked, or when the readback failed, which must not stop the panel
    /// from presenting.
    pub fn present_and_capture(&mut self, capture: bool) -> Option<Vec<u8>> {
        if !capture {
            self.present();
            return None;
        }
        match &self.target {
            Target::Offscreen { texture, size } => {
                let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
                self.render_into(&view);
                let (w, h) = *size;
                self.copy_back(texture, w, h, wgpu::TextureFormat::Rgba8Unorm)
                    .ok()
            }
            Target::Surface { surface, config } => {
                let frame = surface.get_current_texture().ok()?;
                let view = frame
                    .texture
                    .create_view(&wgpu::TextureViewDescriptor::default());
                self.render_into(&view);
                let (w, h) = (config.width, config.height);
                let out = self.copy_back(&frame.texture, w, h, config.format).ok();
                frame.present();
                out
            }
        }
    }

    pub fn read_rgba(&self) -> Result<Vec<u8>, PipelineError> {
        let (texture, (width, height)) = match &self.target {
            Target::Offscreen { texture, size } => (texture, *size),
            // A surface texture only exists between `get_current_texture` and `present`,
            // so there is nothing to copy from here. `capture_into` does the readback
            // inside the frame instead, while the texture is alive.
            Target::Surface { .. } => {
                return Err(PipelineError::Surface(
                    "read_rgba needs an offscreen target; use capture_into during a frame".into(),
                ))
            }
        };
        self.copy_back(texture, width, height, wgpu::TextureFormat::Rgba8Unorm)
    }

    /// Copy a texture back to tightly-packed RGBA8.
    ///
    /// Swizzles when the surface is BGRA, which it usually is: a swapchain picks whatever
    /// the platform prefers, and a PNG of a BGRA buffer read as RGBA looks plausible and
    /// has the red and blue channels swapped — the kind of wrong that survives review.
    fn copy_back(
        &self,
        texture: &wgpu::Texture,
        width: u32,
        height: u32,
        format: wgpu::TextureFormat,
    ) -> Result<Vec<u8>, PipelineError> {
        let unpadded = (width * 4) as usize;
        let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT as usize;
        let padded = unpadded.div_ceil(align) * align;
        let padded_u32 = u32::try_from(padded)
            .map_err(|_| PipelineError::Surface("row stride too large".into()))?;
        let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("readback"),
            size: (padded * height as usize) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        encoder.copy_texture_to_buffer(
            wgpu::ImageCopyTexture {
                texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::ImageCopyBuffer {
                buffer: &buffer,
                layout: wgpu::ImageDataLayout {
                    offset: 0,
                    bytes_per_row: Some(padded_u32),
                    rows_per_image: Some(height),
                },
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
        self.queue.submit(Some(encoder.finish()));

        let slice = buffer.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        self.device.poll(wgpu::Maintain::Wait);
        rx.recv()
            .map_err(|_| PipelineError::Surface("map channel closed".into()))?
            .map_err(|e| PipelineError::Surface(format!("{e:?}")))?;

        let data = slice.get_mapped_range();
        let mut out = Vec::with_capacity(unpadded * height as usize);
        for row in 0..height as usize {
            let start = row * padded;
            out.extend_from_slice(&data[start..start + unpadded]);
        }
        drop(data);
        buffer.unmap();

        // A swapchain picks whatever the platform prefers, which on most desktops is
        // BGRA. Read as RGBA that produces a picture that looks entirely plausible with
        // red and blue swapped — wrong in a way that survives a glance.
        if matches!(
            format,
            wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb
        ) {
            for px in out.chunks_exact_mut(4) {
                px.swap(0, 2);
            }
        }
        Ok(out)
    }

    fn render_into(&self, view: &wgpu::TextureView) {
        let mut ordered: Vec<&LayerState> = self.layers.values().collect();
        ordered.sort_by_key(|s| s.meta.z);

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("frame"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("composite"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            for state in ordered {
                if let Some(gpu) = &state.gpu {
                    // Each layer picks its program: uploaded layers sample one packed
                    // texture, imported ones sample two planes and convert.
                    pass.set_pipeline(match gpu.texture {
                        LayerTexture::Packed { .. } => &self.programs.packed,
                        LayerTexture::Nv12 { .. } => &self.programs.nv12,
                    });
                    pass.set_bind_group(0, &gpu.bind_group, &[]);
                    pass.draw(0..6, 0..1);
                }
            }
        }
        self.queue.submit(Some(encoder.finish()));
    }
}

impl Compositor for WgpuCompositor {
    fn upsert_layer(&mut self, layer: Layer) {
        let entry = self.layers.entry(layer.id).or_insert_with(|| LayerState {
            meta: layer.clone(),
            gpu: None,
        });
        entry.meta = layer.clone();
        // If the layer already has a texture, push the new transform/opacity to its uniform.
        if let Some(gpu) = &entry.gpu {
            gpu.write_uniform(&self.queue, layer.transform, layer.opacity);
        }
    }

    fn remove_layer(&mut self, id: LayerId) {
        self.layers.remove(&id);
    }

    fn present(&mut self) {
        match &self.target {
            Target::Offscreen { texture, .. } => {
                let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
                self.render_into(&view);
            }
            Target::Surface { surface, config } => {
                let frame = match surface.get_current_texture() {
                    Ok(frame) => Some(frame),
                    Err(e) => {
                        // Outdated/Lost after compositor-side changes (scale, mode) and
                        // Timeout under Wayland frame-callback stalls all recover the
                        // same way: reconfigure the swapchain and try once more.
                        tracing::warn!(error = ?e, "surface frame acquire failed; reconfiguring");
                        surface.configure(&self.device, config);
                        surface.get_current_texture().ok()
                    }
                };
                if let Some(frame) = frame {
                    let view = frame
                        .texture
                        .create_view(&wgpu::TextureViewDescriptor::default());
                    self.render_into(&view);
                    frame.present();
                }
            }
        }
    }
}

/// Copy one already-clamped sub-rect of a tightly-packed `full_width`-stride frame into
/// `texture` via the queue's staging path. `DirtyRect::full` writes the whole frame.
fn write_pixels_region(
    queue: &wgpu::Queue,
    texture: &wgpu::Texture,
    full_width: u32,
    px: &[u8],
    rect: DirtyRect,
) {
    let offset = ((rect.y as usize) * (full_width as usize) + rect.x as usize) * 4;
    queue.write_texture(
        wgpu::ImageCopyTexture {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d {
                x: rect.x,
                y: rect.y,
                z: 0,
            },
            aspect: wgpu::TextureAspect::All,
        },
        &px[offset..],
        wgpu::ImageDataLayout {
            offset: 0,
            bytes_per_row: Some(4 * full_width),
            rows_per_image: None,
        },
        wgpu::Extent3d {
            width: rect.width,
            height: rect.height,
            depth_or_array_layers: 1,
        },
    );
}

/// The limits every device we open gets: the downlevel baseline, raised to the adapter's
/// real max texture size. The baseline caps 2D textures at 2048, which can't even
/// configure a 4K surface (the Dell panel is 3840×2160).
fn compositor_limits(adapter: &wgpu::Adapter) -> wgpu::Limits {
    wgpu::Limits::downlevel_defaults().using_resolution(adapter.limits())
}

/// Open the render device, preferring one that can import a hardware decoder's surfaces.
///
/// Two attempts, in order, and the *downgrade is logged* — a compositor that quietly
/// opened a plain device is a compositor that will quietly decode on the CPU forever,
/// which looks like nothing at all until someone notices the fan.
fn open_device(
    adapter: &wgpu::Adapter,
) -> Result<(wgpu::Device, wgpu::Queue, Option<GpuImporter>), PipelineError> {
    match GpuImporter::open_device(adapter, compositor_limits(adapter)) {
        Ok(Some((device, queue, importer))) => {
            tracing::info!(
                backend = %importer.capability(),
                "compositor device opened with GPU surface import",
            );
            return Ok((device, queue, Some(importer)));
        }
        Ok(None) => {
            // No hwaccel backend compiled for this platform — expected, not a downgrade.
        }
        Err(e) => tracing::warn!(
            error = %e,
            "no interop-capable GPU device; falling back to a plain device \
             (hardware decode will not be used)",
        ),
    }

    let (device, queue) = pollster::block_on(adapter.request_device(
        &wgpu::DeviceDescriptor {
            label: Some("castaway-compositor"),
            required_features: wgpu::Features::empty(),
            required_limits: compositor_limits(adapter),
            memory_hints: wgpu::MemoryHints::Performance,
        },
        None,
    ))
    .map_err(|e| PipelineError::GpuInit(e.to_string()))?;
    Ok((device, queue, None))
}

/// One entry in a bind group layout, spelled out so the two layouts below stay readable.
const fn texture_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable: true },
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        },
        count: None,
    }
}

const fn sampler_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
        count: None,
    }
}

const fn uniform_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Uniform,
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    }
}

/// Build one textured-quad program from a shader source and a bind group layout.
fn build_program(
    device: &wgpu::Device,
    format: wgpu::TextureFormat,
    label: &str,
    source: &str,
    entries: &[wgpu::BindGroupLayoutEntry],
) -> (wgpu::RenderPipeline, wgpu::BindGroupLayout) {
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some(label),
        source: wgpu::ShaderSource::Wgsl(source.into()),
    });
    let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some(label),
        entries,
    });
    let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some(label),
        bind_group_layouts: &[&bind_group_layout],
        push_constant_ranges: &[],
    });
    let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some(label),
        layout: Some(&layout),
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: "vs_main",
            buffers: &[],
            compilation_options: wgpu::PipelineCompilationOptions::default(),
        },
        fragment: Some(wgpu::FragmentState {
            module: &shader,
            entry_point: "fs_main",
            targets: &[Some(wgpu::ColorTargetState {
                format,
                blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                write_mask: wgpu::ColorWrites::ALL,
            })],
            compilation_options: wgpu::PipelineCompilationOptions::default(),
        }),
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview: None,
        cache: None,
    });
    (pipeline, bind_group_layout)
}

fn build_programs(device: &wgpu::Device, format: wgpu::TextureFormat) -> Programs {
    let (packed, packed_bgl) = build_program(
        device,
        format,
        "composite-packed",
        SHADER,
        &[texture_entry(0), sampler_entry(1), uniform_entry(2)],
    );
    // The NV12 program only compiles usefully on a device that has the feature; building
    // it unconditionally is still fine — it is a shader and a layout, not a texture.
    let (nv12, nv12_bgl) = build_program(
        device,
        format,
        "composite-nv12",
        SHADER_NV12,
        &[
            texture_entry(0),
            texture_entry(1),
            sampler_entry(2),
            uniform_entry(3),
        ],
    );
    let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
        label: Some("layer-sampler"),
        mag_filter: wgpu::FilterMode::Linear,
        min_filter: wgpu::FilterMode::Linear,
        ..Default::default()
    });
    Programs {
        packed,
        packed_bgl,
        nv12,
        nv12_bgl,
        sampler,
    }
}

fn default_z(id: LayerId) -> i32 {
    match id {
        LayerId::Attract => -10,
        // Above the idle scene, below video: a sender with pixels of its own outranks a
        // card describing a sender without them.
        LayerId::NowPlaying => -5,
        LayerId::Video => 0,
        LayerId::Browser => 5,
        LayerId::Osd => 10,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// Skip GPU tests gracefully if no adapter is available in this environment.
    macro_rules! compositor_or_skip {
        ($w:expr, $h:expr) => {
            match WgpuCompositor::new_offscreen($w, $h) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("skipping GPU test: {e}");
                    return;
                }
            }
        };
    }

    fn solid(width: u32, height: u32, rgba: [u8; 4]) -> Vec<u8> {
        rgba.iter()
            .copied()
            .cycle()
            .take((width * height * 4) as usize)
            .collect()
    }

    #[test]
    fn full_screen_red_layer_fills_target() {
        let mut c = compositor_or_skip!(32, 32);
        c.upload_texture(
            LayerId::Video,
            4,
            4,
            TexelFormat::Rgba8,
            &solid(4, 4, [255, 0, 0, 255]),
        )
        .unwrap();
        c.upsert_layer(Layer {
            id: LayerId::Video,
            z: 0,
            opacity: 1.0,
            transform: Transform::default(),
        });
        c.present();
        let px = c.read_rgba().unwrap();
        // Center pixel should be red.
        let idx = ((16 * 32) + 16) * 4;
        assert_eq!(&px[idx..idx + 4], &[255, 0, 0, 255], "center should be red");
    }

    #[test]
    fn region_update_writes_only_the_rect() {
        // 4×4 target with a 4×4 texture → 1:1 texel-to-pixel mapping, exact readback.
        let mut c = compositor_or_skip!(4, 4);
        c.upload_texture(
            LayerId::Video,
            4,
            4,
            TexelFormat::Rgba8,
            &solid(4, 4, [255, 0, 0, 255]),
        )
        .unwrap();
        c.upsert_layer(Layer {
            id: LayerId::Video,
            z: 0,
            opacity: 1.0,
            transform: Transform::default(),
        });
        // Green frame, but only the top-left 2×2 declared dirty.
        c.upload_texture_regions(
            LayerId::Video,
            4,
            4,
            TexelFormat::Rgba8,
            &solid(4, 4, [0, 255, 0, 255]),
            &[DirtyRect {
                x: 0,
                y: 0,
                width: 2,
                height: 2,
            }],
        )
        .unwrap();
        c.present();
        let px = c.read_rgba().unwrap();
        let at = |x: usize, y: usize| {
            let i = (y * 4 + x) * 4;
            [px[i], px[i + 1], px[i + 2]]
        };
        assert_eq!(at(0, 0), [0, 255, 0], "dirty rect took the new pixels");
        assert_eq!(at(3, 3), [255, 0, 0], "outside the rect is untouched");
    }

    #[test]
    fn pip_layer_covers_only_its_corner() {
        let mut c = compositor_or_skip!(64, 64);
        // Full-screen blue background.
        c.upload_texture(
            LayerId::Video,
            2,
            2,
            TexelFormat::Rgba8,
            &solid(2, 2, [0, 0, 255, 255]),
        )
        .unwrap();
        c.upsert_layer(Layer {
            id: LayerId::Video,
            z: 0,
            opacity: 1.0,
            transform: Transform::default(),
        });
        // Green PiP in the bottom-right corner (corner 3) — BGRA path: green survives
        // the swizzle-free upload because G is channel-order invariant.
        c.upload_texture(
            LayerId::Browser,
            2,
            2,
            TexelFormat::Bgra8,
            &solid(2, 2, [0, 255, 0, 255]),
        )
        .unwrap();
        c.upsert_layer(Layer {
            id: LayerId::Browser,
            z: 5,
            opacity: 1.0,
            transform: Transform::pip(3),
        });
        c.present();
        let px = c.read_rgba().unwrap();
        let at = |x: usize, y: usize| {
            let i = (y * 64 + x) * 4;
            [px[i], px[i + 1], px[i + 2]]
        };
        // Top-left is background blue; bottom-right holds the green PiP.
        assert_eq!(at(4, 4), [0, 0, 255], "corner away from PiP stays blue");
        assert_eq!(at(58, 58), [0, 255, 0], "PiP corner is green");
    }
}
